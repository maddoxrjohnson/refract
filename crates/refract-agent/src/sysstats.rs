//! CPU and memory as Linux sees them, for `systemStats` (see
//! `.plans/system-split-spec.md` §4–5).
//!
//! CPU figures compare two readings: the `cpu` line of `/proc/stat`, and the
//! `utime + stime` of the helper and its live descendants from `/proc/<pid>/stat`. Both
//! count clock ticks summed over every CPU, so a share of the `cpu` line's total since
//! the last reading is already a share of all Linux CPUs, with no need for the tick
//! rate or the wall clock. The first reading only primes the counters.
//!
//! Everything read is in `/proc`, which is in memory, so a sample takes a few
//! milliseconds even with hundreds of processes.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::time::{Duration, Instant};

use refract_core::protocol::{LinuxOwnStats, LinuxStats};

/// Clock ticks from the `cpu` line of `/proc/stat`, summed over every CPU.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CpuTimes {
    /// Every tick: user, nice, system, idle, iowait, irq, softirq and steal. Guest time
    /// is already inside user and nice, so it is not added again.
    pub total: u64,
    /// `total` less idle and iowait.
    pub busy: u64,
}

/// The `cpu` line of `/proc/stat` and how many `cpuN` lines follow it.
pub fn parse_proc_stat(text: &str) -> Option<(CpuTimes, u32)> {
    let mut times = None;
    let mut count = 0;
    for line in text.lines() {
        let mut fields = line.split_whitespace();
        match fields.next() {
            Some("cpu") => {
                let ticks: Vec<u64> = fields
                    .take(8)
                    .map(|f| f.parse().ok())
                    .collect::<Option<_>>()?;
                // Old kernels have fewer columns; idle and iowait must be there.
                if ticks.len() < 5 {
                    return None;
                }
                let total = ticks.iter().fold(0u64, |a, &t| a.saturating_add(t));
                times = Some(CpuTimes {
                    total,
                    busy: total.saturating_sub(ticks[3]).saturating_sub(ticks[4]),
                });
            }
            Some(name)
                if name
                    .strip_prefix("cpu")
                    .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit())) =>
            {
                count += 1;
            }
            _ => {}
        }
    }
    times.map(|t| (t, count))
}

/// Memory figures from `/proc/meminfo`, in bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MemInfo {
    pub total: u64,
    /// Programs' use: `MemTotal - MemFree - cache`, never below 0.
    pub used: u64,
    /// File cache: `Buffers + Cached + SReclaimable - Shmem`, never below 0.
    pub cache: u64,
}

/// [`MemInfo`] from `/proc/meminfo`. `MemTotal` and `MemFree` must be there; the cache
/// fields count as 0 when missing.
pub fn parse_meminfo(text: &str) -> Option<MemInfo> {
    let (mut total, mut free) = (None, None);
    let (mut buffers, mut cached, mut reclaimable, mut shmem) = (0, 0, 0, 0);
    for line in text.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let Some(bytes) = kb_value(value) else {
            continue;
        };
        match key.trim() {
            "MemTotal" => total = Some(bytes),
            "MemFree" => free = Some(bytes),
            "Buffers" => buffers = bytes,
            "Cached" => cached = bytes,
            "SReclaimable" => reclaimable = bytes,
            "Shmem" => shmem = bytes,
            _ => {}
        }
    }
    let total: u64 = total?;
    let cache = buffers
        .saturating_add(cached)
        .saturating_add(reclaimable)
        .saturating_sub(shmem);
    Some(MemInfo {
        total,
        used: total.saturating_sub(free?).saturating_sub(cache),
        cache,
    })
}

/// A `/proc` value such as `  123456 kB`, in bytes.
fn kb_value(value: &str) -> Option<u64> {
    let mut parts = value.split_whitespace();
    let n: u64 = parts.next()?.parse().ok()?;
    match parts.next() {
        Some("kB") => Some(n.saturating_mul(1024)),
        None => Some(n),
        Some(_) => None,
    }
}

/// What the tree walk needs from `/proc/<pid>/stat`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PidStat {
    pub ppid: u32,
    /// `utime + stime`, in clock ticks.
    pub ticks: u64,
    /// `starttime`, to tell a process from a later one given the same pid.
    pub start: u64,
}

