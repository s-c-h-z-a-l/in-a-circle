//! Process-level measurements read from /proc and getrusage.

use std::fs;

#[derive(Debug, Clone, Copy, Default)]
pub struct Mem {
    pub rss_kb: u64,
    pub pss_kb: u64,
    pub anon_kb: u64,
}

pub fn mem() -> Mem {
    let mut m = Mem::default();
    let Ok(s) = fs::read_to_string("/proc/self/smaps_rollup") else {
        return m;
    };
    for line in s.lines() {
        let mut it = line.split_whitespace();
        let (Some(k), Some(v)) = (it.next(), it.next()) else {
            continue;
        };
        let v: u64 = v.parse().unwrap_or(0);
        match k {
            "Rss:" => m.rss_kb = v,
            "Pss:" => m.pss_kb = v,
            "Anonymous:" => m.anon_kb = v,
            _ => {}
        }
    }
    m
}

/// Sum of voluntary + involuntary context switches over all threads.
pub fn ctx_switches() -> u64 {
    let Ok(dir) = fs::read_dir("/proc/self/task") else {
        return 0;
    };
    dir.flatten()
        .filter_map(|e| fs::read_to_string(e.path().join("status")).ok())
        .map(|s| {
            s.lines()
                .filter(|l| l.contains("ctxt_switches:"))
                .filter_map(|l| l.split_whitespace().nth(1)?.parse::<u64>().ok())
                .sum::<u64>()
        })
        .sum()
}

pub fn threads() -> usize {
    fs::read_dir("/proc/self/task").map_or(0, |d| d.count())
}

/// User + system CPU time of the whole process (all threads), in microseconds.
pub fn cpu_us() -> u64 {
    // SAFETY: getrusage only writes into the zeroed struct we pass.
    let ru = unsafe {
        let mut ru: libc::rusage = std::mem::zeroed();
        libc::getrusage(libc::RUSAGE_SELF, &mut ru);
        ru
    };
    let tv = |t: libc::timeval| t.tv_sec as u64 * 1_000_000 + t.tv_usec as u64;
    tv(ru.ru_utime) + tv(ru.ru_stime)
}

/// CLOCK_REALTIME in nanoseconds, for latency measured across processes.
pub fn realtime_ns() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos())
}

pub fn report_mem(tag: &str) {
    let m = mem();
    println!(
        "M {tag} rss_kb={} pss_kb={} anon_kb={} threads={}",
        m.rss_kb,
        m.pss_kb,
        m.anon_kb,
        threads()
    );
}
