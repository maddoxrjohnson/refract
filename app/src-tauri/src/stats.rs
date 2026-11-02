//! Machine load for the system widget: CPU and memory from `sysinfo`, GPU from the
//! counters `gpu.rs` already reads, and Refract's own share of each. Sampled on the GPU
//! sampler's thread, so it too runs only while the window is visible.
//!
//! WSL's share (`.plans/system-split-spec.md`) comes from two places: the helper's
//! `systemStats`, which says what Linux uses, and the WSL VM's process (`vmmemWSL`),
//! whose working set is what that costs Windows.

use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use refract_client::{AgentStatus, RequestError};
use refract_core::protocol::{code, LinuxStats, Reply};
use serde::Serialize;

use crate::gpu::GpuUsage;

/// Emitted with [`SystemStats`] after each reading of the GPU counters.
pub const STATS_EVENT: &str = "system://stats";

/// Executables that count as Refract when started by it: WebView2, which draws the
/// interface, and `wsl.exe`, which carries the helper. Anything else the app starts
/// (VS Code, `cmd.exe`, `gh.exe`) is not Refract's own load, and neither are its children.
const OWN_IMAGES: &[&str] = &["msedgewebview2.exe", "wsl.exe"];

/// The WSL 2 VM's process, as the process list names it.
const WSL_VM_IMAGE: &str = "vmmemWSL";

/// How long a tick waits for the helper's `systemStats` answer: well inside the 2 s
/// between ticks. A later answer is dropped and that tick has no WSL figures.
#[cfg_attr(not(windows), allow(dead_code))]
const LINUX_TIMEOUT: Duration = Duration::from_millis(400);

/// How long the last good `systemStats` answer stands in when a tick has none: through
/// one late answer and the priming one after it, so the WSL figures don't blink out.
const LINUX_HOLD: Duration = Duration::from_secs(6);

/// Counts the helper's starts, so a helper too old for `systemStats` is asked again only
/// after a restart, which may be a newer one.
static HELPER_STARTS: AtomicU64 = AtomicU64::new(0);

/// The helper start that answered `systemStats` with "unknown method", or `u64::MAX`.
/// Kept across sampler threads, so that helper is asked once, not on every window show.
static UNSUPPORTED: AtomicU64 = AtomicU64::new(u64::MAX);

#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SystemStats {
    /// All CPUs, 0 to 100.
    pub cpu_percent: f64,
    pub mem_used_bytes: u64,
    pub mem_total_bytes: u64,
    /// Busiest GPU engine, as in [`GpuUsage::total_percent`].
    pub gpu_percent: f64,
    /// WSL's part of the figures above; `None` when the helper gave no reading.
    pub wsl: Option<WslShare>,
    pub own: OwnShare,
}

/// WSL's part of a reading, on Windows' scale: CPU of all Windows CPUs, memory as the VM's
/// working set.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WslShare {
    /// Of all CPUs, 0 to 100, and no more than the machine's total.
    pub cpu_percent: f64,
    /// The VM's working set.
    pub mem_bytes: u64,
    /// The part of `mem_bytes` Linux programs use, from [`split_working_set`].
    pub mem_program_bytes: u64,
    /// The part that is Linux file cache, which Linux can give back.
    pub mem_cache_bytes: u64,
    /// The VM's GPU use; `None` when the counters credit nothing to it.
    pub gpu_percent: Option<f64>,
}

/// What this process, its WebView2 processes and the helper's `wsl.exe` use, plus the
/// helper itself inside WSL.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OwnShare {
    /// Of all CPUs, 0 to 100: Windows and WSL together.
    pub cpu_percent: f64,
    /// Windows and WSL together.
    pub mem_bytes: u64,
    pub gpu_percent: f64,
    /// The helper and its children's part of `cpu_percent`, inside WSL's share.
    pub wsl_cpu_percent: f64,
    /// Their part of `mem_bytes`.
    pub wsl_mem_bytes: u64,
}

/// One process from a snapshot of the process list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessEntry {
    pub pid: u32,
    pub parent: u32,
    /// The executable's file name, such as `msedgewebview2.exe`.
    pub name: String,
}

/// The whole machine, as `sysinfo` reports it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Machine {
    /// All CPUs, 0 to 100.
    pub cpu_percent: f64,
    pub mem_used_bytes: u64,
    pub mem_total_bytes: u64,
    /// Logical CPUs; a process's CPU use is a share of one of them.
    pub cpu_count: usize,
}

