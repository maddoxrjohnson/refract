//! Machine load for the system widget: CPU and memory from `sysinfo`, GPU from the
//! counters `gpu.rs` already reads, and Refract's own share of each. Sampled on the GPU
//! sampler's thread, so it too runs only while the window is visible.

use std::collections::HashSet;

use serde::Serialize;

use crate::gpu::GpuUsage;

/// Emitted with [`SystemStats`] after each reading of the GPU counters.
pub const STATS_EVENT: &str = "system://stats";

/// Executables that count as Refract when started by it: WebView2, which draws the
/// interface, and `wsl.exe`, which carries the helper. Anything else the app starts
/// (VS Code, `cmd.exe`, `gh.exe`) is not Refract's own load, and neither are its children.
const OWN_IMAGES: &[&str] = &["msedgewebview2.exe", "wsl.exe"];

#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SystemStats {
    /// All CPUs, 0 to 100.
    pub cpu_percent: f64,
    pub mem_used_bytes: u64,
    pub mem_total_bytes: u64,
    /// Busiest GPU engine, as in [`GpuUsage::total_percent`].
    pub gpu_percent: f64,
    pub own: OwnShare,
}

/// What this process, its WebView2 processes and the helper's `wsl.exe` use.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OwnShare {
    /// Of all CPUs, 0 to 100.
    pub cpu_percent: f64,
    pub mem_bytes: u64,
    pub gpu_percent: f64,
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

/// A percentage that is finite and within 0 to 100.
fn percent(value: f64) -> f64 {
    if value.is_finite() {
        value.clamp(0.0, 100.0)
    } else {
        0.0
    }
}

/// Puts one reading together. `gpu` is the GPU use with `own_percent` counted over the
/// same processes as `own`.
pub fn aggregate(machine: Machine, own: &[ProcessLoad], gpu: GpuUsage) -> SystemStats {
    let own_cpu: f64 = own
        .iter()
        .map(|p| p.cpu_percent)
        .filter(|v| v.is_finite() && *v > 0.0)
        .sum();
    let own_mem: u64 = own.iter().map(|p| p.mem_bytes).sum();
    SystemStats {
        cpu_percent: percent(machine.cpu_percent),
        mem_used_bytes: machine.mem_used_bytes.min(machine.mem_total_bytes),
        mem_total_bytes: machine.mem_total_bytes,
        gpu_percent: percent(gpu.total_percent),
        own: OwnShare {
            cpu_percent: percent(own_cpu / machine.cpu_count.max(1) as f64),
            mem_bytes: own_mem.min(machine.mem_total_bytes),
            gpu_percent: percent(gpu.own_percent),
        },
    }
}

#[cfg(windows)]
pub use sampler::Sampler;

#[cfg(windows)]
mod sampler {
    use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};

    use super::{aggregate, own_processes, Machine, ProcessEntry, ProcessLoad, SystemStats};
    use crate::gpu;

    /// Keeps `sysinfo`'s previous reading, which CPU use is measured against.
    pub struct Sampler {
        system: System,
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
            let mut sampler = Sampler { system };
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
        /// empty when there is none.
        pub fn sample(
            &mut self,
            processes: &[ProcessEntry],
            gpu_samples: &[(String, f64)],
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
            aggregate(machine, &loads, gpu::usage(gpu_samples, &own_set))
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
            aggregate(MACHINE, &own, gpu),
            SystemStats {
                cpu_percent: 37.5,
                mem_used_bytes: 12 << 30,
                mem_total_bytes: 32 << 30,
                gpu_percent: 55.0,
                own: OwnShare {
                    cpu_percent: 1.0,
                    mem_bytes: 130 << 20,
                    gpu_percent: 3.5,
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
        let stats = aggregate(machine, &own, gpu);
        assert_eq!(stats.cpu_percent, 0.0);
        assert_eq!(stats.mem_used_bytes, 30);
        assert_eq!(stats.gpu_percent, 100.0);
        assert_eq!(stats.own.cpu_percent, 100.0);
        assert_eq!(stats.own.mem_bytes, 30);
        assert_eq!(stats.own.gpu_percent, 0.0);
        assert_eq!(aggregate(MACHINE, &[], gpu).own.cpu_percent, 0.0);
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
            serde_json::to_value(aggregate(MACHINE, &own, gpu)).unwrap(),
            serde_json::json!({
                "cpuPercent": 37.5,
                "memUsedBytes": 12u64 << 30,
                "memTotalBytes": 32u64 << 30,
                "gpuPercent": 20.0,
                "own": {"cpuPercent": 0.5, "memBytes": 100, "gpuPercent": 2.0},
            })
        );
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
        let stats = sampler.sample(&crate::gpu::sampler_processes(), &[]);
        println!("sampled in {:?}: {stats:?}", started.elapsed());
        assert!(stats.mem_total_bytes > 0);
        assert!(stats.mem_used_bytes > 0);
        assert!(stats.own.mem_bytes > 0);
    }
}
