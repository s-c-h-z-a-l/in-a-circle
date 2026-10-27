//! Desktop notifications for a run: the texts (pure functions) and the
//! [`Notifier`], which shows "running" with Cancel while the run goes on,
//! the result with Undo at the end, and waits a bounded time for the click.

use std::os::fd::{AsRawFd, RawFd};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use dbus::Error;
use dbus::notifications::{Event, Notice, Notifications};
use engine::exec::{CancelToken, Durability, Observer, Outcome, PublicationReason, Wake};
use engine::journal::{self, JobId, UndoResult};

/// A run that takes longer than this gets a "running" notification.
pub const RUNNING_AFTER: Duration = Duration::from_secs(1);
/// At most four progress updates a second.
const PROGRESS_EVERY: Duration = Duration::from_millis(250);
/// How long the final notification offers Undo.
pub const UNDO_WAIT: Duration = Duration::from_secs(10);
/// Connect, authenticate and `Hello` together.
const CONNECT_WAIT: Duration = Duration::from_millis(300);
/// A call while the run goes on: the supervisor waits for it.
const QUICK_WAIT: Duration = Duration::from_millis(100);
/// The whole open while the run goes on.
const QUICK_OPEN: Duration = Duration::from_millis(500);
/// Frames one wake reads before it hands back to the supervisor.
const WAKE_FRAMES: usize = 256;
/// Events about our notification in one wake past which the server is
/// taken as broken.
const WAKE_EVENTS: usize = 64;
/// Wakes in a row that spend all of `WAKE_FRAMES` past which the server is
/// taken as broken (a flood of signals about other notifications).
const SPENT_WAKES: u32 = 4;
/// A call once the run is over.
const CALL_WAIT: Duration = Duration::from_millis(500);

/// The most characters in a summary or in one body line.
const MAX_CHARS: usize = 200;
/// Problem lines in a body before `… and <r> more`.
const PROBLEM_LINES: usize = 3;
/// Outputs and problems a [`Summary`] keeps: enough for every text.
const KEEP_DONE: usize = 2;
const KEEP_PROBLEMS: usize = 4;

/// What the final notification says, in memory that does not grow with
/// the number of inputs.
#[derive(Debug, Default, Clone)]
pub struct Summary {
    /// The first outputs: `(input, output)`.
    pub done: Vec<(PathBuf, PathBuf)>,
    /// Inputs with an output.
    pub done_count: usize,
    /// The first problems, `(file name, reason)`, failures before skips.
    pub problems: Vec<(String, String)>,
    pub skipped: usize,
    pub failed: usize,
    pub cancelled: bool,
    pub lossy: bool,
    pub sandboxed: bool,
    /// An output went to the download folder (the source folder was not
    /// writable).
    pub to_downloads: bool,
    /// The first inputs published where it could not be confirmed:
    /// `(file name, why, op)`.
    pub unknown: Vec<(String, String, String)>,
    /// Inputs (not events) published where it could not be confirmed
    /// (sticky: no Undo).
    pub unknown_count: usize,
    /// The inputs of `unknown`, so a repeated event for one counts once.
    unknown_inputs: Vec<PathBuf>,
}

impl Summary {
    /// Keeps what the texts need from one outcome. The counts here are per
    /// outcome; [`Summary::set_counts`] replaces them with the per-input ones.
    pub fn record(&mut self, o: &Outcome) {
        match o {
            Outcome::Done { input, output, .. } => {
                if self.done.len() < KEEP_DONE {
                    self.done.push((input.clone(), output.clone()));
                }
                // Several outputs of one input go into a folder next to it.
                let source = input.parent().unwrap_or(Path::new("/"));
                if !output.starts_with(source) {
                    self.to_downloads = true;
                }
                self.done_count += 1;
            }
            Outcome::Failed { input, reason } => {
                // Failures stay ahead of skips: the first `failed` entries
                // are failures.
                let at = self.failed.min(self.problems.len());
                if at < KEEP_PROBLEMS {
                    self.problems.insert(at, (name_of(input), reason.clone()));
                    self.problems.truncate(KEEP_PROBLEMS);
                }
                self.failed += 1;
            }
            Outcome::Skipped { input, reason } => {
                if self.problems.len() < KEEP_PROBLEMS {
                    self.problems.push((name_of(input), reason.clone()));
                }
                self.skipped += 1;
            }
            Outcome::PublishedUnknown {
                input,
                location,
                reason,
                durability,
                ..
            } => {
                if self.unknown_inputs.contains(input) {
                    return;
                }
                if self.unknown.len() < KEEP_PROBLEMS {
                    self.unknown_inputs.push(input.clone());
                    let mut why = unknown_reason(*reason);
                    if let Durability::Unconfirmed(d) = durability {
                        why.push_str(&format!("; not confirmed on disk: {d}"));
                    }
                    let op = format!("{:016x}", location.anchor.op.0);
                    self.unknown.push((name_of(input), why, op));
                }
                self.unknown_count += 1;
            }
            Outcome::Warned { .. } => {}
        }
    }