/// One of Refract's processes.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ProcessLoad {
    /// Of one CPU, so above 100 when it keeps several busy.
    pub cpu_percent: f64,
    pub mem_bytes: u64,
}

/// What a reading knows about WSL.
#[derive(Debug, Clone, PartialEq)]
pub struct WslInput {
    /// The helper's `systemStats` answer.
    pub linux: LinuxStats,
    /// `vmmemWSL`'s working set; `None` when it could not be read, and Linux's
    /// `MemTotal - MemFree` stands in for it.
    pub working_set: Option<u64>,
    /// From [`crate::gpu::process_percent`] for `vmmemWSL`.
    pub gpu_percent: Option<f64>,
}

/// `root` and the processes descended from it through [`OWN_IMAGES`]: a child with
/// another name is left out along with everything under it. A parent id can be stale,
/// as in [`crate::gpu::descendants`].
pub fn own_processes(root: u32, processes: &[ProcessEntry]) -> HashSet<u32> {
    let mut found = HashSet::from([root]);
    let mut queue = vec![root];
    while let Some(parent) = queue.pop() {
        for process in processes {
            let ours = OWN_IMAGES
                .iter()
                .any(|image| process.name.eq_ignore_ascii_case(image));
            if process.parent == parent
                && process.pid != parent
                && ours
                && found.insert(process.pid)
            {
                queue.push(process.pid);
            }
        }
    }
    found
}

/// The WSL VM's process id, when WSL is running.
pub fn wsl_vm(processes: &[ProcessEntry]) -> Option<u32> {
    processes
        .iter()
        .find(|p| p.name.eq_ignore_ascii_case(WSL_VM_IMAGE))
        .map(|p| p.pid)
}

/// Splits the VM's working set `ws` into what Linux programs use and what is file cache,
/// from Linux's `programs` and `cache`. Part of the VM can be paged out on the Windows
/// side, so not all of `programs + cache` need be resident; programs are taken to be
/// resident first.
pub fn split_working_set(ws: u64, programs: u64, cache: u64) -> (u64, u64) {
    let programs = programs.min(ws);
    (programs, cache.min(ws - programs))
}

/// A percentage that is finite and within 0 to 100.
fn percent(value: f64) -> f64 {
    if value.is_finite() {
        value.clamp(0.0, 100.0)
    } else {
        0.0
    }
}

/// WSL's share on Windows' scale, given the machine's CPU and memory in use, and the
/// helper's own part of it. `None` on the helper's first reading, which has no CPU use.
fn wsl_share(
    input: &WslInput,
    cpu_percent: f64,
    mem_used_bytes: u64,
    windows_cpus: usize,
) -> Option<(WslShare, f64, u64)> {
    let linux = &input.linux;
    // Linux's shares are of its own CPUs, which `.wslconfig` can make fewer.
    let scale = f64::from(linux.cpu_count) / windows_cpus.max(1) as f64;
    let cpu = percent(linux.cpu_percent? * scale).min(cpu_percent);
    let own_cpu = percent(linux.own.cpu_percent.unwrap_or(0.0) * scale).min(cpu);
    let ws = input
        .working_set
        .unwrap_or(linux.mem_used_bytes.saturating_add(linux.mem_cache_bytes))
        .min(mem_used_bytes);
    let (programs, cache) = split_working_set(ws, linux.mem_used_bytes, linux.mem_cache_bytes);
    let share = WslShare {
        cpu_percent: cpu,
        mem_bytes: ws,
        mem_program_bytes: programs,
        mem_cache_bytes: cache,
        gpu_percent: input.gpu_percent.map(percent),
    };
    Some((share, own_cpu, linux.own.mem_bytes.min(ws)))
}

/// Puts one reading together. `gpu` is the GPU use with `own_percent` counted over the
/// same processes as `own`.
pub fn aggregate(
    machine: Machine,
    own: &[ProcessLoad],
    gpu: GpuUsage,
    wsl: Option<&WslInput>,
) -> SystemStats {
    let own_cpu: f64 = own
        .iter()
        .map(|p| p.cpu_percent)
        .filter(|v| v.is_finite() && *v > 0.0)
        .sum();
    let own_mem: u64 = own.iter().map(|p| p.mem_bytes).sum();
    let cpu_percent = percent(machine.cpu_percent);
    let mem_used_bytes = machine.mem_used_bytes.min(machine.mem_total_bytes);
    let wsl = wsl.and_then(|w| wsl_share(w, cpu_percent, mem_used_bytes, machine.cpu_count));
    let (wsl_cpu, wsl_mem) = wsl.map_or((0.0, 0), |(_, cpu, mem)| (cpu, mem));
    SystemStats {
        cpu_percent,
        mem_used_bytes,
        mem_total_bytes: machine.mem_total_bytes,
        gpu_percent: percent(gpu.total_percent),
        wsl: wsl.map(|(share, _, _)| share),
        own: OwnShare {
            cpu_percent: percent(own_cpu / machine.cpu_count.max(1) as f64 + wsl_cpu),
            mem_bytes: own_mem.saturating_add(wsl_mem).min(machine.mem_total_bytes),
            gpu_percent: percent(gpu.own_percent),
            wsl_cpu_percent: wsl_cpu,
            wsl_mem_bytes: wsl_mem,
        },
    }
}

