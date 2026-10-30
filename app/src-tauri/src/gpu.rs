//! GPU use from Windows' performance counters, sampled while the window is visible.
//! The same thread samples the system widget's stats (`stats.rs`).
//!
//! The counter is PDH's `\GPU Engine(*)\Utilization Percentage`, whose instances are
//! named like `pid_1234_luid_0x00000000_0x0000D1C1_phys_0_eng_3_engtype_3D`: one per
//! process and GPU engine. The busiest kind of engine stands for the whole GPU, as in
//! Task Manager.

use std::collections::{HashMap, HashSet};

use serde::Serialize;
use tauri::AppHandle;

/// Emitted with [`GpuUsage`] about every [`INTERVAL`] while the window is visible.
pub const GPU_EVENT: &str = "system://gpu";

/// How often the counters are read.
#[cfg_attr(not(windows), allow(dead_code))]
const INTERVAL: std::time::Duration = std::time::Duration::from_secs(2);

#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GpuUsage {
    /// Busiest engine type across all processes, 0 to 100.
    pub total_percent: f64,
    /// The same, counting only this process and the WebView2 processes it started.
    pub own_percent: f64,
}

/// What a counter instance's name says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Engine<'a> {
    pub pid: u32,
    /// The adapter: what follows `luid_`, up to `_eng_`. Engines of different GPUs
    /// are not added up.
    pub adapter: &'a str,
    /// The engine's number on its adapter.
    pub index: &'a str,
    /// `3D`, `Copy`, `VideoDecode` and so on. Empty for engines the driver leaves
    /// unnamed, which are then told apart by [`Engine::index`].
    pub engine_type: &'a str,
}

impl<'a> Engine<'a> {
    /// What instances are added up by: the engine type, or the engine itself when it
    /// has no type.
    fn group(&self) -> (&'a str, &'a str, &'a str) {
        if self.engine_type.is_empty() {
            (self.adapter, "", self.index)
        } else {
            (self.adapter, self.engine_type, "")
        }
    }
}

/// Reads an instance name such as
/// `pid_1234_luid_0x00000000_0x0000D1C1_phys_0_eng_3_engtype_3D`; the engine type can
/// be empty. Anything else is `None`.
pub fn parse_instance(name: &str) -> Option<Engine<'_>> {
    let rest = name.strip_prefix("pid_")?;
    let (pid, rest) = rest.split_once('_')?;
    let pid = pid.parse().ok()?;
    let rest = rest.strip_prefix("luid_")?;
    let (adapter, rest) = rest.split_once("_eng_")?;
    let (index, engine_type) = rest.split_once("_engtype_")?;
    if adapter.is_empty() || index.is_empty() {
        return None;
    }
    Some(Engine {
        pid,
        adapter,
        index,
        engine_type,
    })
}

/// Adds up each (adapter, engine type) across the instances `include` accepts and
/// returns the busiest, capped at 100. Unreadable names and values that are not
/// finite are skipped.
pub fn busiest_engine<'a>(
    samples: impl IntoIterator<Item = (&'a str, f64)>,
    include: impl Fn(u32) -> bool,
) -> f64 {
    let mut sums: HashMap<(&str, &str, &str), f64> = HashMap::new();
    for (name, value) in samples {
        let Some(engine) = parse_instance(name) else {
            continue;
        };
        if !value.is_finite() || !include(engine.pid) {
            continue;
        }
        *sums.entry(engine.group()).or_default() += value.max(0.0);
    }
    sums.into_values().fold(0.0, f64::max).min(100.0)
}

/// Total and own use from one reading of the counter. `own` holds this process's id
/// and those of the processes it started.
pub fn usage(samples: &[(String, f64)], own: &HashSet<u32>) -> GpuUsage {
    let pairs = || samples.iter().map(|(name, value)| (name.as_str(), *value));
    GpuUsage {
        total_percent: busiest_engine(pairs(), |_| true),
        own_percent: busiest_engine(pairs(), |pid| own.contains(&pid)),
    }
}

/// `root` and every process descended from it, from `(pid, parent pid)` pairs. WebView2
/// runs its browser process as a child of the app and its GPU and renderer processes
/// as children of that. A parent id can be stale (Windows reuses ids), so a process
/// whose parent has exited can be miscounted; that is rare and short-lived.
pub fn descendants(root: u32, processes: &[(u32, u32)]) -> HashSet<u32> {
    let mut found = HashSet::from([root]);
    let mut queue = vec![root];
    while let Some(parent) = queue.pop() {
        for &(pid, ppid) in processes {
            // The System Idle Process (0) is its own parent.
            if ppid == parent && pid != parent && found.insert(pid) {
                queue.push(pid);
            }
        }
    }
    found
}