/// [`PidStat`] from a `/proc/<pid>/stat` line. The command name in field 2 is in
/// parentheses and may itself hold spaces and parentheses, so fields are counted from
/// the last `)`.
pub fn parse_pid_stat(stat: &str) -> Option<PidStat> {
    let (_, rest) = stat.rsplit_once(')')?;
    // `rest` starts at field 3 (state).
    let fields: Vec<&str> = rest.split_whitespace().take(20).collect();
    let field = |n: usize| -> Option<u64> { fields.get(n - 3)?.parse().ok() };
    Some(PidStat {
        ppid: field(4)?.try_into().ok()?,
        ticks: field(14)?.saturating_add(field(15)?),
        start: field(22)?,
    })
}

/// `VmRSS` from `/proc/<pid>/status`, in bytes. Kernel threads and zombies have none,
/// which counts as 0.
pub fn parse_status_rss(status: &str) -> u64 {
    status
        .lines()
        .find_map(|line| kb_value(line.strip_prefix("VmRSS:")?))
        .unwrap_or(0)
}

/// `root` and every process below it, by `ppid`. A parent that is gone (its children
/// were handed to another process, or the reading is stale) or a loop in the table
/// cannot make the walk repeat or include anything not reached from `root`.
pub fn descendants(root: u32, parents: &HashMap<u32, u32>) -> Vec<u32> {
    let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
    for (&pid, &ppid) in parents {
        if pid != ppid {
            children.entry(ppid).or_default().push(pid);
        }
    }
    let mut seen = HashSet::from([root]);
    let mut tree = vec![root];
    let mut i = 0;
    while let Some(&pid) = tree.get(i) {
        i += 1;
        for &child in children.get(&pid).into_iter().flatten() {
            if seen.insert(child) {
                tree.push(child);
            }
        }
    }
    tree
}

/// A share of `total` ticks as a percentage, limited to 0–100. `None` when no time
/// passed.
fn percent(part: u64, total: u64) -> Option<f64> {
    (total > 0).then(|| (part as f64 / total as f64 * 100.0).min(100.0))
}

/// The CPU ticks of the helper's tree, by pid and start time.
pub type TreeTicks = HashMap<(u32, u64), u64>;

/// Ticks the tree used between two readings: each process in both counts its increase
/// (never below 0), and each process new since `prev` counts all of its ticks. A
/// process that exited is left out.
pub fn tree_delta(prev: &TreeTicks, cur: &TreeTicks) -> u64 {
    cur.iter()
        .map(|(key, &ticks)| match prev.get(key) {
            Some(&before) => ticks.saturating_sub(before),
            None => ticks,
        })
        .fold(0, u64::saturating_add)
}

/// A call sooner than this after the last reading gets no CPU figures and leaves that
/// reading in place: a few ticks between two calls would swing between 0 and 100 %.
pub const MIN_INTERVAL: Duration = Duration::from_secs(1);

/// One reading of everything `systemStats` reports.
#[derive(Debug, Clone, PartialEq)]
pub struct Reading {
    pub at: Instant,
    /// `None` when `/proc/stat` could not be read.
    pub cpu: Option<CpuTimes>,
    pub cpu_count: u32,
    pub mem: MemInfo,
    /// `None` when `/proc` could not be listed.
    pub tree: Option<TreeTicks>,
    pub tree_rss: u64,
}

/// What the next reading's CPU figures are measured from.
#[derive(Debug, Clone, PartialEq)]
struct Mark {
    at: Instant,
    cpu: CpuTimes,
    tree: TreeTicks,
}

/// Turns readings into [`LinuxStats`], keeping the last complete one to compare the
/// next with.
#[derive(Debug, Default)]
pub struct Sampler {
    prev: Option<Mark>,
}

impl Sampler {
    /// Reads `/proc` for this process and its descendants.
    pub fn sample(&mut self) -> LinuxStats {
        self.next(read(std::process::id()))
    }

    /// The figures for `cur` against the previous reading, which `cur` then replaces.
    /// A reading missing its CPU times or its process tree, or one taken less than
    /// [`MIN_INTERVAL`] after the previous, has no CPU figures and is not kept.
    pub fn next(&mut self, cur: Reading) -> LinuxStats {
        let soon = self
            .prev
            .as_ref()
            .is_some_and(|prev| cur.at.saturating_duration_since(prev.at) < MIN_INTERVAL);
        let (cpu_percent, own_cpu) = match (&self.prev, cur.cpu, &cur.tree) {
            (Some(prev), Some(cpu), Some(tree)) if !soon => {
                // A total that went back (it never should) counts as no time passing.
                let elapsed = cpu.total.saturating_sub(prev.cpu.total);
                (
                    percent(cpu.busy.saturating_sub(prev.cpu.busy), elapsed),
                    percent(tree_delta(&prev.tree, tree), elapsed),
                )
            }
            _ => (None, None),
        };
        let stats = LinuxStats {
            cpu_percent,
            cpu_count: cur.cpu_count,
            mem_total_bytes: cur.mem.total,
            mem_used_bytes: cur.mem.used,
            mem_cache_bytes: cur.mem.cache,
            own: LinuxOwnStats {
                cpu_percent: own_cpu,
                mem_bytes: cur.tree_rss,
            },
        };
        if let (Some(cpu), Some(tree), false) = (cur.cpu, cur.tree, soon) {
            self.prev = Some(Mark {
                at: cur.at,
                cpu,
                tree,
            });
        }
        stats
    }
}