    /// `(done, skipped, failed, unknown)` inputs, as the batch rule counts
    /// them (`unknown`: the inputs of those done that are not confirmed).
    pub fn set_counts(&mut self, (done, skipped, failed, unknown): (usize, usize, usize, usize)) {
        self.unknown_count = unknown;
        self.done_count = done;
        self.skipped = skipped;
        self.failed = failed;
    }
}

/// The user-facing cause of a `PublishedUnknown`.
pub fn unknown_reason(r: PublicationReason) -> String {
    match r {
        PublicationReason::AncestorChanged => {
            "converted, but its folder was moved or replaced while it was saved".to_owned()
        }
        PublicationReason::BaselineChanged => {
            "converted, but its files changed while they were saved".to_owned()
        }
    }
}

/// The "running" notification: summary, body and the progress value.
/// `fraction` is the engine's progress of the whole run.
pub fn running_text(label: &str, done: usize, total: usize, fraction: f32) -> (String, String, u8) {
    // `as` saturates and maps NaN to 0.
    let value = (fraction * 100.0).round().clamp(0.0, 100.0) as u8;
    (fit(label, false), format!("{done} / {total} files"), value)
}

/// The notification at the end of a run: summary and body. `markup`: the
/// server reads the body as markup, so `&`, `<` and `>` are escaped.
pub fn final_text(s: &Summary, markup: bool) -> (String, String) {
    let mut body = Vec::new();
    if s.lossy && s.done_count > 0 {
        body.push("Formatting was lost.".to_owned());
    }
    let summary = if s.cancelled {
        let rest = s.skipped + s.failed;
        body.push(format!("{} done, {rest} not converted", s.done_count));
        "Cancelled".to_owned()
    } else if let ([(_, out)], 1, 0, 0) = (&s.done[..], s.done_count, s.skipped, s.failed) {
        if s.to_downloads {
            body.push("Saved to Downloads (source folder is read-only)".to_owned());
        }
        name_of(out)
    } else {
        let shown = s.problems.len().min(PROBLEM_LINES);
        for (name, why) in &s.problems[..shown] {
            body.push(format!("{name}: {why}"));
        }
        let more = (s.skipped + s.failed).saturating_sub(shown);
        if more > 0 {
            body.push(format!("… and {more} more"));
        }
        if s.done_count == 0 {
            "Nothing converted".to_owned()
        } else {
            let mut t = format!("{} done", s.done_count);
            if s.skipped > 0 {
                t.push_str(&format!(", {} skipped", s.skipped));
            }
            if s.failed > 0 {
                t.push_str(&format!(", {} failed", s.failed));
            }
            t
        }
    };
    // Sticky: converted, kept, but not where (or not as) it was proved.
    if s.unknown_count > 0 {
        for (name, why, op) in s.unknown.iter().take(PROBLEM_LINES) {
            body.push(format!("{name}: {why} (op {op})"));
        }
        body.push(format!(
            "{} not confirmed where it was saved; Undo unavailable.",
            s.unknown_count
        ));
    }
    // The sandbox gate refused the run: no action started.
    if !s.sandboxed {
        body.push("Sandbox unavailable.".to_owned());
    }
    (fit(&summary, false), body_of(&body, markup))
}

/// The notification after an Undo click.
pub fn undo_text(r: &UndoResult) -> (String, String) {
    let mut body = format!("{} removed", r.removed.len());
    // Kept files by reason, in the order the reasons first appear.
    let mut reasons: Vec<(&str, usize)> = Vec::new();
    for (_, why) in &r.kept {
        match reasons.iter_mut().find(|(w, _)| w == why) {
            Some((_, n)) => *n += 1,
            None => reasons.push((why, 1)),
        }
    }
    for (why, n) in reasons {
        body.push_str(&format!(", {n} kept ({why})"));
    }
    ("Undone".to_owned(), fit(&body, false))
}

/// `lines` as a body: each fitted, joined by newlines.
pub fn body_of(lines: &[String], markup: bool) -> String {
    let fitted: Vec<String> = lines.iter().map(|l| fit(l, markup)).collect();
    fitted.join("\n")
}

