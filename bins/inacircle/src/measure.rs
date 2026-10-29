//! The wheel's measurement switch (`INACIRCLE_MEASURE=1`): a meter the
//! surface feeds, and the one stderr line it prints at exit. Without the
//! switch no meter exists, so the cost is one `getenv`.

use std::time::{Duration, Instant};

/// Hover samples kept for the percentiles. Measurement-only memory,
/// allocated once at the start (so a repaint never reallocates): 4096 x 8 B
/// = 32 KiB, a few seconds of continuous pointer motion, well inside a
/// wheel session that lasts one gesture. Past it `hover_events` still
/// counts every repaint but the percentiles describe the first
/// `hover_samples` ones (reported, not hidden), while `hover_us_max` covers
/// every event.
const HOVER_SAMPLES: usize = 4096;

/// Paint records kept for the trace. Measurement-only memory, allocated
/// once: 64 x 8 B = 512 B. A startup paints 2-3 times and a session a few
/// dozen at most (a hover repaint per pointer event is counted, not
/// recorded, once the trace is full); `paints` still counts every one.
const PAINT_TRACE: usize = 64;

/// What the wheel measured about itself. Pure bookkeeping: the caller
/// supplies every clock reading. A reading below zero (the clock failed)
/// is a marker never taken.
pub struct Meter {
    cpu0_us: i64,
    /// `CLOCK_MONOTONIC` at the start, in µs (the same clock the run's
    /// action-entry marker uses, so the two logs correlate).
    mono0_us: i64,
    wall0: Instant,
    /// CPU and wall time from the start to the first commit of the wheel
    /// (queued: the request is in the connection's buffer).
    commit: Option<(i64, Duration)>,
    /// The same, to the first successful flush after that commit.
    flush: Option<(i64, Duration)>,
    hover_us: Vec<u64>,
    /// The longest hover repaint over every event, kept or not: the
    /// samples are capped, the maximum must not be.
    hover_max_us: u64,
    /// The first paints in order: phase, buffer, generation, whether the
    /// buffer was held by the compositor (it must never be).
    paint_trace: Vec<(char, u8, u32, bool)>,
    /// Every paint, recorded or not, and those onto a busy buffer.
    paints: u64,
    busy_paints: u64,
    /// Every hover repaint, kept or not.
    hover_count: u64,
    idle_wakeups: u64,
    /// Highest proportional set size sampled, in kB (-1 before a sample).
    pss_kb: i64,
    /// Inputs fed to the machine (pointer, drag, key...): the per-event work.
    events: u64,
    /// CPU the meter itself spent (reading smaps for PSS), in µs.
    overhead_us: i64,
    /// Process CPU and `CLOCK_MONOTONIC` when the drop's data was in.
    drop_at: Option<(i64, i64)>,
    /// CPU from the drop's data to the stage-2 wheel's first commit.
    stage2_us: Option<i64>,
    /// The same pair when the early list came in.
    list_at: Option<(i64, i64)>,
    /// CPU from the early list to the lit level-1 wheel's first commit.
    level1_us: Option<i64>,
    /// The same pair at the choice (absolute, like the run's marks).
    choice_at: Option<(i64, i64)>,
}

/// A CPU reading relative to the start, or -1 (never taken) if either
/// reading failed: two failed readings must not subtract to a valid 0.
fn since(cpu_us: i64, cpu0_us: i64) -> i64 {
    if cpu_us < 0 || cpu0_us < 0 {
        -1
    } else {
        cpu_us - cpu0_us
    }
}

/// A pair of readings, or `None` if either failed.
fn pair(cpu_us: i64, mono_us: i64) -> Option<(i64, i64)> {
    (cpu_us >= 0 && mono_us >= 0).then_some((cpu_us, mono_us))
}

impl Meter {
    /// Starts now, with the process's CPU time so far as `cpu0_us` and
    /// `CLOCK_MONOTONIC` as `mono0_us`.
    pub fn new(cpu0_us: i64, mono0_us: i64, wall0: Instant) -> Meter {
        Meter {
            cpu0_us,
            mono0_us,
            wall0,
            commit: None,
            flush: None,
            hover_us: Vec::with_capacity(HOVER_SAMPLES),
            hover_max_us: 0,
            paint_trace: Vec::with_capacity(PAINT_TRACE),
            paints: 0,
            busy_paints: 0,
            hover_count: 0,
            idle_wakeups: 0,
            pss_kb: -1,
            events: 0,
            overhead_us: 0,
            drop_at: None,
            stage2_us: None,
            list_at: None,
            level1_us: None,
            choice_at: None,
        }
    }

