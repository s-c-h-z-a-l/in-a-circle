//! The wheel's measurement switch (`INACIRCLE_MEASURE=1`): a meter the
//! surface feeds, and the one stderr line it prints at exit. Without the
//! switch no meter exists, so the cost is one `getenv`.

use std::time::{Duration, Instant};

/// What the wheel measured about itself. Pure bookkeeping: the caller
/// supplies every clock reading.
pub struct Meter {
    cpu0_us: i64,
    wall0: Instant,
    /// CPU and wall time from the start to the first commit of the wheel.
    commit: Option<(i64, Duration)>,
    hover_us: Vec<u64>,
    idle_wakeups: u64,
    /// Highest proportional set size sampled, in kB (-1 before a sample).
    pss_kb: i64,
    /// Inputs fed to the machine (pointer, drag, key...): the per-event work.
    events: u64,
    /// CPU the meter itself spent (reading smaps for PSS), in µs.
    overhead_us: i64,
}

impl Meter {
    /// Starts now, with the process's CPU time so far as `cpu0_us`.
    pub fn new(cpu0_us: i64, wall0: Instant) -> Meter {
        Meter {
            cpu0_us,
            wall0,
            commit: None,
            hover_us: Vec::new(),
            idle_wakeups: 0,
            pss_kb: -1,
            events: 0,
            overhead_us: 0,
        }
    }

    /// The mapped wheel's first `wl_surface.commit`; later calls are ignored.
    pub fn committed(&mut self, cpu_us: i64, now: Instant) {
        if self.commit.is_none() {
            self.commit = Some((cpu_us - self.cpu0_us, now.duration_since(self.wall0)));
        }
    }

    /// One hover repaint took `d`.
    pub fn hover(&mut self, d: Duration) {
        self.hover_us.push(d.as_micros() as u64);
    }

    /// A proportional-set-size sample in kB; the highest one is kept. The
    /// kernel tracks no PSS peak, so the surface samples after full paints.
    pub fn pss(&mut self, kb: i64) {
        self.pss_kb = self.pss_kb.max(kb);
    }

    /// CPU the meter spent on itself, to subtract from the wheel's.
    pub fn overhead(&mut self, us: i64) {
        self.overhead_us += us.max(0);
    }

    /// One input fed to the machine.
    pub fn event(&mut self) {
        self.events += 1;
    }

    /// `poll` returned with nothing to do while the wheel waited.
    pub fn idle_wakeup(&mut self) {
        self.idle_wakeups += 1;
    }

    /// The report line (without a newline). `-1` marks a value that was
    /// never taken (no commit) or is unavailable.
    /// `exit_cpu_us` is the process's CPU when the surface is gone, so the
    /// run's own share is the later `measure self_cpu_us` minus it.
    pub fn report(&self, vm_hwm_kb: i64, exit_cpu_us: i64) -> String {
        let (cpu, wall) = self
            .commit
            .map_or((-1, -1), |(c, w)| (c, w.as_micros() as i64));
        let mut sorted = self.hover_us.clone();
        sorted.sort_unstable();
        let p50 = sorted.get(sorted.len().saturating_sub(1) / 2).copied();
        let max = sorted.last().copied();
        format!(
            "measure wheel main_to_commit_cpu_us={cpu} main_to_commit_wall_us={wall} \
             hover_events={} hover_us_p50={} hover_us_max={} idle_wakeups={} events={} wheel_cpu_us={} meter_cpu_us={} pss_kb={} vm_hwm_kb={vm_hwm_kb}",
            sorted.len(),
            p50.unwrap_or(0),
            max.unwrap_or(0),
            self.idle_wakeups,
            self.events,
            if exit_cpu_us < 0 {
                -1
            } else {
                exit_cpu_us - self.cpu0_us
            },
            self.overhead_us,
            self.pss_kb
        )
    }
}

/// Proportional set size (`Pss`) in kB from `/proc/self/smaps_rollup`, -1
/// if unavailable. Shared pages (libc, the loader) count by their share, so
/// this is the budget's measure; `VmHWM` counts them whole.
pub fn pss_kb() -> i64 {
    std::fs::read_to_string("/proc/self/smaps_rollup")
        .ok()
        .and_then(|s| {
            s.lines()
                .find_map(|l| l.strip_prefix("Pss:"))
                .and_then(|v| v.split_whitespace().next()?.parse().ok())
        })
        .unwrap_or(-1)
}

/// Peak RSS (`VmHWM`) in kB from `/proc/self/status`, -1 if unavailable.
pub fn vm_hwm_kb() -> i64 {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find_map(|l| l.strip_prefix("VmHWM:"))
                .and_then(|v| v.split_whitespace().next()?.parse().ok())
        })
        .unwrap_or(-1)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(n: u64) -> Duration {
        Duration::from_micros(n)
    }

    #[test]
    fn report_aggregates_the_run() {
        let t0 = Instant::now();
        let mut m = Meter::new(1_000, t0);
        for us in [30, 10, 20, 50, 40] {
            m.hover(ms(us));
        }
        m.idle_wakeup();
        m.idle_wakeup();
        m.committed(1_700, t0 + ms(2_500));
        // A later commit does not move the first one.
        m.committed(9_999, t0 + ms(9_999));
        m.pss(2_100);
        m.pss(1_900);
        m.event();
        m.event();
        m.event();
        assert_eq!(
            m.report(2_840, 4_000),
            "measure wheel main_to_commit_cpu_us=700 main_to_commit_wall_us=2500 \
             hover_events=5 hover_us_p50=30 hover_us_max=50 idle_wakeups=2 events=3 wheel_cpu_us=3000 meter_cpu_us=0 pss_kb=2100 vm_hwm_kb=2840"
        );
    }

    #[test]
    fn report_of_a_wheel_that_never_mapped() {
        let m = Meter::new(0, Instant::now());
        assert_eq!(
            m.report(-1, -1),
            "measure wheel main_to_commit_cpu_us=-1 main_to_commit_wall_us=-1 \
             hover_events=0 hover_us_p50=0 hover_us_max=0 idle_wakeups=0 events=0 wheel_cpu_us=-1 meter_cpu_us=0 pss_kb=-1 vm_hwm_kb=-1"
        );
    }

    #[test]
    fn p50_of_an_even_count_is_the_lower_middle() {
        let mut m = Meter::new(0, Instant::now());
        for us in [4, 1, 3, 2] {
            m.hover(ms(us));
        }
        assert!(m.report(0, -1).contains("hover_us_p50=2 hover_us_max=4"));
    }
}