/// Separators some servers break lines at, and the bidi marks, embeddings,
/// overrides and isolates that reorder what is shown
/// (`invoice\u{202E}fdp.exe` would read `invoiceexe.pdf`).
fn is_layout_control(c: char) -> bool {
    matches!(
        c,
        '\u{2028}' | '\u{2029}' // line and paragraph separators
            | '\u{200E}' | '\u{200F}' // LRM, RLM
            | '\u{202A}'..='\u{202E}' // embeddings and overrides
            | '\u{2066}'..='\u{2069}' // isolates
    )
}

/// One line of at most `MAX_CHARS` characters: control characters, line
/// and paragraph separators and bidi controls become spaces, markup is
/// escaped (an entity counts in full and is never cut), and a cut line
/// ends in `…`.
fn fit(s: &str, markup: bool) -> String {
    let piece = |c: char| -> std::borrow::Cow<'static, str> {
        match c {
            '&' if markup => "&amp;".into(),
            '<' if markup => "&lt;".into(),
            '>' if markup => "&gt;".into(),
            c if c.is_control() || is_layout_control(c) => " ".into(),
            c => c.to_string().into(),
        }
    };
    let mut out = String::new();
    let mut len = 0;
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        let p = piece(c);
        let n = p.chars().count();
        // The last piece may fill the line; otherwise leave room for `…`.
        let room = if chars.peek().is_none() {
            MAX_CHARS
        } else {
            MAX_CHARS - 1
        };
        if len + n > room {
            out.push('…');
            return out;
        }
        out.push_str(&p);
        len += n;
    }
    out
}

enum Bus {
    /// Not connected yet: the first notification connects.
    Idle,
    On(Notifications),
    /// The connection failed during the run: nothing more until the end,
    /// which opens once more for the result.
    Lost,
    /// Failed for good: no notifications for the rest of the run.
    Off,
}

/// Notifications for one run. As the run's [`Observer`] it shows a
/// "running" notification with progress and Cancel once the run takes
/// longer than [`RUNNING_AFTER`]; [`Notifier::finish`] shows the result
/// and waits for Undo. Connects to the bus only when it first has
/// something to show. A connection that fails during the run is dropped
/// and opened once more at the end, so the result still replaces the
/// "running" notification; a failure at the end, or a broken server
/// (malformed replies, floods), turns it off for the rest of the run with
/// one line on stderr. It never changes the run's result.
pub struct Notifier {
    label: String,
    total: usize,
    start: Instant,
    summary: Summary,
    bus: Bus,
    /// The notification on screen (0: none yet).
    id: u32,
    /// The user closed the "running" notification: no more updates.
    closed: bool,
    /// Inputs done and the run's fraction, and what is on screen of them.
    done: usize,
    fraction: f32,
    shown: (usize, u8),
    /// When the last progress update went out.
    sent: Instant,
    /// A Cancel click read outside `wake`, for the next `wake` to answer.
    cancel_seen: bool,
    /// `wake` answered Cancel.
    cancelled: bool,
    /// Drains in a row that spent all of `WAKE_FRAMES`.
    spent: u32,
    /// `finish` has begun: a failure now is final.
    ending: bool,
}

impl Notifier {
    pub fn new(label: &str, lossy: bool, total: usize) -> Notifier {
        let now = Instant::now();
        Notifier {
            label: label.to_owned(),
            total,
            start: now,
            summary: Summary {
                lossy,
                ..Summary::default()
            },
            bus: Bus::Idle,
            id: 0,
            closed: false,
            done: 0,
            fraction: 0.0,
            shown: (0, 0),
            sent: now,
            cancel_seen: false,
            cancelled: false,
            spent: 0,
            ending: false,
        }
    }

    /// Connects if not tried yet, or (at the end) once more after the
    /// connection was lost. During the run (`quick`) the whole open has
    /// `QUICK_OPEN`: the supervisor waits for it. At the end, connect has
    /// `CONNECT_WAIT` and every call its own time (a D-Bus activated
    /// server may need it to start).
    fn connect(&mut self, quick: bool) {
        match self.bus {
            Bus::Idle => {}
            Bus::Lost if !quick => {}
            _ => return,
        }
        let Some(addr) = dbus::session_address() else {
            return self.off(&"no unix session bus address");
        };
        let now = Instant::now();
        let opened = if quick {
            Notifications::open_within(&addr, now + QUICK_OPEN)
        } else {
            Notifications::open(&addr, now + CONNECT_WAIT)
        };
        match opened {
            Ok(n) => self.bus = Bus::On(n),
            Err(e) => self.fail(&e),
        }
    }