    /// The mapped wheel's first `wl_surface.commit` request (queued, not
    /// yet sent); later calls are ignored.
    pub fn committed(&mut self, cpu_us: i64, now: Instant) {
        if self.commit.is_none() {
            self.commit = Some((since(cpu_us, self.cpu0_us), now.duration_since(self.wall0)));
        }
    }

    /// Whether the first commit is queued and no flush has sent it yet.
    pub fn awaiting_flush(&self) -> bool {
        self.commit.is_some() && self.flush.is_none()
    }

    /// A flush of the connection succeeded: the first one after the first
    /// commit is when that commit left the process. A flush before any
    /// commit is ignored.
    pub fn flushed(&mut self, cpu_us: i64, now: Instant) {
        if self.awaiting_flush() {
            self.flush = Some((since(cpu_us, self.cpu0_us), now.duration_since(self.wall0)));
        }
    }

    /// One hover repaint took `d`. Never allocates: the samples are
    /// capped at [`HOVER_SAMPLES`].
    pub fn hover(&mut self, d: Duration) {
        self.hover_count += 1;
        let us = d.as_micros() as u64;
        self.hover_max_us = self.hover_max_us.max(us);
        if self.hover_us.len() < HOVER_SAMPLES {
            self.hover_us.push(us);
        }
    }

    /// One paint into buffer `slot` at wheel `generation`. `phase`: `S`
    /// the startup paint of buffer 0, `F` a full paint and `H` a hover
    /// paint inside a render, `P` a prepaint. Never allocates.
    pub fn paint(&mut self, phase: char, slot: usize, generation: u32, busy: bool) {
        self.paints += 1;
        self.busy_paints += u64::from(busy);
        if self.paint_trace.len() < PAINT_TRACE {
            self.paint_trace.push((phase, slot as u8, generation, busy));
        }
    }

    #[cfg(test)]
    pub fn hover_capacity(&self) -> usize {
        self.hover_us.capacity()
    }

    /// A proportional-set-size sample in kB; the highest one is kept. The
    /// kernel tracks no PSS peak, so the surface samples after full paints.
    pub fn pss(&mut self, kb: i64) {
        self.pss_kb = self.pss_kb.max(kb);
    }

    /// The drop's data is in (stage 2 starts).
    pub fn dropped(&mut self, cpu_us: i64, mono_us: i64) {
        if self.drop_at.is_none() {
            self.drop_at = pair(cpu_us, mono_us);
        }
    }

    /// The early list (read at `enter`) is in; the first one counts.
    pub fn listed(&mut self, cpu_us: i64, mono_us: i64) {
        if self.list_at.is_none() {
            self.list_at = pair(cpu_us, mono_us);
        }
    }

    #[cfg(test)]
    pub fn has_list(&self) -> bool {
        self.list_at.is_some()
    }

    #[cfg(test)]
    pub fn list_cpu(&self) -> Option<i64> {
        self.list_at.map(|(c, _)| c)
    }

    /// A new wheel's first commit: after the early list, level 1 shown;
    /// after a drop, stage 2 shown.
    pub fn swapped(&mut self, cpu_us: i64) {
        if let (Some((l, _)), None, None) = (self.list_at, self.level1_us, self.drop_at) {
            self.level1_us = Some(cpu_us - l);
        }
        if let (Some((d, _)), None) = (self.drop_at, self.stage2_us) {
            self.stage2_us = Some(cpu_us - d);
        }
    }