/// Starts sampling when the window becomes visible and stops when it is hidden.
/// Called by `window::sync_visibility` on each change.
pub fn set_sampling(app: &AppHandle, visible: bool) {
    #[cfg(windows)]
    sampler::set(app, visible);
    #[cfg(not(windows))]
    let _ = (app, visible);
}

/// The process list the sampler reads, for the ignored test in `stats.rs`.
#[cfg(all(windows, test))]
pub fn sampler_processes() -> Vec<crate::stats::ProcessEntry> {
    sampler::processes()
}

#[cfg(windows)]
mod sampler {
    #[cfg(test)]
    use std::collections::HashSet;
    use std::sync::mpsc::{self, RecvTimeoutError, Sender};
    use std::sync::Mutex;
    use std::thread;

    use tauri::{AppHandle, Emitter};
    use windows::core::w;
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
        TH32CS_SNAPPROCESS,
    };
    use windows::Win32::System::Performance::{
        PdhAddEnglishCounterW, PdhCloseQuery, PdhCollectQueryData, PdhGetFormattedCounterArrayW,
        PdhOpenQueryW, PDH_CSTATUS_NEW_DATA, PDH_CSTATUS_VALID_DATA, PDH_FMT_COUNTERVALUE_ITEM_W,
        PDH_FMT_DOUBLE, PDH_HCOUNTER, PDH_HQUERY, PDH_MORE_DATA,
    };

    use super::{descendants, usage, GPU_EVENT, INTERVAL};
    use crate::stats::{self, ProcessEntry, STATS_EVENT};

    /// The running sampler's stop signal. Dropping it ends the thread at once.
    static RUNNING: Mutex<Option<Sender<()>>> = Mutex::new(None);

    pub fn set(app: &AppHandle, visible: bool) {
        let Ok(mut running) = RUNNING.lock() else {
            return;
        };
        if !visible {
            // The thread sees the channel close, closes its query and ends.
            *running = None;
            return;
        }
        if running.is_some() {
            return;
        }
        let (stop, stopped) = mpsc::channel::<()>();
        let app = app.clone();
        let spawned = thread::Builder::new()
            .name("refract-gpu".into())
            .spawn(move || {
                // Without GPU counters the stats still go out, with no GPU use.
                let query = Query::open()
                    .map_err(|err| eprintln!("refract: GPU counters unavailable: {err}"))
                    .ok();
                // The first readings only prime the rate counters.
                if let Some(query) = &query {
                    query.collect();
                }
                let mut sampler = stats::Sampler::new(&processes());
                loop {
                    match stopped.recv_timeout(INTERVAL) {
                        Err(RecvTimeoutError::Timeout) => {}
                        _ => return,
                    }
                    let samples = query.as_ref().and_then(Query::read);
                    let processes = processes();
                    if let Some(samples) = &samples {
                        let pairs: Vec<(u32, u32)> =
                            processes.iter().map(|p| (p.pid, p.parent)).collect();
                        let own = descendants(std::process::id(), &pairs);
                        if let Err(err) = app.emit(GPU_EVENT, usage(samples, &own)) {
                            eprintln!("refract: emitting {GPU_EVENT}: {err}");
                        }
                    }
                    if let Some(sampler) = &mut sampler {
                        let stats = sampler.sample(&processes, samples.as_deref().unwrap_or(&[]));
                        if let Err(err) = app.emit(STATS_EVENT, stats) {
                            eprintln!("refract: emitting {STATS_EVENT}: {err}");
                        }
                    }
                }
            });
        match spawned {
            Ok(_) => *running = Some(stop),
            Err(err) => eprintln!("refract: starting the GPU sampler: {err}"),
        }
    }

    /// A PDH query on the GPU engine counter, closed on drop.
    pub struct Query {
        query: PDH_HQUERY,
        counter: PDH_HCOUNTER,
    }

    impl Query {
        pub fn open() -> Result<Query, String> {
            let mut query = PDH_HQUERY::default();
            // SAFETY: plain PDH calls with out-pointers to locals.
            let status = unsafe { PdhOpenQueryW(None, 0, &mut query) };
            if status != 0 {
                return Err(format!("PdhOpenQuery failed: {status:#x}"));
            }
            let mut counter = PDH_HCOUNTER::default();
            let status = unsafe {
                PdhAddEnglishCounterW(
                    query,
                    w!(r"\GPU Engine(*)\Utilization Percentage"),
                    0,
                    &mut counter,
                )
            };
            if status != 0 {
                unsafe { PdhCloseQuery(query) };
                return Err(format!("PdhAddEnglishCounter failed: {status:#x}"));
            }
            Ok(Query { query, counter })
        }

        /// Takes a reading. Returns whether it worked.
        pub fn collect(&self) -> bool {
            // SAFETY: the query is open until drop.
            unsafe { PdhCollectQueryData(self.query) == 0 }
        }

        /// Takes a reading and returns each instance's utilization since the last
        /// one, or `None` if there is none.
        pub fn read(&self) -> Option<Vec<(String, f64)>> {
            if !self.collect() {
                return None;
            }
            let mut size = 0u32;
            let mut count = 0u32;
            // SAFETY: the first call asks for the buffer size; the second fills a
            // buffer of that size, which is aligned for the items it starts with.
            let status = unsafe {
                PdhGetFormattedCounterArrayW(
                    self.counter,
                    PDH_FMT_DOUBLE,
                    &mut size,
                    &mut count,
                    None,
                )
            };
            if status != PDH_MORE_DATA {
                return None;
            }
            let item = std::mem::size_of::<PDH_FMT_COUNTERVALUE_ITEM_W>();
            let mut buffer: Vec<PDH_FMT_COUNTERVALUE_ITEM_W> =
                vec![PDH_FMT_COUNTERVALUE_ITEM_W::default(); (size as usize).div_ceil(item)];
            let status = unsafe {
                PdhGetFormattedCounterArrayW(
                    self.counter,
                    PDH_FMT_DOUBLE,
                    &mut size,
                    &mut count,
                    Some(buffer.as_mut_ptr()),
                )
            };
            if status != 0 {
                return None;
            }
            let samples = buffer
                .iter()
                .take(count as usize)
                .filter(|item| {
                    matches!(
                        item.FmtValue.CStatus,
                        PDH_CSTATUS_VALID_DATA | PDH_CSTATUS_NEW_DATA
                    )
                })
                // SAFETY: the names point into `buffer`; `doubleValue` is the member
                // PDH_FMT_DOUBLE fills.
                .filter_map(|item| {
                    let name = unsafe { item.szName.to_string() }.ok()?;
                    Some((name, unsafe { item.FmtValue.Anonymous.doubleValue }))
                })
                .collect();
            Some(samples)
        }
    }

    impl Drop for Query {
        fn drop(&mut self) {
            // SAFETY: closing the query also frees its counter.
            unsafe { PdhCloseQuery(self.query) };
        }
    }

    /// Every process, from a ToolHelp snapshot. Empty on error.
    pub fn processes() -> Vec<ProcessEntry> {
        let mut found = Vec::new();
        // SAFETY: the snapshot handle is closed below; the entry carries its size.
        unsafe {
            let Ok(snapshot) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else {
                return found;
            };
            let mut entry = PROCESSENTRY32W {
                dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
                ..Default::default()
            };
            let mut more = Process32FirstW(snapshot, &mut entry).is_ok();
            while more {
                let name = &entry.szExeFile;
                let len = name.iter().position(|&c| c == 0).unwrap_or(name.len());
                found.push(ProcessEntry {
                    pid: entry.th32ProcessID,
                    parent: entry.th32ParentProcessID,
                    name: String::from_utf16_lossy(&name[..len]),
                });
                more = Process32NextW(snapshot, &mut entry).is_ok();
            }
            let _ = CloseHandle(snapshot);
        }
        found
    }

    /// For the ignored test that reads the real counters.
    #[cfg(test)]
    pub fn own_pids() -> HashSet<u32> {
        let pairs: Vec<(u32, u32)> = processes().iter().map(|p| (p.pid, p.parent)).collect();
        descendants(std::process::id(), &pairs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: &str = "luid_0x00000000_0x0000D1C1_phys_0";
    const B: &str = "luid_0x00000000_0x0000E2F4_phys_0";

    fn name(pid: u32, adapter: &str, eng: u32, engine_type: &str) -> String {
        format!("pid_{pid}_{adapter}_eng_{eng}_engtype_{engine_type}")
    }

    fn samples(items: &[(String, f64)]) -> impl Iterator<Item = (&str, f64)> {
        items.iter().map(|(n, v)| (n.as_str(), *v))
    }

    #[test]
    fn instance_names_are_parsed() {
        assert_eq!(
            parse_instance("pid_1234_luid_0x00000000_0x0000D1C1_phys_0_eng_3_engtype_3D"),
            Some(Engine {
                pid: 1234,
                adapter: "0x00000000_0x0000D1C1_phys_0",
                index: "3",
                engine_type: "3D",
            })
        );
        assert_eq!(
            parse_instance("pid_8_luid_0x0_0x1_phys_1_eng_12_engtype_Video Decode")
                .map(|e| e.engine_type),
            Some("Video Decode")
        );
        assert_eq!(
            parse_instance("pid_8_luid_0x0_0x1_phys_0_eng_11_engtype_").map(|e| e.index),
            Some("11")
        );
        for bad in [
            "",
            "_Total",
            "pid__luid_0x0_phys_0_eng_0_engtype_3D",
            "pid_x_luid_0x0_phys_0_eng_0_engtype_3D",
            "pid_1_luid_0x0_phys_0_eng_0",
            "pid_1_luid_0x0_phys_0_eng__engtype_3D",
            "pid_1_0x0_phys_0_eng_0_engtype_3D",
        ] {
            assert_eq!(parse_instance(bad), None, "{bad}");
        }
    }

    #[test]
    fn busiest_engine_type_sums_processes() {
        let items = [
            (name(1, A, 0, "3D"), 20.0),
            (name(2, A, 0, "3D"), 15.0),
            (name(1, A, 1, "3D"), 5.0),
            (name(2, A, 3, "Copy"), 30.0),
            (name(3, A, 4, "VideoDecode"), 10.0),
            // Unnamed engines are each their own group.
            (name(1, A, 5, ""), 25.0),
            (name(2, A, 6, ""), 25.0),
        ];
        assert_eq!(busiest_engine(samples(&items), |_| true), 40.0);
        assert_eq!(busiest_engine(samples(&items), |pid| pid == 2), 30.0);
        assert_eq!(busiest_engine(samples(&items), |pid| pid == 9), 0.0);
    }

    #[test]
    fn adapters_are_not_added_together() {
        let items = [(name(1, A, 0, "3D"), 60.0), (name(1, B, 0, "3D"), 70.0)];
        assert_eq!(busiest_engine(samples(&items), |_| true), 70.0);
    }

    #[test]
    fn odd_values_are_capped_or_skipped() {
        let items = [
            (name(1, A, 0, "3D"), 80.0),
            (name(2, A, 0, "3D"), 45.0),
            (name(3, A, 1, "Copy"), f64::NAN),
            (name(3, A, 2, "Compute"), -5.0),
            ("not an engine".to_owned(), 99.0),
        ];
        assert_eq!(busiest_engine(samples(&items), |_| true), 100.0);
        assert_eq!(busiest_engine(samples(&items), |pid| pid == 3), 0.0);
        assert_eq!(busiest_engine(std::iter::empty(), |_| true), 0.0);
    }

    #[test]
    fn usage_counts_own_processes() {
        let items = vec![
            (name(10, A, 0, "3D"), 12.0),
            (name(11, A, 0, "3D"), 8.0),
            (name(99, A, 0, "3D"), 50.0),
        ];
        let own = HashSet::from([10, 11]);
        assert_eq!(
            usage(&items, &own),
            GpuUsage {
                total_percent: 70.0,
                own_percent: 20.0,
            }
        );
        assert_eq!(
            serde_json::to_value(usage(&items, &own)).unwrap(),
            serde_json::json!({"totalPercent": 70.0, "ownPercent": 20.0})
        );
    }

    #[test]
    fn descendants_follow_the_process_tree() {
        // 100 is the app, 200 the WebView2 browser, 300 its GPU process.
        let processes = [
            (0, 0),
            (4, 0),
            (100, 50),
            (200, 100),
            (300, 200),
            (301, 200),
            (400, 4),
        ];
        assert_eq!(
            descendants(100, &processes),
            HashSet::from([100, 200, 300, 301])
        );
        assert_eq!(descendants(0, &processes), HashSet::from([0, 4, 400]));
        assert_eq!(descendants(7, &[]), HashSet::from([7]));
    }

    /// Reads the real counters once and prints what it saw. Run with `--ignored`.
    #[cfg(windows)]
    #[test]
    #[ignore]
    fn samples_real_counters() {
        let query = sampler::Query::open().expect("opening the GPU counters");
        assert!(query.collect());
        std::thread::sleep(std::time::Duration::from_secs(1));
        let started = std::time::Instant::now();
        let samples = query.read().expect("reading the GPU counters");
        let read_in = started.elapsed();
        let own = sampler::own_pids();
        println!(
            "read in {read_in:?}, processes listed in {:?}",
            started.elapsed() - read_in
        );
        let result = usage(&samples, &own);
        let parsed = samples
            .iter()
            .filter(|(n, _)| parse_instance(n).is_some())
            .count();
        println!(
            "{} instances ({parsed} parsed), {} own pids, {result:?}",
            samples.len(),
            own.len()
        );
        for (name, _) in samples
            .iter()
            .filter(|(n, _)| parse_instance(n).is_none())
            .take(5)
        {
            println!("unparsed: {name}");
        }
        if let Some((name, value)) = samples.iter().max_by(|a, b| a.1.total_cmp(&b.1)) {
            println!("busiest instance: {name} = {value:.2}");
        }
        assert_eq!(parsed, samples.len());
    }
}