    /// A bus error. During the run a transport failure (timeout, I/O,
    /// hangup) only drops the connection; a broken server, or any failure
    /// at the end, is final.
    fn fail(&mut self, e: &Error) {
        let transport = matches!(
            e,
            Error::Timeout | Error::Io(_) | Error::Closed | Error::Auth
        );
        if transport && !self.ending {
            self.bus = Bus::Lost;
        } else {
            self.off(e);
        }
    }

    fn off(&mut self, e: &dyn std::fmt::Display) {
        eprintln!("inacircle: notifications unavailable: {e}");
        self.bus = Bus::Off;
    }

    /// Shows `n` if connected; an error turns notifications off.
    fn show(&mut self, n: &Notice, wait: Duration) -> Option<u32> {
        let Bus::On(bus) = &mut self.bus else {
            return None;
        };
        match bus.notify(n, Instant::now() + wait) {
            Ok(id) => Some(id),
            Err(e) => {
                self.fail(&e);
                None
            }
        }
    }

    fn value(&self) -> u8 {
        running_text("", self.done, self.total, self.fraction).2
    }

    /// The running notification is out of date and worth updating.
    fn wants_update(&self) -> bool {
        !self.closed && self.done < self.total && (self.done, self.value()) != self.shown
    }

    /// Shows or updates the "running" notification.
    fn running(&mut self) {
        self.connect(true);
        let (summary, body, value) =
            running_text(&self.label, self.done, self.total, self.fraction);
        let n = Notice {
            replaces: self.id,
            summary: &summary,
            body: &body,
            actions: &[("cancel", "Cancel")],
            progress: Some(value),
        };
        if let Some(id) = self.show(&n, QUICK_WAIT) {
            self.id = id;
            self.shown = (self.done, value);
            self.sent = Instant::now();
            // A click that came during the call is queued, and the fd no
            // longer says so: read it now.
            self.cancel_seen |= self.drain();
        }
    }

    /// Due work: the first "running" notification, or a progress update
    /// held back by the rate limit. None once every input is done: the
    /// result follows.
    fn tick(&mut self) {
        let now = Instant::now();
        let first = self.id == 0 && now >= self.start + RUNNING_AFTER;
        let update = self.id != 0 && self.wants_update() && now >= self.sent + PROGRESS_EVERY;
        if !matches!(self.bus, Bus::Idle | Bus::On(_)) || !(first || update) {
            return;
        }
        self.running();
    }

    /// Reads what the server sent, at most `WAKE_FRAMES` frames (the rest
    /// waits for the next wake); true for a Cancel click on ours. More
    /// than `WAKE_EVENTS` events about ours, or `SPENT_WAKES` drains in a
    /// row that spend every frame, is no person clicking: off.
    fn drain(&mut self) -> bool {
        let id = self.id;
        let Bus::On(n) = &mut self.bus else {
            return false;
        };
        let mut frames = WAKE_FRAMES;
        let mut events = 0;
        let mut cancel = false;
        loop {
            let e = match n.poll_event_within(id, &mut frames) {
                Ok(Some(e)) => e,
                Ok(None) if frames > 0 => {
                    self.spent = 0;
                    return cancel;
                }
                Ok(None) => {
                    self.spent += 1;
                    if self.spent >= SPENT_WAKES {
                        self.off(&"too many signals from the notification server");
                    }
                    return cancel;
                }
                Err(e) => {
                    self.fail(&e);
                    return cancel;
                }
            };
            events += 1;
            if events > WAKE_EVENTS {
                self.off(&"too many signals from the notification server");
                return cancel;
            }
            match e {
                Event::Action(key) => cancel |= key == "cancel",
                Event::Closed(_) => self.closed = true,
                Event::Timeout => {}
            }
        }
    }