    /// The user chose a slice.
    pub fn chose(&mut self, cpu_us: i64, mono_us: i64) {
        if self.choice_at.is_none() {
            self.choice_at = pair(cpu_us, mono_us);
        }
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

    /// The report line (without a newline). `-1` marks a legacy value that
    /// was never taken (no commit) or is unavailable; every marker added
    /// since, and the hover percentiles, are left out of the line when
    /// missing, never written as 0.
    /// `exit_cpu_us` is the process's CPU when the surface is gone, so the
    /// run's own share is the later `measure self_cpu_us` minus it.
    pub fn report(&self, vm_hwm_kb: i64, exit_cpu_us: i64) -> String {
        let (cpu, wall) = self
            .commit
            .map_or((-1, -1), |(c, w)| (c, w.as_micros() as i64));
        let mut sorted = self.hover_us.clone();
        sorted.sort_unstable();
        let mut line = format!(
            "measure wheel pid={} start_abs_cpu_us={} start_abs_wall_us={} \
             main_to_commit_cpu_us={cpu} main_to_commit_wall_us={wall}",
            std::process::id(),
            self.cpu0_us,
            self.mono0_us
        );
        if let Some((c, w)) = self.flush {
            line += &format!(
                " main_to_flush_cpu_us={c} main_to_flush_wall_us={}",
                w.as_micros()
            );
        }
        line += &format!(
            " paints={} busy_paints={} paint_trace=",
            self.paints, self.busy_paints
        );
        for (i, &(phase, slot, g, busy)) in self.paint_trace.iter().enumerate() {
            if i > 0 {
                line.push(',');
            }
            line += &format!("{phase}{slot}g{g}{}", if busy { "!" } else { "" });
        }
        if self.paint_trace.is_empty() {
            line.push('-');
        }
        line += &format!(" hover_events={}", self.hover_count);
        if let (Some(_), n) = (sorted.last(), sorted.len()) {
            let max = self.hover_max_us;
            // Nearest rank: the ceil(p * n)-th smallest.
            let rank = |pct: usize| sorted[(pct * n).div_ceil(100).max(1) - 1];
            line += &format!(
                " hover_samples={n} hover_us_p50={} hover_us_p95={} hover_us_max={max}",
                rank(50),
                rank(95)
            );
            // In event order: the maximum is usually one of the first.
            let first: Vec<String> = self.hover_us.iter().take(4).map(u64::to_string).collect();
            line += &format!(" hover_us_first={}", first.join(","));
        }
        line += &format!(
            " idle_wakeups={} events={} wheel_cpu_us={} meter_cpu_us={} drop_to_stage2_cpu_us={} list_to_level1_cpu_us={}",
            self.idle_wakeups,
            self.events,
            if exit_cpu_us < 0 {
                -1
            } else {
                exit_cpu_us - self.cpu0_us
            },
            self.overhead_us,
            self.stage2_us.unwrap_or(-1),
            self.level1_us.unwrap_or(-1),
        );
        for (name, at) in [
            ("list", self.list_at),
            ("drop", self.drop_at),
            ("choice", self.choice_at),
        ] {
            if let Some((c, w)) = at {
                line += &format!(" {name}_abs_cpu_us={c} {name}_abs_wall_us={w}");
            }
        }
        line += &format!(" pss_kb={} vm_hwm_kb={vm_hwm_kb}", self.pss_kb);
        line
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

    /// The value of `key` in a report line, if the key is present.
    fn field<'a>(r: &'a str, key: &str) -> Option<&'a str> {
        r.split_whitespace()
            .find_map(|w| w.strip_prefix(key)?.strip_prefix('='))
    }

    #[test]
    fn report_aggregates_the_run() {
        let t0 = Instant::now();
        let mut m = Meter::new(1_000, 50_000, t0);
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
        m.dropped(3_000, 53_000);
        m.dropped(9_000, 59_000);
        m.swapped(3_400);
        m.swapped(9_999);
        m.chose(3_800, 53_800);
        let r = m.report(2_840, 4_000);
        assert_eq!(
            field(&r, "pid"),
            Some(std::process::id().to_string().as_str())
        );
        assert_eq!(field(&r, "main_to_commit_cpu_us"), Some("700"));
        assert_eq!(field(&r, "main_to_commit_wall_us"), Some("2500"));
        assert_eq!(field(&r, "hover_events"), Some("5"));
        assert_eq!(field(&r, "hover_us_p50"), Some("30"));
        assert_eq!(field(&r, "hover_us_p95"), Some("50"));
        assert_eq!(field(&r, "hover_us_max"), Some("50"));
        assert_eq!(field(&r, "idle_wakeups"), Some("2"));
        assert_eq!(field(&r, "events"), Some("3"));
        assert_eq!(field(&r, "wheel_cpu_us"), Some("3000"));
        assert_eq!(field(&r, "drop_to_stage2_cpu_us"), Some("400"));
        assert_eq!(field(&r, "pss_kb"), Some("2100"));
        assert_eq!(field(&r, "vm_hwm_kb"), Some("2840"));
        // Start, drop and choice: absolute CPU and CLOCK_MONOTONIC wall.
        assert_eq!(field(&r, "start_abs_cpu_us"), Some("1000"));
        assert_eq!(field(&r, "start_abs_wall_us"), Some("50000"));
        assert_eq!(field(&r, "drop_abs_cpu_us"), Some("3000"));
        assert_eq!(field(&r, "drop_abs_wall_us"), Some("53000"));
        assert_eq!(field(&r, "choice_abs_cpu_us"), Some("3800"));
        assert_eq!(field(&r, "choice_abs_wall_us"), Some("53800"));
    }

