//! Desktop notifications for a run: the texts (pure functions) and the
//! [`Notifier`], which shows "running" with Cancel while the run goes on,
//! the result with Undo at the end, and waits a bounded time for the click.

use std::os::fd::{AsRawFd, RawFd};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use dbus::Error;
use dbus::notifications::{Event, Notice, Notifications};
use engine::exec::{CancelToken, Durability, Observer, Outcome, PublicationReason, Wake};
use engine::journal::{self, GroupOutcome, JobId, UndoReason, UndoResult};

use crate::surface::StartupError;

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
/// A call once the run is over. `notification_rpc_deadline`
/// (parameters.g0p.json): one RPC gets 500 ms, the figure the D-Bus crate's
/// own per-call wait has; the preflight put a working fake server's reply
/// in the microseconds, so this only bounds a server that stopped.
const CALL_WAIT: Duration = Duration::from_millis(500);
/// The whole open (connect, authenticate, `Hello`, capabilities, owner,
/// match) for the failure notice: one shared 500 ms frame, the same
/// `notification_rpc_deadline` row, so the open cannot cost more than one
/// call does.
const ERROR_OPEN: Duration = Duration::from_millis(500);
/// The notification's summary when the wheel did not open.
const COULD_NOT_OPEN: &str = "Could not open the circle.";

/// The most characters in a summary or in one body line.
const MAX_CHARS: usize = 200;
/// Problem lines in a body before `… and <r> more`.
const PROBLEM_LINES: usize = 3;
/// Outputs and problems a [`Summary`] keeps: enough for every text.
const KEEP_DONE: usize = 2;
const KEEP_PROBLEMS: usize = 4;
/// Warned inputs a [`Summary`] keeps to match against their `done`. A
/// warning needs a lossy action to have dropped something from that very
/// input, so a normal selection has a handful; 64 is far above that and
/// costs at most 64 paths (about 8 kB for ordinary paths, 256 kB at the
/// 4096-byte path limit). Past it the summary no longer tells which inputs
/// were warned about, sets `lost_overflow`, and from then on says "lost"
/// for any input that has an output: it never claims that nothing was lost.
const KEEP_LOST: usize = 64;

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
    /// The route has a `lossy=yes` action: only then can a warning mean
    /// lost formatting (the gate; the loss itself is `lost_done`).
    pub lossy: bool,
    /// Some input with an output also had a warning. The text family warns
    /// only about a loss; an input that fails does not count (it has no
    /// output).
    pub lost_done: bool,
    /// The first inputs that were warned about (the engine relays an
    /// input's `warn` lines before its `done`, so a `done` finds its input
    /// here). At most `KEEP_LOST`: a run with more falls back to "some
    /// input lost something", see `lost_done`.
    lost_inputs: Vec<PathBuf>,
    /// A warned input did not fit in `lost_inputs`: any input converted
    /// from then on may be that one, so each counts as having lost.
    lost_overflow: bool,
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
    /// The run's recording warning (no Undo, and why), from
    /// `journal::Recorded`.
    pub warning: Option<String>,
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
                if self.lost_overflow || self.lost_inputs.contains(input) {
                    self.lost_done = true;
                }
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
                // Deduplicated here only among the first KEEP_PROBLEMS
                // inputs (a bounded scan, not O(n^2)); the real count is
                // the per-input one that `set_counts` puts over this.
                self.unknown_count += 1;
            }
            Outcome::Warned { input, .. } => {
                if self.lost_inputs.contains(input) {
                    // Another warning of an input already known.
                } else if self.lost_inputs.len() < KEEP_LOST {
                    self.lost_inputs.push(input.clone());
                } else {
                    self.lost_overflow = true;
                    // The converted inputs so far may include this one.
                    if self.done_count > 0 {
                        self.lost_done = true;
                    }
                }
            }
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
    if s.lossy && s.lost_done {
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
    // Why Undo is not offered (the unknown lines above already say so).
    if s.unknown_count == 0
        && let Some(w) = &s.warning
    {
        body.push(w.clone());
    }
    // The sandbox gate refused the run: no action started.
    if !s.sandboxed {
        body.push("Sandbox unavailable.".to_owned());
    }
    (fit(&summary, false), body_of(&body, markup))
}