    /// The result of the run with Undo, then the Undo window: an Undo click
    /// undoes `job` and says how it went; at the end of the window the
    /// notification is sent again without the button. `counts` are the
    /// run's `(done, skipped, failed)` inputs; `cancelled`: a cancel signal
    /// stopped the run (a Cancel click seen here counts too). A signal on
    /// `stop` during the window ends it. The run's history and job registry
    /// entry must be closed before this is called.
    pub fn finish(
        mut self,
        job: &JobId,
        sandboxed: bool,
        counts: (usize, usize, usize, usize),
        cancelled: bool,
        undoable: bool,
        stop: &CancelToken,
    ) {
        // Own CPU of the final notice (open + Notify), for the measurement.
        let t0 = crate::measuring().then(|| crate::cpu_us(libc::RUSAGE_SELF));
        let lap = || {
            if let Some(t0) = t0 {
                let t = crate::cpu_us(libc::RUSAGE_SELF) - t0;
                eprintln!("inacircle: measure notify_cpu_us={t}");
            }
        };
        self.ending = true;
        self.connect(false);
        let Bus::On(bus) = &self.bus else {
            return lap();
        };
        let markup = bus.has("body-markup");
        let mut s = std::mem::take(&mut self.summary);
        s.set_counts(counts);
        s.sandboxed = sandboxed;
        s.cancelled = cancelled || self.cancelled;
        let (summary, body) = final_text(&s, markup);
        let undo: &[(&str, &str)] = if s.done_count > 0 && undoable && s.unknown_count == 0 {
            &[("undo", "Undo")]
        } else {
            &[]
        };
        let n = Notice {
            replaces: self.id,
            summary: &summary,
            body: &body,
            actions: undo,
            progress: None,
        };
        let shown = self.show(&n, CALL_WAIT);
        lap();
        let Some(id) = shown else {
            return;
        };
        let Bus::On(bus) = &mut self.bus else {
            return;
        };
        if undo.is_empty() || !bus.has("actions") {
            return;
        }
        let deadline = Instant::now() + undo_wait();
        loop {
            match window_event(bus, id, deadline, stop) {
                Ok(Event::Action(key)) if key == "undo" => break,
                // A Cancel click that came too late, after the run.
                Ok(Event::Action(_)) => {}
                Ok(Event::Closed(_)) => return,
                // The window is over, or a cancel signal ended it.
                Ok(Event::Timeout) => {
                    // No dead button left in the notification centre. A
                    // resend without the button pops up again on common
                    // servers (noctalia), so the notice is closed instead.
                    if let Err(e) = bus.close(id, Instant::now() + CALL_WAIT) {
                        self.off(&e);
                    }
                    return;
                }
                Err(e) => return self.off(&e),
            }
        }
        let (summary, body) = match journal::undo(job) {
            Ok(r) => undo_text(&r),
            Err(e) => ("Undo failed".to_owned(), e.to_string()),
        };
        let body = body_of(&[body], markup);
        // In place of the result: its Undo button is spent.
        self.show(
            &Notice {
                replaces: id,
                summary: &summary,
                body: &body,
                actions: &[],
                progress: None,
            },
            CALL_WAIT,
        );
    }
}

/// The next signal about `id` like `next_event`, but `Event::Timeout` also
/// as soon as a cancel signal is pending on `stop`.
fn window_event(
    bus: &mut Notifications,
    id: u32,
    deadline: Instant,
    stop: &CancelToken,
) -> Result<Event, Error> {
    loop {
        if bus.pending() {
            if let Some(e) = bus.poll_event(id)? {
                return Ok(e);
            }
            continue;
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Ok(Event::Timeout);
        }
        let mut fds = [bus.as_raw_fd(), stop.as_raw_fd()].map(|fd| libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        });
        // Round up so the deadline has passed when poll returns.
        let ms = left.as_micros().div_ceil(1000).min(i32::MAX as u128) as i32;
        // SAFETY: fds is a valid array of two pollfds.
        let r = unsafe { libc::poll(fds.as_mut_ptr(), 2, ms) };
        if r < 0 {
            let e = std::io::Error::last_os_error();
            if e.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(Error::Io(e));
        }
        if fds[1].revents != 0 && stop.is_cancelled() {
            return Ok(Event::Timeout);
        }
        if fds[0].revents != 0
            && let Some(e) = bus.poll_event(id)?
        {
            return Ok(e);
        }
    }
}