    /// The early list's CPU runs to the first commit after it, once; the
    /// drop's stage-2 swap is a separate number.
    #[test]
    fn list_to_level1_is_the_first_swap_after_the_list() {
        let t0 = Instant::now();
        let mut m = Meter::new(0, 0, t0);
        m.swapped(500); // no list yet: nothing
        m.listed(1_000, 7_000);
        m.listed(1_100, 7_100);
        m.swapped(1_400);
        m.swapped(1_900);
        m.dropped(2_000, 8_000);
        m.swapped(2_300);
        let r = m.report(0, -1);
        assert!(r.contains("list_to_level1_cpu_us=400"), "{r}");
        assert!(r.contains("drop_to_stage2_cpu_us=300"), "{r}");
        // List and drop each keep their own CPU and wall pair.
        assert_eq!(field(&r, "list_abs_cpu_us"), Some("1000"), "{r}");
        assert_eq!(field(&r, "list_abs_wall_us"), Some("7000"), "{r}");
        assert_eq!(field(&r, "drop_abs_cpu_us"), Some("2000"), "{r}");
        assert_eq!(field(&r, "drop_abs_wall_us"), Some("8000"), "{r}");
    }

    /// A marker never taken is absent from the line, not zero; the
    /// `-1` sentinel stays for the legacy fields scripts already parse.
    #[test]
    fn report_of_a_wheel_that_never_mapped() {
        let m = Meter::new(0, 0, Instant::now());
        let r = m.report(-1, -1);
        for key in [
            "main_to_flush_cpu_us",
            "main_to_flush_wall_us",
            "list_abs_cpu_us",
            "list_abs_wall_us",
            "drop_abs_cpu_us",
            "drop_abs_wall_us",
            "choice_abs_cpu_us",
            "choice_abs_wall_us",
            "hover_us_p50",
            "hover_us_p95",
            "hover_us_max",
        ] {
            assert_eq!(field(&r, key), None, "{key} in {r}");
        }
        assert_eq!(field(&r, "main_to_commit_cpu_us"), Some("-1"), "{r}");
        assert_eq!(field(&r, "hover_events"), Some("0"), "{r}");
    }

    #[test]
    fn hover_without_events_has_no_percentile() {
        let mut m = Meter::new(0, 0, Instant::now());
        let r = m.report(0, -1);
        assert_eq!(field(&r, "hover_events"), Some("0"));
        assert_eq!(field(&r, "hover_us_p50"), None, "{r}");
        assert_eq!(field(&r, "hover_us_max"), None, "{r}");
        m.hover(ms(7));
        let r = m.report(0, -1);
        assert_eq!(field(&r, "hover_us_p50"), Some("7"), "{r}");
    }

    /// The queued commit and the flush that sent it are separate points;
    /// a flush before any commit does not count, and the first flush
    /// after the commit wins.
    #[test]
    fn queued_commit_and_flush_are_separate_markers() {
        let t0 = Instant::now();
        let mut m = Meter::new(100, 0, t0);
        assert!(!m.awaiting_flush());
        m.flushed(150, t0 + ms(40)); // before any commit: ignored
        m.committed(300, t0 + ms(900));
        assert!(m.awaiting_flush());
        m.flushed(380, t0 + ms(1_100));
        assert!(!m.awaiting_flush());
        m.flushed(999, t0 + ms(9_000));
        let r = m.report(0, -1);
        assert_eq!(field(&r, "main_to_commit_cpu_us"), Some("200"), "{r}");
        assert_eq!(field(&r, "main_to_commit_wall_us"), Some("900"), "{r}");
        assert_eq!(field(&r, "main_to_flush_cpu_us"), Some("280"), "{r}");
        assert_eq!(field(&r, "main_to_flush_wall_us"), Some("1100"), "{r}");
    }