/// Called on each change in the helper's status; see [`HELPER_STARTS`].
pub fn on_helper_status(status: &AgentStatus) {
    if *status == AgentStatus::Running {
        HELPER_STARTS.fetch_add(1, Ordering::SeqCst);
    }
}

/// Asks the helper for `systemStats` on each tick of one sampler thread.
#[derive(Debug, Default)]
#[cfg_attr(not(windows), allow(dead_code))]
pub struct LinuxSource {
    /// Whether the last tick had an answer. One that follows a tick without one is
    /// measured from some older call, perhaps from before the window was hidden, so it
    /// only primes the counters.
    primed: bool,
    /// The last answer used and when, held for up to [`LINUX_HOLD`].
    last: Option<(Instant, LinuxStats)>,
}

#[cfg_attr(not(windows), allow(dead_code))]
impl LinuxSource {
    /// This tick's answer, waiting at most [`LINUX_TIMEOUT`]. Without one to use (no
    /// helper, a late answer or a priming one), the last answer while it is under
    /// [`LINUX_HOLD`] old, else `None`; always `None` from a helper too old to answer.
    pub fn read(&mut self, app: &tauri::AppHandle) -> Option<LinuxStats> {
        use tauri::Manager;
        let start = HELPER_STARTS.load(Ordering::SeqCst);
        if UNSUPPORTED.load(Ordering::SeqCst) == start {
            return None;
        }
        let result = match app.try_state::<crate::agent::Agent>() {
            Some(agent) => {
                agent.request(refract_core::protocol::Method::SystemStats, LINUX_TIMEOUT)
            }
            None => Err(RequestError::NotRunning),
        };
        self.accept(result, start, Instant::now())
    }

    /// What [`LinuxSource::read`] does with the helper's answer at `now`, during helper
    /// start `start`.
    fn accept(
        &mut self,
        result: Result<Reply, RequestError>,
        start: u64,
        now: Instant,
    ) -> Option<LinuxStats> {
        let stats = match result {
            Ok(Reply::SystemStats(stats)) => Some(stats),
            Err(RequestError::Agent { code: c, .. }) if c == code::METHOD_NOT_FOUND => {
                UNSUPPORTED.store(start, Ordering::SeqCst);
                self.last = None;
                return None;
            }
            _ => None,
        };
        let primed = std::mem::replace(&mut self.primed, stats.is_some());
        // An answer without CPU use (a helper's first) is a priming one too.
        if let Some(stats) = stats.filter(|s| primed && s.cpu_percent.is_some()) {
            self.last = Some((now, stats.clone()));
            return Some(stats);
        }
        self.last
            .as_ref()
            .filter(|(at, _)| now.saturating_duration_since(*at) < LINUX_HOLD)
            .map(|(_, stats)| stats.clone())
    }
}

#[cfg(all(windows, test))]
use sampler::find_working_set;
#[cfg(windows)]
pub use sampler::Sampler;