/// [`UNDO_WAIT`], or `INACIRCLE_UNDO_WAIT_MS` (for tests).
fn undo_wait() -> Duration {
    std::env::var("INACIRCLE_UNDO_WAIT_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .map_or(UNDO_WAIT, Duration::from_millis)
}

impl Observer for Notifier {
    fn event(&mut self, o: &Outcome) {
        self.summary.record(o);
    }

    fn dir(&mut self, _path: &Path) {}

    fn progress(&mut self, done: usize, _total: usize, fraction: f32) {
        self.done = done;
        self.fraction = fraction;
        self.tick();
    }

    fn wake_fd(&self) -> Option<RawFd> {
        match &self.bus {
            Bus::On(n) if self.id != 0 => Some(n.as_raw_fd()),
            _ => None,
        }
    }

    fn wake_at(&self) -> Option<Instant> {
        match &self.bus {
            // Work the fd cannot announce: a click already read (even if
            // the bus failed right after), signals queued during a call or
            // left by a spent budget.
            _ if self.cancel_seen => Some(Instant::now()),
            Bus::Off | Bus::Lost => None,
            Bus::On(n) if n.pending() => Some(Instant::now()),
            _ if self.id == 0 => Some(self.start + RUNNING_AFTER),
            _ if self.wants_update() => Some(self.sent + PROGRESS_EVERY),
            _ => None,
        }
    }

    fn wake(&mut self) -> Wake {
        let cancel = self.drain() || std::mem::take(&mut self.cancel_seen);
        self.tick();
        if cancel {
            self.cancelled = true;
            Wake::Cancel
        } else {
            Wake::Continue
        }
    }
}

fn name_of(p: &Path) -> String {
    p.file_name()
        .map_or_else(|| p.to_string_lossy(), |n| n.to_string_lossy())
        .into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use engine::exec::FileId;

    fn done(input: &str, output: &str) -> Outcome {
        Outcome::Done {
            input: input.into(),
            output: output.into(),
            id: FileId {
                dev: 0,
                ino: 0,
                size: 0,
                mtime_ns: 0,
                ctime_ns: 0,
            },
        }
    }

    fn fail(input: &str, reason: &str) -> Outcome {
        Outcome::Failed {
            input: input.into(),
            reason: reason.into(),
        }
    }

    fn skip(input: &str, reason: &str) -> Outcome {
        Outcome::Skipped {
            input: input.into(),
            reason: reason.into(),
        }
    }

    /// A sandboxed summary of `outcomes`, counted per outcome.
    fn summary(outcomes: &[Outcome]) -> Summary {
        let mut s = Summary {
            sandboxed: true,
            ..Summary::default()
        };
        for o in outcomes {
            s.record(o);
        }
        s
    }

    fn lines(body: &str) -> Vec<&str> {
        if body.is_empty() {
            Vec::new()
        } else {
            body.split('\n').collect()
        }
    }

    #[test]
    fn running_text_counts_files() {
        // The value is the engine's fraction of the whole run, so it moves
        // within one long input too.
        assert_eq!(
            running_text("Markdown", 1, 3, 0.456),
            ("Markdown".into(), "1 / 3 files".into(), 46)
        );
        assert_eq!(running_text("Markdown", 0, 1, 0.5).2, 50);
        assert_eq!(running_text("Markdown", 0, 0, f32::NAN).2, 0);
        assert_eq!(running_text("Markdown", 5, 5, 1.7).2, 100);
        assert_eq!(running_text("Markdown", 0, 5, -0.2).2, 0);
    }

    #[test]
    fn published_unknown_is_a_sticky_line_without_undo() {
        use engine::exec::{AnchorHint, CheckPoint, GroupId, LocationEvidence, OperationId};
        let unknown = Outcome::PublishedUnknown {
            input: "/d/b.txt".into(),
            group: GroupId(1),
            location: LocationEvidence {
                path_at_check: None,
                anchor: AnchorHint {
                    parent_dev: 1,
                    parent_ino: 2,
                    object_dev: 1,
                    object_ino: 3,
                    leaf: "b".into(),
                    op: OperationId(0xcd),
                },
                checked_at: CheckPoint::PostPublish,
            },
            reason: PublicationReason::AncestorChanged,
            durability: Durability::Unconfirmed("folder sync failed".into()),
        };
        let mut s = summary(&[done("/d/a.txt", "/d/a/a.md"), unknown]);
        s.set_counts((2, 0, 0, 1));
        let (summary, body) = final_text(&s, false);
        assert_eq!(summary, "2 done");
        assert_eq!(
            lines(&body),
            [
                "b.txt: converted, but its folder was moved or replaced while it was saved; \
                 not confirmed on disk: folder sync failed (op 00000000000000cd)",
                "1 not confirmed where it was saved; Undo unavailable."
            ]
        );
    }

    /// Two events for one input are one input not confirmed.
    #[test]
    fn repeated_unknown_for_one_input_counts_once() {
        use engine::exec::{AnchorHint, CheckPoint, GroupId, LocationEvidence, OperationId};
        let unknown = |input: &str| Outcome::PublishedUnknown {
            input: input.into(),
            group: GroupId(1),
            location: LocationEvidence {
                path_at_check: None,
                anchor: AnchorHint {
                    parent_dev: 1,
                    parent_ino: 2,
                    object_dev: 1,
                    object_ino: 3,
                    leaf: "b".into(),
                    op: OperationId(0xcd),
                },
                checked_at: CheckPoint::PostPublish,
            },
            reason: PublicationReason::BaselineChanged,
            durability: Durability::Synced,
        };
        let s = summary(&[
            unknown("/d/b.txt"),
            unknown("/d/b.txt"),
            unknown("/d/c.txt"),
        ]);
        assert_eq!(s.unknown_count, 2);
        assert_eq!(s.unknown.len(), 2);
        let (_, body) = final_text(&s, false);
        assert!(body.contains("2 not confirmed where"), "{body}");
    }

    #[test]
    fn single_output_summary_is_the_file_name() {
        let s = summary(&[done("/d/a.txt", "/d/a.md")]);
        assert_eq!(final_text(&s, false), ("a.md".into(), String::new()));
    }

    #[test]
    fn many_outputs_summary_counts() {
        let s = summary(&[
            done("/d/a.txt", "/d/a.md"),
            done("/d/b.txt", "/d/b.md"),
            skip("/d/c.png", "unsupported"),
            fail("/d/e.txt", "cannot read"),
        ]);
        let (sum, body) = final_text(&s, false);
        assert_eq!(sum, "2 done, 1 skipped, 1 failed");
        assert_eq!(lines(&body), ["e.txt: cannot read", "c.png: unsupported"]);
        // Without problems only the done count.
        let s = summary(&[done("/d/a.txt", "/d/a.md"), done("/d/b.txt", "/d/b.md")]);
        assert_eq!(final_text(&s, false), ("2 done".into(), String::new()));
        // One input with several outputs is not the single-output case.
        let mut s = summary(&[
            done("/d/a.pdf", "/d/a/1.png"),
            done("/d/a.pdf", "/d/a/2.png"),
        ]);
        s.set_counts((1, 0, 0, 0));
        assert_eq!(final_text(&s, false).0, "1 done");
    }

    #[test]
    fn problems_list_is_capped_at_three_lines() {
        let mut s = summary(&[
            done("/d/a.txt", "/d/a.md"),
            skip("/d/s1", "unsupported"),
            skip("/d/s2", "unsupported"),
            fail("/d/f1", "x"),
            skip("/d/s3", "unsupported"),
            fail("/d/f2", "y"),
        ]);
        s.set_counts((1, 3, 2, 0));
        let (sum, body) = final_text(&s, false);
        assert_eq!(sum, "1 done, 3 skipped, 2 failed");
        // Failures first, then skips, then the rest as a count.
        assert_eq!(
            lines(&body),
            ["f1: x", "f2: y", "s1: unsupported", "… and 2 more"]
        );
    }

    #[test]
    fn nothing_converted() {
        let s = summary(&[skip("/d/a.png", "unsupported"), fail("/d/b.txt", "bad")]);
        let (sum, body) = final_text(&s, false);
        assert_eq!(sum, "Nothing converted");
        assert_eq!(lines(&body), ["b.txt: bad", "a.png: unsupported"]);
    }

    #[test]
    fn cancelled_text() {
        let mut s = summary(&[
            done("/d/a.txt", "/d/a.md"),
            fail("/d/b.txt", "cancelled"),
            fail("/d/c.txt", "cancelled"),
        ]);
        s.cancelled = true;
        assert_eq!(
            final_text(&s, false),
            ("Cancelled".into(), "1 done, 2 not converted".into())
        );
    }

    #[test]
    fn cancelled_reason_without_a_cancel_is_a_failure() {
        // An action may report a failure in the same words.
        let s = summary(&[fail("/d/b.txt", "cancelled")]);
        assert!(!s.cancelled);
        assert_eq!(
            final_text(&s, false),
            ("Nothing converted".into(), "b.txt: cancelled".into())
        );
    }

    #[test]
    fn lossy_first_line() {
        let mut s = summary(&[done("/d/a.txt", "/d/a.md"), done("/d/b.txt", "/d/b.md")]);
        s.lossy = true;
        assert_eq!(lines(&final_text(&s, false).1), ["Formatting was lost."]);
        s.set_counts((2, 1, 0, 0));
        s.problems.push(("c.png".into(), "unsupported".into()));
        assert_eq!(
            lines(&final_text(&s, false).1),
            ["Formatting was lost.", "c.png: unsupported"]
        );
        // Nothing done: nothing lost.
        let mut s = summary(&[fail("/d/b.txt", "bad")]);
        s.lossy = true;
        assert_eq!(lines(&final_text(&s, false).1), ["b.txt: bad"]);
    }

    #[test]
    fn no_sandbox_last_line() {
        let mut s = summary(&[done("/d/a.txt", "/d/a.md"), fail("/d/b.txt", "bad")]);
        s.sandboxed = false;
        s.lossy = true;
        assert_eq!(
            lines(&final_text(&s, false).1),
            ["Formatting was lost.", "b.txt: bad", "Sandbox unavailable."]
        );
        let mut s = summary(&[done("/d/a.txt", "/d/a.md")]);
        s.sandboxed = false;
        assert_eq!(
            final_text(&s, false),
            ("a.md".into(), "Sandbox unavailable.".into())
        );
    }

    #[test]
    fn downloads_line() {
        let s = summary(&[done("/ro/a.txt", "/home/u/Downloads/a.md")]);
        assert!(s.to_downloads);
        assert_eq!(
            final_text(&s, false),
            (
                "a.md".into(),
                "Saved to Downloads (source folder is read-only)".into()
            )
        );
        // Several outputs of one input in a folder next to the source.
        let s = summary(&[done("/d/a.pdf", "/d/a/1.png")]);
        assert!(!s.to_downloads);
    }

    #[test]
    fn notice_text_escapes_and_truncates() {
        let s = summary(&[fail("/d/a<b>&c.txt", "bad")]);
        assert_eq!(final_text(&s, true).1, "a&lt;b&gt;&amp;c.txt: bad");
        assert_eq!(final_text(&s, false).1, "a<b>&c.txt: bad");
        // Control characters would break the lines apart.
        let s = summary(&[fail("/d/a\tb\nc.txt", "bad")]);
        assert_eq!(final_text(&s, false).1, "a b c.txt: bad");
        // Bidi overrides would show `invoiceexe.pdf`; line and paragraph
        // separators break lines on some servers.
        let s = summary(&[fail("/d/invoice\u{202E}fdp.exe", "bad")]);
        assert_eq!(final_text(&s, false).1, "invoice fdp.exe: bad");
        let odd = "a\u{2028}b\u{2029}c\u{200E}d\u{200F}e\u{202A}f\u{202D}g\u{2066}h\u{2069}i";
        let s = summary(&[fail(&format!("/d/{odd}"), "bad")]);
        assert_eq!(final_text(&s, false).1, "a b c d e f g h i: bad");

        let long = format!("{}.txt", "é".repeat(300));
        let out = format!("/d/{}.md", "é".repeat(300));
        let s = summary(&[done(&format!("/d/{long}"), &out)]);
        let (sum, _) = final_text(&s, true);
        assert!(sum.chars().count() <= MAX_CHARS, "{}", sum.chars().count());
        assert!(sum.ends_with('…') && sum.starts_with("éé"), "{sum}");
        let s = summary(&[fail(&format!("/d/{long}"), "bad")]);
        let body = final_text(&s, false).1;
        assert!(body.chars().count() <= MAX_CHARS && body.ends_with('…'));
        // Escaping counts toward the limit and never cuts an entity.
        let s = summary(&[fail(&format!("/d/{}", "&".repeat(300)), "bad")]);
        let body = final_text(&s, true).1;
        assert!(
            body.chars().count() <= MAX_CHARS,
            "{}",
            body.chars().count()
        );
        assert!(body.ends_with("&amp;…"), "{body}");
        let (sum, _, _) = running_text(&"x".repeat(300), 0, 1, 0.0);
        assert_eq!(sum.chars().count(), MAX_CHARS);
    }

    #[test]
    fn summary_memory_is_bounded() {
        let mut s = Summary::default();
        for i in 0..10_000 {
            s.record(&done(&format!("/d/{i}.txt"), &format!("/d/{i}.md")));
            s.record(&fail(&format!("/d/{i}.x"), "bad"));
            s.record(&skip(&format!("/d/{i}.y"), "unsupported"));
        }
        assert!(s.done.len() <= KEEP_DONE && s.problems.len() <= KEEP_PROBLEMS);
        assert_eq!(
            (s.done_count, s.skipped, s.failed),
            (10_000, 10_000, 10_000)
        );
    }

    #[test]
    fn undo_text_counts_removed_and_kept() {
        let r = UndoResult {
            removed: vec!["/d/a.md".into(), "/d/b.md".into()],
            kept: vec![],
        };
        assert_eq!(undo_text(&r), ("Undone".into(), "2 removed".into()));
        let r = UndoResult {
            removed: vec!["/d/a.md".into()],
            kept: vec![
                ("/d/b.md".into(), "changed since".into()),
                ("/d/c.md".into(), "missing".into()),
                ("/d/e.md".into(), "changed since".into()),
            ],
        };
        assert_eq!(
            undo_text(&r),
            (
                "Undone".into(),
                "1 removed, 2 kept (changed since), 1 kept (missing)".into()
            )
        );
    }
}