    /// Hover samples are kept up to a cap; the count goes on, the
    /// storage does not grow, and nothing reallocates in the repaint.
    #[test]
    fn hover_samples_are_bounded() {
        let mut m = Meter::new(0, 0, Instant::now());
        let cap = m.hover_capacity();
        for _ in 0..cap + 100 {
            m.hover(ms(5));
        }
        assert_eq!(m.hover_capacity(), cap, "no growth");
        let r = m.report(0, -1);
        assert_eq!(
            field(&r, "hover_events"),
            Some((cap + 100).to_string().as_str())
        );
        assert_eq!(field(&r, "hover_samples"), Some(cap.to_string().as_str()));
    }

    /// The maximum covers every hover, not only the kept samples: a spike
    /// after the cap must show.
    #[test]
    fn hover_max_covers_events_past_the_sample_cap() {
        let mut m = Meter::new(0, 0, Instant::now());
        let cap = m.hover_capacity();
        for _ in 0..cap {
            m.hover(ms(5));
        }
        m.hover(ms(900)); // the cap + 1st event, the largest
        let r = m.report(0, -1);
        assert_eq!(field(&r, "hover_us_max"), Some("900"), "{r}");
        assert_eq!(field(&r, "hover_us_p95"), Some("5"), "{r}");
        assert_eq!(field(&r, "hover_samples"), Some(cap.to_string().as_str()));
    }

    /// Two failed CPU readings are not a 0 us timing: the marker stays -1.
    #[test]
    fn failed_cpu_readings_do_not_subtract_to_zero() {
        let t0 = Instant::now();
        let mut m = Meter::new(-1, 0, t0);
        m.committed(-1, t0 + ms(10));
        m.flushed(-1, t0 + ms(20));
        let r = m.report(0, -1);
        assert_eq!(field(&r, "main_to_commit_cpu_us"), Some("-1"), "{r}");
        assert_eq!(field(&r, "main_to_flush_cpu_us"), Some("-1"), "{r}");
        let mut m = Meter::new(100, 0, t0);
        m.committed(-1, t0 + ms(10));
        let r = m.report(0, -1);
        assert_eq!(field(&r, "main_to_commit_cpu_us"), Some("-1"), "{r}");
    }

    /// A reading the clock could not give (negative) is a missing marker.
    #[test]
    fn unavailable_clock_readings_are_absent() {
        let mut m = Meter::new(0, 0, Instant::now());
        m.chose(-1, -1);
        let r = m.report(0, -1);
        assert_eq!(field(&r, "choice_abs_cpu_us"), None, "{r}");
        assert_eq!(field(&r, "choice_abs_wall_us"), None, "{r}");
    }

    #[test]
    fn p50_of_an_even_count_is_the_lower_middle() {
        let mut m = Meter::new(0, 0, Instant::now());
        for us in [4, 1, 3, 2] {
            m.hover(ms(us));
        }
        assert!(m.report(0, -1).contains("hover_us_p50=2 "));
        assert_eq!(field(&m.report(0, -1), "hover_us_max"), Some("4"));
    }

    #[test]
    fn a_pair_needs_both_readings() {
        assert_eq!(pair(1, 2), Some((1, 2)));
        assert_eq!(pair(0, 0), Some((0, 0)));
        assert_eq!(pair(-1, 2), None);
        assert_eq!(pair(1, -1), None);
        assert_eq!(pair(-1, -1), None);
    }

    /// Paints are counted exactly (busy ones apart) and the trace keeps the
    /// first `PAINT_TRACE` of them. Kills `+=` -> `*=` / `-=` and `<` ->
    /// `<=` in `Meter::paint`.
    #[test]
    fn paints_are_counted_and_the_trace_is_bounded() {
        let mut m = Meter::new(0, 0, Instant::now());
        for i in 0..(PAINT_TRACE + 5) {
            m.paint('F', 0, 1, i % 2 == 0);
        }
        assert_eq!(m.paints, (PAINT_TRACE + 5) as u64);
        assert_eq!(m.busy_paints, (PAINT_TRACE as u64 + 5).div_ceil(2));
        assert_eq!(m.paint_trace.len(), PAINT_TRACE);
        assert_eq!(m.paint_trace.capacity(), PAINT_TRACE);
    }
}