/// The notification for a wheel that did not open: the summary and the
/// cause, in plain text (the markup escaping is the sender's). The cause is
/// named only as far as it is proven; an unknown one gets the general copy
/// and never blames the pointer.
pub fn startup_error_text(e: &StartupError) -> (String, String) {
    let body = match e {
        StartupError::PointerUnavailable => "The pointer position was unavailable.",
        StartupError::KeyboardUnavailable => "The keyboard was unavailable.",
        StartupError::ConfigureTimeout => "The compositor did not respond in time.",
        StartupError::Protocol(e) if e.kind() == std::io::ErrorKind::TimedOut => {
            "The wheel could not start in time."
        }
        // Never announced (see `wheel_failed`); the text is for completeness.
        StartupError::AfterOpen(_) | StartupError::Protocol(_) => {
            "The connection to the compositor failed."
        }
        StartupError::Initialization(_) => "The wheel could not be set up.",
    };
    (COULD_NOT_OPEN.to_owned(), body.to_owned())
}

/// Shows the failure notice once, through the bounded path the run's final
/// notice uses: the open within `ERROR_OPEN`, the call within `CALL_WAIT`,
/// no actions (no Undo, no Cancel), nothing to wait for afterwards. No bus,
/// or a bus that fails or stays silent, is one line on stderr; it never
/// changes the exit.
pub fn notify_startup_error(e: &StartupError) {
    let (summary, body) = startup_error_text(e);
    let Some(addr) = dbus::session_address() else {
        eprintln!("inacircle: notifications unavailable: no unix session bus address");
        return;
    };
    let mut bus = match Notifications::open_within(&addr, Instant::now() + ERROR_OPEN) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("inacircle: notifications unavailable: {e}");
            return;
        }
    };
    let body = body_of(&[body], bus.has("body-markup"));
    let n = Notice {
        replaces: 0,
        summary: &summary,
        body: &body,
        actions: &[],
        progress: None,
    };
    if let Err(e) = bus.notify(&n, Instant::now() + CALL_WAIT) {
        eprintln!("inacircle: notifications unavailable: {e}");
    }
}

/// Why a group was kept, as the user reads it after a count.
fn kept_why(r: UndoReason) -> &'static str {
    match r {
        UndoReason::Changed => "kept because it changed",
        UndoReason::Missing => "not found where it was saved",
        UndoReason::AncestorChanged => "kept: its folder moved",
        UndoReason::NoTrash => "kept: no Trash on the same disk",
        UndoReason::NameTaken => "kept: no free name in Trash",
        UndoReason::TooLong => "kept: path too long for Trash",
        UndoReason::HistoryNotWritten(_) => "kept: history not writable",
        UndoReason::InfoNotWritten(_) => "kept: Trash info not written",
        UndoReason::MoveRefused(_) => "kept: could not move to Trash",
        _ => "kept",
    }
}

/// Why a moved group came back.
fn restored_why(r: UndoReason) -> &'static str {
    match r {
        UndoReason::InfoChanged => "moved back: Trash info changed",
        _ => "moved back because it changed",
    }
}

/// `n what` for each distinct `what`, in the order they first appear.
fn counted(items: impl Iterator<Item = String>) -> Vec<String> {
    let mut seen: Vec<(String, usize)> = Vec::new();
    for w in items {
        match seen.iter_mut().find(|(s, _)| *s == w) {
            Some((_, n)) => *n += 1,
            None => seen.push((w, 1)),
        }
    }
    seen.into_iter().map(|(w, n)| format!("{n} {w}")).collect()
}