/// A [`Reading`] from `/proc`, for `root` and its descendants. CPU times and the tree
/// are `None` when `/proc/stat` or the `/proc` listing cannot be read; anything else
/// that cannot be read counts as 0, and a process that exits during the walk is left out.
pub fn read(root: u32) -> Reading {
    let at = Instant::now();
    let (cpu, cpu_count) = match fs::read_to_string("/proc/stat")
        .ok()
        .and_then(|text| parse_proc_stat(&text))
    {
        Some((cpu, count)) => (Some(cpu), count),
        None => (None, 0),
    };
    let mem = fs::read_to_string("/proc/meminfo")
        .ok()
        .and_then(|text| parse_meminfo(&text))
        .unwrap_or_default();
    let Ok(entries) = fs::read_dir("/proc") else {
        return Reading {
            at,
            cpu,
            cpu_count,
            mem,
            tree: None,
            tree_rss: 0,
        };
    };
    let mut stats = HashMap::new();
    for entry in entries.flatten() {
        let Some(pid) = entry.file_name().to_str().and_then(|n| n.parse().ok()) else {
            continue;
        };
        if let Some(stat) = fs::read_to_string(entry.path().join("stat"))
            .ok()
            .and_then(|text| parse_pid_stat(&text))
        {
            stats.insert(pid, stat);
        }
    }
    let parents = stats.iter().map(|(&pid, s)| (pid, s.ppid)).collect();
    let mut tree = TreeTicks::new();
    let mut tree_rss = 0u64;
    for pid in descendants(root, &parents) {
        let Some(stat) = stats.get(&pid) else {
            continue;
        };
        tree.insert((pid, stat.start), stat.ticks);
        let rss = fs::read_to_string(format!("/proc/{pid}/status"))
            .map(|text| parse_status_rss(&text))
            .unwrap_or(0);
        tree_rss = tree_rss.saturating_add(rss);
    }
    Reading {
        at,
        cpu,
        cpu_count,
        mem,
        tree: Some(tree),
        tree_rss,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PROC_STAT: &str = "\
cpu  1000 50 400 8000 300 20 30 0 100 0
cpu0 500 25 200 4000 150 10 15 0 50 0
cpu1 500 25 200 4000 150 10 15 0 50 0
intr 123456 0 0
ctxt 987654
btime 1762000000
processes 4242
";

    #[test]
    fn proc_stat_gives_busy_and_total_ticks_and_the_cpu_count() {
        let (times, count) = parse_proc_stat(PROC_STAT).unwrap();
        // Guest (100) is inside user already.
        assert_eq!(times.total, 1000 + 50 + 400 + 8000 + 300 + 20 + 30);
        assert_eq!(times.busy, 1000 + 50 + 400 + 20 + 30);
        assert_eq!(count, 2);
        assert_eq!(parse_proc_stat("intr 1\n"), None);
        assert_eq!(parse_proc_stat("cpu 1 2 x 4 5\n"), None);
    }

    const MEMINFO: &str = "\
MemTotal:        8000000 kB
MemFree:          500000 kB
MemAvailable:    5000000 kB
Buffers:          100000 kB
Cached:          4000000 kB
SwapCached:            0 kB
Shmem:            200000 kB
SReclaimable:     300000 kB
SUnreclaim:        50000 kB
HugePages_Total:       0
";

    #[test]
    fn meminfo_splits_programs_from_cache() {
        let mem = parse_meminfo(MEMINFO).unwrap();
        assert_eq!(mem.total, 8_000_000 * 1024);
        assert_eq!(mem.cache, (100_000 + 4_000_000 + 300_000 - 200_000) * 1024);
        assert_eq!(
            mem.used,
            (8_000_000 - 500_000 - 4_200_000) * 1024,
            "total - free - cache"
        );
    }

    #[test]
    fn meminfo_never_goes_below_zero() {
        // More shared memory than cache, and more cache than total - free.
        let odd = "MemTotal: 1000 kB\nMemFree: 900 kB\nCached: 500 kB\nShmem: 50 kB\n";
        let mem = parse_meminfo(odd).unwrap();
        assert_eq!(mem.cache, 450 * 1024);
        assert_eq!(mem.used, 0);
        let shmem = "MemTotal: 1000 kB\nMemFree: 100 kB\nCached: 10 kB\nShmem: 50 kB\n";
        let mem = parse_meminfo(shmem).unwrap();
        assert_eq!(mem.cache, 0);
        assert_eq!(mem.used, 900 * 1024);
        assert_eq!(parse_meminfo("MemFree: 1 kB\n"), None);
    }

    #[test]
    fn pid_stat_counts_fields_after_the_last_parenthesis() {
        let stat = "4242 (odd (name) x) S 4100 4242 4100 0 -1 4194560 900 0 0 0 \
                    37 12 0 0 20 0 3 0 556677 12345678 2048 18446744073709551615";
        assert_eq!(
            parse_pid_stat(stat),
            Some(PidStat {
                ppid: 4100,
                ticks: 49,
                start: 556677
            })
        );
        assert_eq!(parse_pid_stat("4242 (cut) S 1 2 3"), None);
        assert_eq!(parse_pid_stat("no parenthesis"), None);
    }

    #[test]
    fn status_rss_is_in_bytes_and_missing_counts_as_zero() {
        let status = "Name:\trefract-agent\nVmPeak:\t  90000 kB\nVmRSS:\t   41000 kB\n";
        assert_eq!(parse_status_rss(status), 41_000 * 1024);
        assert_eq!(
            parse_status_rss("Name:\tkthreadd\nState:\tS (sleeping)\n"),
            0
        );
    }

    #[test]
    fn descendants_follow_ppid_from_the_root_only() {
        let parents = HashMap::from([
            (1, 0),
            (100, 1),
            (200, 100), // the helper's child
            (201, 200), // a grandchild
            (300, 1),   // unrelated
            // A stale parent: 400's parent is gone and was never in the tree.
            (400, 399),
        ]);
        let mut tree = descendants(100, &parents);
        tree.sort();
        assert_eq!(tree, [100, 200, 201]);
        // A root missing from the table (it exited) is still listed alone.
        assert_eq!(descendants(999, &parents), [999]);
    }

    #[test]
    fn descendants_survive_a_loop() {
        // 10 -> 11 -> 12 -> 10, and a process listed as its own parent.
        let parents = HashMap::from([(10, 12), (11, 10), (12, 11), (13, 13), (14, 13)]);
        let mut tree = descendants(10, &parents);
        tree.sort();
        assert_eq!(tree, [10, 11, 12]);
        let mut tree = descendants(13, &parents);
        tree.sort();
        assert_eq!(tree, [13, 14]);
    }

    /// A reading taken `secs` seconds after [`start`].
    fn reading_at(secs: u64, total: u64, busy: u64, tree: &[((u32, u64), u64)]) -> Reading {
        Reading {
            at: start() + Duration::from_secs(secs),
            cpu: Some(CpuTimes { total, busy }),
            cpu_count: 4,
            mem: MemInfo {
                total: 8,
                used: 2,
                cache: 3,
            },
            tree: Some(tree.iter().copied().collect()),
            tree_rss: 5,
        }
    }

    fn start() -> Instant {
        static START: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
        *START.get_or_init(Instant::now)
    }

    /// Readings two seconds apart, as the app's ticks are.
    struct Clock(u64);

    impl Clock {
        fn reading(&mut self, total: u64, busy: u64, tree: &[((u32, u64), u64)]) -> Reading {
            self.0 += 2;
            reading_at(self.0, total, busy, tree)
        }
    }

    #[test]
    fn a_call_within_a_second_has_no_cpu_and_keeps_the_last_reading() {
        let mut sampler = Sampler::default();
        sampler.next(reading_at(0, 1000, 100, &[((7, 1), 50)]));
        // 0.5 s later: a handful of ticks, which would give a wild share.
        let soon = Reading {
            at: start() + Duration::from_millis(500),
            ..reading_at(0, 1010, 110, &[((7, 1), 55)])
        };
        let stats = sampler.next(soon);
        assert_eq!((stats.cpu_percent, stats.own.cpu_percent), (None, None));
        assert_eq!(stats.mem_used_bytes, 2);
        // The next call is measured from the first reading still.
        let stats = sampler.next(reading_at(2, 1800, 300, &[((7, 1), 90)]));
        assert_eq!(stats.cpu_percent, Some(25.0));
        assert_eq!(stats.own.cpu_percent, Some(5.0));
    }

    #[test]
    fn an_unreadable_proc_gives_no_cpu_and_keeps_the_last_reading() {
        let mut sampler = Sampler::default();
        sampler.next(reading_at(0, 1000, 100, &[((7, 1), 50)]));
        let no_stat = Reading {
            cpu: None,
            ..reading_at(2, 0, 0, &[((7, 1), 60)])
        };
        let stats = sampler.next(no_stat);
        assert_eq!((stats.cpu_percent, stats.own.cpu_percent), (None, None));
        let no_listing = Reading {
            tree: None,
            tree_rss: 0,
            ..reading_at(4, 1400, 200, &[])
        };
        let stats = sampler.next(no_listing);
        assert_eq!((stats.cpu_percent, stats.own.cpu_percent), (None, None));
        // Still measured from the first reading, not from boot or an empty tree.
        let stats = sampler.next(reading_at(6, 1800, 300, &[((7, 1), 90)]));
        assert_eq!(stats.cpu_percent, Some(25.0));
        assert_eq!(stats.own.cpu_percent, Some(5.0));
    }

    #[test]
    fn the_first_call_only_primes_the_counters() {
        let mut sampler = Sampler::default();
        let first = sampler.next(Clock(0).reading(1000, 100, &[((7, 1), 50)]));
        assert_eq!(first.cpu_percent, None);
        assert_eq!(first.own.cpu_percent, None);
        assert_eq!(first.cpu_count, 4);
        assert_eq!(
            (
                first.mem_total_bytes,
                first.mem_used_bytes,
                first.mem_cache_bytes
            ),
            (8, 2, 3)
        );
        assert_eq!(first.own.mem_bytes, 5);
    }

    #[test]
    fn cpu_is_a_share_of_the_ticks_since_the_last_call() {
        let mut sampler = Sampler::default();
        let mut clock = Clock(0);
        sampler.next(clock.reading(1000, 100, &[((7, 1), 50), ((8, 2), 10)]));
        // 800 ticks pass over all CPUs, 200 of them busy. The tree: pid 7 used 30,
        // pid 8 exited, pid 9 is new with 10.
        let stats = sampler.next(clock.reading(1800, 300, &[((7, 1), 80), ((9, 3), 10)]));
        assert_eq!(stats.cpu_percent, Some(25.0));
        assert_eq!(stats.own.cpu_percent, Some(5.0));
    }

    #[test]
    fn a_reused_pid_counts_as_a_new_process() {
        let prev = TreeTicks::from([((7, 1), 500)]);
        let cur = TreeTicks::from([((7, 2), 20)]);
        assert_eq!(tree_delta(&prev, &cur), 20);
    }

    #[test]
    fn counters_that_go_back_count_as_zero() {
        let mut sampler = Sampler::default();
        let mut clock = Clock(0);
        sampler.next(clock.reading(1000, 500, &[((7, 1), 50)]));
        let stats = sampler.next(clock.reading(1800, 400, &[((7, 1), 40)]));
        assert_eq!(stats.cpu_percent, Some(0.0));
        assert_eq!(stats.own.cpu_percent, Some(0.0));
        // The total going back means no time passed.
        let stats = sampler.next(clock.reading(900, 450, &[((7, 1), 45)]));
        assert_eq!(stats.cpu_percent, None);
        assert_eq!(stats.own.cpu_percent, None);
    }

    #[test]
    fn no_time_passing_gives_none_not_nan() {
        let mut sampler = Sampler::default();
        let mut clock = Clock(0);
        sampler.next(clock.reading(1000, 100, &[]));
        let stats = sampler.next(clock.reading(1000, 100, &[]));
        assert_eq!(stats.cpu_percent, None);
        assert_eq!(stats.own.cpu_percent, None);
    }

    #[test]
    fn shares_are_limited_to_100() {
        let mut sampler = Sampler::default();
        let mut clock = Clock(0);
        sampler.next(clock.reading(1000, 100, &[]));
        // A new process with more ticks than passed (it started before the last call
        // but was not yet in the tree).
        let stats = sampler.next(clock.reading(1100, 300, &[((9, 3), 500)]));
        assert_eq!(stats.cpu_percent, Some(100.0));
        assert_eq!(stats.own.cpu_percent, Some(100.0));
    }

    #[test]
    fn this_process_is_in_its_own_reading() {
        let reading = read(std::process::id());
        assert!(reading.cpu.unwrap().total > 0);
        assert!(reading.cpu_count >= 1);
        assert!(reading.mem.total > 0);
        assert!(reading.tree_rss > 0);
        assert!(reading
            .tree
            .unwrap()
            .keys()
            .any(|&(pid, _)| pid == std::process::id()));
    }
}