#[cfg(windows)]
mod sampler {
    use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};
    use windows::Wdk::System::SystemInformation::{
        NtQuerySystemInformation, SystemProcessInformation,
    };
    use windows::Win32::Foundation::STATUS_INFO_LENGTH_MISMATCH;
    use windows::Win32::System::WindowsProgramming::SYSTEM_PROCESS_INFORMATION;

    /// The most room [`Sampler::working_set`] takes for the process list.
    const PROCESS_INFO_CAP: usize = 64 << 20;

    use refract_core::protocol::LinuxStats;

    use super::{
        aggregate, own_processes, wsl_vm, Machine, ProcessEntry, ProcessLoad, SystemStats, WslInput,
    };
    use crate::gpu;

    /// Keeps `sysinfo`'s previous reading, which CPU use is measured against.
    pub struct Sampler {
        system: System,
        /// Room for the process list [`working_set`] reads, kept between ticks.
        process_info: Vec<u64>,
    }

    impl Sampler {
        /// Takes a first reading, so the next [`Sampler::sample`] has CPU use to report.
        pub fn new(processes: &[ProcessEntry]) -> Option<Sampler> {
            let system = match System::new() {
                Ok(system) => system,
                Err(err) => {
                    eprintln!("refract: system stats unavailable: {err}");
                    return None;
                }
            };
            let mut sampler = Sampler {
                system,
                process_info: Vec::new(),
            };
            sampler.refresh(&own_pids(processes));
            Some(sampler)
        }

        fn refresh(&mut self, own: &[Pid]) {
            self.system.refresh_cpu_usage();
            self.system.refresh_memory();
            self.system.refresh_processes_specifics(
                ProcessesToUpdate::Some(own),
                true,
                ProcessRefreshKind::nothing().with_cpu().with_memory(),
            );
        }

        /// Reads the machine now. `gpu_samples` is this tick's GPU counter reading,
        /// empty when there is none; `linux` is the helper's answer, if any.
        pub fn sample(
            &mut self,
            processes: &[ProcessEntry],
            gpu_samples: &[(String, f64)],
            linux: Option<LinuxStats>,
        ) -> SystemStats {
            let own_set = own_processes(std::process::id(), processes);
            let own: Vec<Pid> = own_set.iter().map(|&pid| Pid::from_u32(pid)).collect();
            self.refresh(&own);
            let loads: Vec<ProcessLoad> = own
                .iter()
                .filter_map(|&pid| self.system.process(pid))
                .map(|p| ProcessLoad {
                    cpu_percent: f64::from(p.cpu_usage()),
                    mem_bytes: p.memory(),
                })
                .collect();
            let machine = Machine {
                cpu_percent: f64::from(self.system.global_cpu_usage()),
                mem_used_bytes: self.system.used_memory(),
                mem_total_bytes: self.system.total_memory(),
                cpu_count: self.system.cpus().len(),
            };
            let wsl = linux.map(|linux| {
                let vm = wsl_vm(processes);
                WslInput {
                    linux,
                    working_set: vm.and_then(|pid| self.working_set(pid)),
                    gpu_percent: vm.and_then(|pid| gpu::process_percent(gpu_samples, pid)),
                }
            });
            aggregate(
                machine,
                &loads,
                gpu::usage(gpu_samples, &own_set),
                wsl.as_ref(),
            )
        }

        /// A process's working set from the system's process list. `vmmemWSL` is a
        /// protected process: opening it is refused without admin, so neither `sysinfo`
        /// nor `GetProcessMemoryInfo` can read it, but the list has it, as for
        /// `Get-Process`.
        pub fn working_set(&mut self, pid: u32) -> Option<u64> {
            let mut len = 0u32;
            let mut filled = false;
            for _ in 0..4 {
                if self.process_info.is_empty() {
                    self.process_info = vec![0; 1 << 16];
                }
                let size = u32::try_from(self.process_info.len() * 8).ok()?;
                // SAFETY: the buffer is `size` bytes and 8-byte aligned.
                let status = unsafe {
                    NtQuerySystemInformation(
                        SystemProcessInformation,
                        self.process_info.as_mut_ptr().cast(),
                        size,
                        &mut len,
                    )
                };
                if status == STATUS_INFO_LENGTH_MISMATCH {
                    // Room for the processes started since the size was given.
                    let words = (len as usize).div_ceil(8) + (1 << 13);
                    let words = words.max(self.process_info.len() * 2);
                    if words * 8 > PROCESS_INFO_CAP {
                        return None;
                    }
                    self.process_info = vec![0; words];
                    continue;
                }
                filled = status.is_ok();
                break;
            }
            if !filled {
                return None;
            }
            let len = (len as usize).min(self.process_info.len() * 8);
            // SAFETY: the bytes of initialised `u64`s, no more than the buffer holds.
            let bytes =
                unsafe { std::slice::from_raw_parts(self.process_info.as_ptr().cast::<u8>(), len) };
            find_working_set(bytes, pid)
        }
    }

    /// `pid`'s working set from a `SystemProcessInformation` list: entries chained by
    /// `NextEntryOffset`, the last one's being 0. An entry that runs past the end of
    /// `list` ends the walk.
    pub fn find_working_set(list: &[u8], pid: u32) -> Option<u64> {
        use std::mem::{offset_of, size_of};
        let entry = size_of::<SYSTEM_PROCESS_INFORMATION>();
        let word = |at: usize| -> Option<usize> {
            let bytes = list.get(at..at.checked_add(size_of::<usize>())?)?;
            Some(usize::from_ne_bytes(bytes.try_into().ok()?))
        };
        let mut offset = 0usize;
        loop {
            if offset.checked_add(entry)? > list.len() {
                return None;
            }
            if word(offset + offset_of!(SYSTEM_PROCESS_INFORMATION, UniqueProcessId))?
                == pid as usize
            {
                let ws = word(offset + offset_of!(SYSTEM_PROCESS_INFORMATION, WorkingSetSize))?;
                return Some(ws as u64);
            }
            let next = u32::from_ne_bytes(list.get(offset..offset + 4)?.try_into().ok()?);
            if next == 0 {
                return None;
            }
            offset = offset.checked_add(next as usize)?;
        }
    }

    fn own_pids(processes: &[ProcessEntry]) -> Vec<Pid> {
        own_processes(std::process::id(), processes)
            .into_iter()
            .map(Pid::from_u32)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(pid: u32, parent: u32, name: &str) -> ProcessEntry {
        ProcessEntry {
            pid,
            parent,
            name: name.into(),
        }
    }

    #[test]
    fn own_processes_follow_webview_and_wsl_only() {
        // 100 is the app; 200 the WebView2 browser with its GPU and renderer
        // processes; 210 the helper's wsl.exe; 220 a VS Code started by a launch,
        // with a child of its own; 400 an unrelated WebView2.
        let processes = [
            entry(0, 0, "[System Process]"),
            entry(100, 50, "refract.exe"),
            entry(200, 100, "msedgewebview2.exe"),
            entry(300, 200, "msedgewebview2.exe"),
            entry(301, 200, "MSEdgeWebView2.exe"),
            entry(210, 100, "wsl.exe"),
            entry(220, 100, "cmd.exe"),
            entry(221, 220, "Code.exe"),
            entry(222, 221, "msedgewebview2.exe"),
            entry(400, 4, "msedgewebview2.exe"),
        ];
        assert_eq!(
            own_processes(100, &processes),
            HashSet::from([100, 200, 300, 301, 210])
        );
        assert_eq!(own_processes(7, &[]), HashSet::from([7]));
    }

    const MACHINE: Machine = Machine {
        cpu_percent: 37.5,
        mem_used_bytes: 12 << 30,
        mem_total_bytes: 32 << 30,
        cpu_count: 8,
    };

    #[test]
    fn own_share_is_summed_and_spread_over_cpus() {
        let own = [
            ProcessLoad {
                cpu_percent: 6.0,
                mem_bytes: 40 << 20,
            },
            ProcessLoad {
                cpu_percent: 2.0,
                mem_bytes: 90 << 20,
            },
        ];
        let gpu = GpuUsage {
            total_percent: 55.0,
            own_percent: 3.5,
        };
        assert_eq!(
            aggregate(MACHINE, &own, gpu, None),
            SystemStats {
                cpu_percent: 37.5,
                mem_used_bytes: 12 << 30,
                mem_total_bytes: 32 << 30,
                gpu_percent: 55.0,
                wsl: None,
                own: OwnShare {
                    cpu_percent: 1.0,
                    mem_bytes: 130 << 20,
                    gpu_percent: 3.5,
                    wsl_cpu_percent: 0.0,
                    wsl_mem_bytes: 0,
                },
            }
        );
    }

    #[test]
    fn odd_readings_are_clamped() {
        let machine = Machine {
            cpu_percent: f64::NAN,
            mem_used_bytes: 40,
            mem_total_bytes: 30,
            cpu_count: 0,
        };
        let own = [
            ProcessLoad {
                cpu_percent: 250.0,
                mem_bytes: 50,
            },
            ProcessLoad {
                cpu_percent: f64::INFINITY,
                mem_bytes: 0,
            },
            ProcessLoad {
                cpu_percent: -3.0,
                mem_bytes: 0,
            },
        ];
        let gpu = GpuUsage {
            total_percent: 140.0,
            own_percent: -1.0,
        };
        let stats = aggregate(machine, &own, gpu, None);
        assert_eq!(stats.cpu_percent, 0.0);
        assert_eq!(stats.mem_used_bytes, 30);
        assert_eq!(stats.gpu_percent, 100.0);
        assert_eq!(stats.own.cpu_percent, 100.0);
        assert_eq!(stats.own.mem_bytes, 30);
        assert_eq!(stats.own.gpu_percent, 0.0);
        assert_eq!(aggregate(MACHINE, &[], gpu, None).own.cpu_percent, 0.0);
    }

    #[test]
    fn stats_are_sent_in_camel_case() {
        let gpu = GpuUsage {
            total_percent: 20.0,
            own_percent: 2.0,
        };
        let own = [ProcessLoad {
            cpu_percent: 4.0,
            mem_bytes: 100,
        }];
        assert_eq!(
            serde_json::to_value(aggregate(MACHINE, &own, gpu, None)).unwrap(),
            serde_json::json!({
                "cpuPercent": 37.5,
                "memUsedBytes": 12u64 << 30,
                "memTotalBytes": 32u64 << 30,
                "gpuPercent": 20.0,
                "wsl": null,
                "own": {
                    "cpuPercent": 0.5,
                    "memBytes": 100,
                    "gpuPercent": 2.0,
                    "wslCpuPercent": 0.0,
                    "wslMemBytes": 0,
                },
            })
        );
        let stats = aggregate(MACHINE, &own, gpu, Some(&wsl_input()));
        let value = serde_json::to_value(stats).unwrap();
        assert_eq!(
            value["wsl"],
            serde_json::json!({
                "cpuPercent": 10.0,
                "memBytes": 6u64 << 30,
                "memProgramBytes": 2u64 << 30,
                "memCacheBytes": 4u64 << 30,
                "gpuPercent": 1.5,
            })
        );
        assert_eq!(value["own"]["wslCpuPercent"], 0.25);
        let no_gpu = WslInput {
            gpu_percent: None,
            ..wsl_input()
        };
        let value = serde_json::to_value(aggregate(MACHINE, &own, gpu, Some(&no_gpu))).unwrap();
        assert_eq!(value["wsl"]["gpuPercent"], serde_json::Value::Null);
    }

    fn wsl_input() -> WslInput {
        WslInput {
            linux: LinuxStats {
                // Linux has 4 of the machine's 8 CPUs.
                cpu_percent: Some(20.0),
                cpu_count: 4,
                mem_total_bytes: 8 << 30,
                mem_used_bytes: 2 << 30,
                mem_cache_bytes: 5 << 30,
                own: refract_core::protocol::LinuxOwnStats {
                    cpu_percent: Some(0.5),
                    mem_bytes: 40 << 20,
                },
            },
            working_set: Some(6 << 30),
            gpu_percent: Some(1.5),
        }
    }

    #[test]
    fn working_set_is_split_programs_first() {
        // All resident.
        assert_eq!(split_working_set(10, 3, 5), (3, 5));
        // Less resident than programs and cache: the cache is what is short.
        assert_eq!(split_working_set(6, 3, 5), (3, 3));
        // Less resident than programs alone: no cache at all.
        assert_eq!(split_working_set(2, 3, 5), (2, 0));
        assert_eq!(split_working_set(8, 3, 0), (3, 0));
        assert_eq!(split_working_set(0, 3, 5), (0, 0));
    }

    #[test]
    fn wsl_is_scaled_to_windows_cpus_and_added_to_own() {
        let own = [ProcessLoad {
            cpu_percent: 8.0,
            mem_bytes: 200 << 20,
        }];
        let gpu = GpuUsage {
            total_percent: 20.0,
            own_percent: 2.0,
        };
        let stats = aggregate(MACHINE, &own, gpu, Some(&wsl_input()));
        assert_eq!(
            stats.wsl,
            Some(WslShare {
                // 20 % of 4 Linux CPUs is 10 % of 8 Windows ones.
                cpu_percent: 10.0,
                mem_bytes: 6 << 30,
                mem_program_bytes: 2 << 30,
                mem_cache_bytes: 4 << 30,
                gpu_percent: Some(1.5),
            })
        );
        assert_eq!(
            stats.own,
            OwnShare {
                cpu_percent: 1.25,
                mem_bytes: 240 << 20,
                gpu_percent: 2.0,
                wsl_cpu_percent: 0.25,
                wsl_mem_bytes: 40 << 20,
            }
        );
    }

    #[test]
    fn wsl_is_limited_to_the_machine() {
        let busy = WslInput {
            linux: LinuxStats {
                cpu_percent: Some(100.0),
                cpu_count: 16,
                own: refract_core::protocol::LinuxOwnStats {
                    cpu_percent: Some(90.0),
                    mem_bytes: 20 << 30,
                },
                ..wsl_input().linux
            },
            working_set: Some(14 << 30),
            gpu_percent: Some(150.0),
        };
        let stats = aggregate(
            MACHINE,
            &[],
            GpuUsage {
                total_percent: 0.0,
                own_percent: 0.0,
            },
            Some(&busy),
        );
        let wsl = stats.wsl.unwrap();
        // No more than the machine's CPU use (37.5 %) or memory in use (12 GB).
        assert_eq!(wsl.cpu_percent, 37.5);
        assert_eq!(wsl.mem_bytes, 12 << 30);
        assert_eq!(wsl.gpu_percent, Some(100.0));
        assert_eq!(stats.own.wsl_cpu_percent, 37.5);
        assert_eq!(stats.own.wsl_mem_bytes, 12 << 30);
    }

    #[test]
    fn wsl_needs_a_cpu_reading_and_falls_back_to_linux_memory() {
        let first = WslInput {
            linux: LinuxStats {
                cpu_percent: None,
                ..wsl_input().linux
            },
            ..wsl_input()
        };
        let gpu = GpuUsage {
            total_percent: 0.0,
            own_percent: 0.0,
        };
        assert_eq!(aggregate(MACHINE, &[], gpu, Some(&first)).wsl, None);
        // No working set: Linux's MemTotal - MemFree (programs and cache) stands in.
        let unread = WslInput {
            working_set: None,
            ..wsl_input()
        };
        let wsl = aggregate(MACHINE, &[], gpu, Some(&unread)).wsl.unwrap();
        assert_eq!(wsl.mem_bytes, 7 << 30);
        assert_eq!(
            (wsl.mem_program_bytes, wsl.mem_cache_bytes),
            (2 << 30, 5 << 30)
        );
        // The helper's own CPU can be missing on its own; it then counts as none.
        let no_own = WslInput {
            linux: LinuxStats {
                own: refract_core::protocol::LinuxOwnStats {
                    cpu_percent: None,
                    mem_bytes: 1,
                },
                ..wsl_input().linux
            },
            ..wsl_input()
        };
        assert_eq!(
            aggregate(MACHINE, &[], gpu, Some(&no_own))
                .own
                .wsl_cpu_percent,
            0.0
        );
    }

    #[test]
    fn the_vm_is_found_by_name() {
        let processes = [entry(4, 0, "System"), entry(12148, 900, "vmmemWSL")];
        assert_eq!(wsl_vm(&processes), Some(12148));
        assert_eq!(wsl_vm(&[entry(5, 0, "vmmem")]), None);
    }

    /// `secs` seconds into a test, as the sampler's 2 s ticks would see it.
    fn tick(secs: u64) -> Instant {
        static START: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
        *START.get_or_init(Instant::now) + Duration::from_secs(secs)
    }

    fn linux_reply(cpu: f64) -> Result<Reply, RequestError> {
        let mut linux = wsl_input().linux;
        linux.cpu_percent = Some(cpu);
        Ok(Reply::SystemStats(linux))
    }

    fn cpu_of(stats: Option<LinuxStats>) -> Option<f64> {
        stats.and_then(|s| s.cpu_percent)
    }

    #[test]
    fn linux_answers_count_after_a_priming_one() {
        let mut source = LinuxSource::default();
        assert_eq!(cpu_of(source.accept(linux_reply(1.0), 1, tick(0))), None);
        assert_eq!(
            cpu_of(source.accept(linux_reply(2.0), 1, tick(2))),
            Some(2.0)
        );
        // A late answer leaves a gap, so the one after it primes again; the last good
        // reading stands in for both while it is under 6 s old.
        let late = source.accept(Err(RequestError::Timeout), 1, tick(4));
        assert_eq!(cpu_of(late), Some(2.0));
        assert_eq!(
            cpu_of(source.accept(linux_reply(3.0), 1, tick(6))),
            Some(2.0)
        );
        assert_eq!(
            cpu_of(source.accept(linux_reply(4.0), 1, tick(8))),
            Some(4.0)
        );
        // Longer without an answer, it is dropped.
        source.accept(Err(RequestError::NotRunning), 1, tick(10));
        source.accept(Err(RequestError::NotRunning), 1, tick(12));
        assert_eq!(
            source.accept(Err(RequestError::NotRunning), 1, tick(14)),
            None
        );
        assert_eq!(source.accept(linux_reply(5.0), 1, tick(16)), None);
        // A restarted helper's first answer has no CPU use: it primes too.
        let mut first = wsl_input().linux;
        first.cpu_percent = None;
        assert_eq!(
            cpu_of(source.accept(linux_reply(6.0), 1, tick(18))),
            Some(6.0)
        );
        let held = source.accept(Ok(Reply::SystemStats(first)), 2, tick(20));
        assert_eq!(cpu_of(held), Some(6.0));
    }

    #[test]
    fn an_older_helper_is_not_asked_again_until_it_restarts() {
        // Helper starts far from the ones other tests use: the marker is shared.
        let mut source = LinuxSource::default();
        source.accept(linux_reply(1.0), 1_000_003, tick(0));
        source.accept(linux_reply(2.0), 1_000_003, tick(2));
        let unknown = || {
            Err(RequestError::Agent {
                code: code::METHOD_NOT_FOUND,
                message: "unknown method".into(),
            })
        };
        // No held reading either: an older helper gives no WSL figures at all.
        assert_eq!(source.accept(unknown(), 1_000_003, tick(4)), None);
        assert_eq!(UNSUPPORTED.load(Ordering::SeqCst), 1_000_003);
        // Other errors are not taken for an older helper.
        let mut other = LinuxSource::default();
        other.accept(Err(RequestError::NotRunning), 2_000_005, tick(0));
        let internal = RequestError::Agent {
            code: code::INTERNAL_ERROR,
            message: String::new(),
        };
        other.accept(Err(internal), 2_000_005, tick(2));
        assert_ne!(UNSUPPORTED.load(Ordering::SeqCst), 2_000_005);
    }

    /// A `SystemProcessInformation` list of `(pid, working set, next offset)` entries.
    #[cfg(windows)]
    fn process_list(entries: &[(usize, usize, u32)]) -> Vec<u8> {
        use std::mem::{offset_of, size_of};
        use windows::Win32::System::WindowsProgramming::SYSTEM_PROCESS_INFORMATION as Info;
        let mut list = Vec::new();
        for &(pid, ws, next) in entries {
            let mut entry = vec![0u8; (next as usize).max(size_of::<Info>())];
            entry[..4].copy_from_slice(&next.to_ne_bytes());
            let id = offset_of!(Info, UniqueProcessId);
            entry[id..id + size_of::<usize>()].copy_from_slice(&pid.to_ne_bytes());
            let at = offset_of!(Info, WorkingSetSize);
            entry[at..at + size_of::<usize>()].copy_from_slice(&ws.to_ne_bytes());
            list.extend(entry);
        }
        list
    }

    #[cfg(windows)]
    #[test]
    fn working_sets_are_found_in_the_process_list() {
        use windows::Win32::System::WindowsProgramming::SYSTEM_PROCESS_INFORMATION as Info;
        let size = std::mem::size_of::<Info>() as u32;
        // Entries with room for their threads after them, as the real list has.
        let list = process_list(&[(0, 0, size + 64), (4, 100, size), (12148, 6 << 30, 0)]);
        assert_eq!(find_working_set(&list, 4), Some(100));
        assert_eq!(find_working_set(&list, 12148), Some(6 << 30));
        assert_eq!(find_working_set(&list, 99), None);
        // A last entry cut short is not read.
        let cut = &list[..list.len() - 8];
        assert_eq!(find_working_set(cut, 12148), None);
        assert_eq!(find_working_set(cut, 4), Some(100));
        // NextEntryOffset 0 ends the list even with more bytes after it.
        let first_only = process_list(&[(4, 100, 0), (12148, 7, 0)]);
        assert_eq!(find_working_set(&first_only, 12148), None);
        // An offset past the end, and an empty list.
        let past = process_list(&[(4, 100, 1 << 20)]);
        assert_eq!(find_working_set(&past[..size as usize], 12148), None);
        assert_eq!(find_working_set(&[], 4), None);
    }

    /// Reads the real machine twice and prints what it saw. Run with `--ignored`.
    #[cfg(windows)]
    #[test]
    #[ignore]
    fn samples_real_stats() {
        let processes = crate::gpu::sampler_processes();
        let mut sampler = Sampler::new(&processes).expect("opening sysinfo");
        std::thread::sleep(std::time::Duration::from_secs(1));
        let started = std::time::Instant::now();
        let processes = crate::gpu::sampler_processes();
        let stats = sampler.sample(&processes, &[], None);
        println!("sampled in {:?}: {stats:?}", started.elapsed());
        if let Some(vm) = wsl_vm(&processes) {
            let started = std::time::Instant::now();
            let working_set = sampler.working_set(vm);
            println!(
                "vmmemWSL ({vm}) working set {working_set:?}, read in {:?}",
                started.elapsed()
            );
            assert!(working_set.is_some_and(|ws| ws > 0));
        }
        assert!(stats.mem_total_bytes > 0);
        assert!(stats.mem_used_bytes > 0);
        assert!(stats.own.mem_bytes > 0);
    }
}