/// The notification after an Undo click: the summary says what happened as
/// a whole, never more than happened. `Undone` (body `Moved to Trash`) only
/// when every group went to the Trash; `Nothing undone` only when every
/// group was kept (one not found where it was saved says so in its line); `Partially undone` when some moved and
/// the rest is known; `Undo uncertain` whenever a group's place or metadata
/// could not be proved; `Undo incomplete` otherwise (a group moved and came
/// back, or stays in Trash after a failed way back). The body counts the
/// groups by what happened, then gives the places of what needs attention
/// and the warnings, each line fitted.
pub fn undo_text(r: &UndoResult, markup: bool) -> (String, String) {
    let is = |f: fn(&GroupOutcome) -> bool| r.groups.iter().filter(|o| f(o)).count();
    let moved = is(|o| matches!(o, GroupOutcome::MovedToTrash { .. }));
    let kept = is(|o| matches!(o, GroupOutcome::Kept { .. }));
    let uncertain = is(|o| matches!(o, GroupOutcome::Uncertain { .. }));
    let all = r.groups.len();
    let summary = if uncertain > 0 {
        "Undo uncertain"
    } else if moved == all && all > 0 {
        "Undone"
    } else if kept == all {
        "Nothing undone"
    } else if moved > 0 {
        "Partially undone"
    } else {
        "Undo incomplete"
    };
    let mut lines = Vec::new();
    if moved == all && all > 0 {
        lines.push("Moved to Trash".to_owned());
    } else {
        let what = r.groups.iter().map(|o| match o {
            GroupOutcome::MovedToTrash { .. } => "moved to Trash".to_owned(),
            GroupOutcome::Kept { reason, .. } => kept_why(*reason).to_owned(),
            GroupOutcome::Restored { reason, .. } => restored_why(*reason).to_owned(),
            GroupOutcome::RecoveryKept { .. } => "left in Trash".to_owned(),
            GroupOutcome::Uncertain { .. } => "uncertain".to_owned(),
        });
        lines.push(counted(what).join(", "));
    }
    // Where what needs attention is.
    let mut details = Vec::new();
    for o in &r.groups {
        match o {
            GroupOutcome::RecoveryKept {
                location, reason, ..
            } => details.push(match &location.path_at_check {
                Some(p) => format!("in Trash at {} ({})", p.display(), reason.name()),
                None => format!("in Trash ({})", reason.name()),
            }),
            GroupOutcome::Uncertain {
                op,
                locations,
                reason,
                ..
            } => {
                let place = locations
                    .iter()
                    .find_map(|l| l.path_at_check.as_ref())
                    .map(|p| format!("at {}", p.display()))
                    .or_else(|| {
                        locations
                            .iter()
                            .find_map(|l| l.last_unverified_path.as_ref())
                            .map(|p| format!("last seen at {} (not confirmed)", p.display()))
                    })
                    .unwrap_or_else(|| "place unknown".to_owned());
                details.push(format!(
                    "uncertain {place}: {}, nothing deleted (op {:016x})",
                    reason.name(),
                    op.0
                ));
            }
            _ => {}
        }
    }
    let more = details.len().saturating_sub(PROBLEM_LINES);
    details.truncate(PROBLEM_LINES);
    lines.extend(details);
    if more > 0 {
        lines.push(format!("… and {more} more"));
    }
    lines.extend(r.warnings.iter().take(PROBLEM_LINES).cloned());
    (fit(summary, false), body_of(&lines, markup))
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

    /// The run's recording warning (`journal::Recorded::warning`), shown in
    /// the final notification.
    pub fn record_warning(&mut self, warning: Option<&str>) {
        self.summary.warning = warning.map(str::to_owned);
    }

    /// The result of the run with Undo, then the Undo window: an Undo click
    /// undoes `job` and says how it went. Undo is offered only when the
    /// history holds the run complete, durable and synced (`undoable`, from
    /// `journal::Recorded`), with the warning of
    /// [`Notifier::record_warning`] shown otherwise; at the end of the
    /// window the notification is sent again without the button. `counts` are the
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
            Ok(r) => undo_text(&r, markup),
            Err(e) => ("Undo failed".to_owned(), body_of(&[e.to_string()], markup)),
        };
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
    use engine::exec::{AnchorHint, CheckPoint, FileId, GroupId, LocationEvidence, OperationId};

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

    fn warn(input: &str) -> Outcome {
        Outcome::Warned {
            input: input.into(),
            message: "blank lines merged".into(),
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

    /// A recording warning is a body line; next to the unknown lines (which
    /// already say Undo is unavailable) it is not repeated.
    #[test]
    fn recording_warning_is_shown_once() {
        let mut s = summary(&[done("/d/a.txt", "/d/a/a.md")]);
        s.set_counts((1, 0, 0, 0));
        s.warning = Some("history not written: disk full; undo unavailable".into());
        let (summary, body) = final_text(&s, false);
        assert_eq!(summary, "a.md");
        assert_eq!(
            lines(&body),
            ["history not written: disk full; undo unavailable"]
        );
        s.set_counts((1, 0, 0, 1));
        let (_, body) = final_text(&s, false);
        assert!(!body.contains("history not written"), "{body}");
    }

    #[test]
    fn published_unknown_is_a_sticky_line_without_undo() {
        use engine::exec::{AnchorHint, CheckPoint, GroupId, LocationEvidence, OperationId};
        let unknown = Outcome::PublishedUnknown {
            input: "/d/b.txt".into(),
            group: GroupId(1),
            location: LocationEvidence {
                path_at_check: None,
                last_unverified_path: None,
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
                last_unverified_path: None,
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
        let mut s = summary(&[
            warn("/d/a.txt"),
            done("/d/a.txt", "/d/a.md"),
            done("/d/b.txt", "/d/b.md"),
        ]);
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
    fn no_loss_no_formatting_line() {
        // A lossy action whose run lost nothing: no line.
        let mut s = summary(&[done("/d/a.txt", "/d/a.md")]);
        s.lossy = true;
        assert!(final_text(&s, false).1.is_empty());
        // The warning belongs to an input that failed, not to the done one.
        let mut s = summary(&[
            warn("/d/b.txt"),
            done("/d/a.txt", "/d/a.md"),
            fail("/d/b.txt", "bad"),
        ]);
        s.lossy = true;
        assert_eq!(lines(&final_text(&s, false).1), ["b.txt: bad"]);
        // A warning of a route without a lossy action says nothing about
        // formatting.
        let s = summary(&[warn("/d/a.txt"), done("/d/a.txt", "/d/a.md")]);
        assert!(final_text(&s, false).1.is_empty());
    }

    #[test]
    fn warned_inputs_past_the_keep_fall_back_to_some_loss() {
        let mut outcomes = vec![done("/d/z.txt", "/d/z.md")];
        for i in 0..=KEEP_LOST {
            outcomes.push(warn(&format!("/d/{i}.txt")));
        }
        let mut s = summary(&outcomes);
        s.lossy = true;
        assert!(s.lost_inputs.len() <= KEEP_LOST);
        assert_eq!(lines(&final_text(&s, false).1), ["Formatting was lost."]);
    }

    #[test]
    fn warned_inputs_past_the_keep_never_claim_that_nothing_was_lost() {
        // The cap is crossed by inputs that all end in a failure; the next
        // one is warned about, then converted. It is not in the kept list,
        // so the match cannot find it: the notice must still say that the
        // run lost something.
        let mut outcomes = Vec::new();
        for i in 0..KEEP_LOST {
            outcomes.push(warn(&format!("/d/{i}.txt")));
            outcomes.push(fail(&format!("/d/{i}.txt"), "bad"));
        }
        let last = format!("/d/{KEEP_LOST}.txt");
        outcomes.push(warn(&last));
        outcomes.push(done(&last, "/d/last.md"));
        let mut s = summary(&outcomes);
        s.lossy = true;
        assert!(s.lost_done, "overflow was taken as no loss");
        assert_eq!(
            lines(&final_text(&s, false).1)[0],
            "Formatting was lost.",
            "{:?}",
            final_text(&s, false)
        );
        // Control: the same run kept within the cap finds no loss when the
        // converted input was never warned about.
        let mut outcomes = vec![warn("/d/0.txt"), fail("/d/0.txt", "bad")];
        outcomes.push(done("/d/z.txt", "/d/z.md"));
        let mut s = summary(&outcomes);
        s.lossy = true;
        assert!(!s.lost_done);
    }

    #[test]
    fn startup_error_text_names_the_proven_cause() {
        use std::io::{Error, ErrorKind};
        let text = |e: StartupError| startup_error_text(&e);
        let summary = "Could not open the circle.";
        assert_eq!(
            text(StartupError::PointerUnavailable),
            (
                summary.into(),
                "The pointer position was unavailable.".into()
            )
        );
        assert_eq!(
            text(StartupError::KeyboardUnavailable),
            (summary.into(), "The keyboard was unavailable.".into())
        );
        assert_eq!(
            text(StartupError::ConfigureTimeout),
            (
                summary.into(),
                "The compositor did not respond in time.".into()
            )
        );
        assert_eq!(
            text(StartupError::Initialization(Error::other("x"))),
            (summary.into(), "The wheel could not be set up.".into())
        );
        assert_eq!(
            text(StartupError::Protocol(Error::other("x"))),
            (
                summary.into(),
                "The connection to the compositor failed.".into()
            )
        );
        // An unproven cause (a timeout with a pointer present) is the
        // general copy and never mentions the pointer.
        let (_, body) = text(StartupError::Protocol(Error::new(ErrorKind::TimedOut, "t")));
        assert_eq!(body, "The wheel could not start in time.");
        assert!(!body.to_lowercase().contains("pointer"));
        // No cause text leaks into the notice: the log has it.
        let (_, body) = text(StartupError::Protocol(Error::other("secret <b>path</b>")));
        assert!(!body.contains("secret"));
    }

    #[test]
    fn no_sandbox_last_line() {
        let mut s = summary(&[
            warn("/d/a.txt"),
            done("/d/a.txt", "/d/a.md"),
            fail("/d/b.txt", "bad"),
        ]);
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

    fn place(p: Option<&str>, last: Option<&str>) -> LocationEvidence {
        LocationEvidence {
            path_at_check: p.map(PathBuf::from),
            last_unverified_path: last.map(PathBuf::from),
            anchor: AnchorHint {
                parent_dev: 1,
                parent_ino: 2,
                object_dev: 1,
                object_ino: 3,
                leaf: "a".into(),
                op: OperationId(0xcd),
            },
            checked_at: CheckPoint::Precheck,
        }
    }

    fn moved() -> GroupOutcome {
        GroupOutcome::MovedToTrash {
            group: GroupId(1),
            location: place(Some("/t/files/a"), None),
            info_leaf: "a.trashinfo".into(),
            durability: Durability::Synced,
        }
    }

    fn kept(reason: UndoReason) -> GroupOutcome {
        GroupOutcome::Kept {
            group: GroupId(2),
            location: place(Some("/d/a"), None),
            reason,
        }
    }

    fn undo_of(groups: Vec<GroupOutcome>) -> (String, String) {
        undo_text(
            &UndoResult {
                groups,
                warnings: vec![],
            },
            false,
        )
    }

    #[test]
    fn undo_text_counts_groups_by_what_happened() {
        assert_eq!(
            undo_of(vec![moved(), moved()]),
            ("Undone".into(), "Moved to Trash".into())
        );
        assert_eq!(
            undo_of(vec![moved(), moved(), kept(UndoReason::Changed)]),
            (
                "Partially undone".into(),
                "2 moved to Trash, 1 kept because it changed".into()
            )
        );
        // Every group provably kept in place: nothing moved.
        assert_eq!(
            undo_of(vec![
                kept(UndoReason::Changed),
                kept(UndoReason::NoTrash),
                kept(UndoReason::Changed)
            ]),
            (
                "Nothing undone".into(),
                "2 kept because it changed, 1 kept: no Trash on the same disk".into()
            )
        );
    }

    /// A move that came back is no "Nothing undone"; one left in the Trash
    /// gives its place; an uncertain one is never a success nor "Nothing
    /// undone", and its unproved path is said to be unconfirmed.
    #[test]
    fn undo_text_names_moves_back_recovery_places_and_uncertainty() {
        let restored = GroupOutcome::Restored {
            group: GroupId(3),
            location: place(Some("/d/a"), None),
            reason: UndoReason::PostMoveMismatch,
            durability: Durability::Synced,
        };
        assert_eq!(
            undo_of(vec![restored.clone()]),
            (
                "Undo incomplete".into(),
                "1 moved back because it changed".into()
            )
        );
        let left = GroupOutcome::RecoveryKept {
            group: GroupId(4),
            location: place(Some("/t/files/a"), None),
            reason: UndoReason::RestoreCollision,
            errno: Some(17),
            durability: Durability::Synced,
        };
        assert_eq!(
            undo_of(vec![moved(), left]),
            (
                "Partially undone".into(),
                "1 moved to Trash, 1 left in Trash\nin Trash at /t/files/a (RestoreCollision)"
                    .into()
            )
        );
        let unsure = GroupOutcome::Uncertain {
            group: GroupId(5),
            op: OperationId(0xab),
            locations: vec![place(None, Some("/t2/files/a"))],
            reason: UndoReason::AncestorChanged,
        };
        for groups in [vec![unsure.clone()], vec![moved(), unsure.clone()]] {
            let (summary, body) = undo_of(groups);
            assert_eq!(summary, "Undo uncertain");
            assert!(
                body.ends_with(
                    "uncertain last seen at /t2/files/a (not confirmed): AncestorChanged, nothing deleted (op 00000000000000ab)"
                ),
                "{body}"
            );
        }
        // Escaped and bounded: a control character in a path is not shown
        // raw, and the warnings follow.
        let (_, body) = undo_text(
            &UndoResult {
                groups: vec![GroupOutcome::RecoveryKept {
                    group: GroupId(6),
                    location: place(Some("/t/a\nb"), None),
                    reason: UndoReason::RestoreFailed,
                    errno: Some(13),
                    durability: Durability::Synced,
                }],
                warnings: vec!["history: the undo was not recorded".into()],
            },
            false,
        );
        let lines: Vec<&str> = body.lines().collect();
        assert_eq!(lines.len(), 3, "{body}");
        assert!(!lines[1].contains("a\nb") && lines[1].starts_with("in Trash at /t/a"));
        assert_eq!(lines[2], "history: the undo was not recorded");
    }
}
