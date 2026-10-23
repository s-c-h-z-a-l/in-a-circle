//! Desktop notifications for a run: the texts (pure functions) and the
//! [`Notifier`], which shows "running" with Cancel while the run goes on,
//! the result with Undo at the end, and waits a bounded time for the click.

use std::os::fd::{AsRawFd, RawFd};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use dbus::notifications::{Event, Notice, Notifications};
use engine::exec::{Observer, Outcome, Wake};
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
                // The engine's reason for the inputs a cancel stopped.
                self.cancelled |= reason == "cancelled";
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
            Outcome::Warned { .. } => {}
        }
    }

    /// `(done, skipped, failed)` inputs, as the batch rule counts them.
    pub fn set_counts(&mut self, (done, skipped, failed): (usize, usize, usize)) {
        self.done_count = done;
        self.skipped = skipped;
        self.failed = failed;
    }
}

/// The "running" notification: summary, body and the progress value.
pub fn running_text(label: &str, done: usize, total: usize) -> (String, String, u8) {
    let value = (done.min(total) * 100).checked_div(total).unwrap_or(0) as u8;
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
    if !s.sandboxed {
        body.push("Ran without sandbox.".to_owned());
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

/// One line of at most `MAX_CHARS` characters: control characters become
/// spaces, markup is escaped (an entity counts in full and is never cut),
/// and a cut line ends in `…`.
fn fit(s: &str, markup: bool) -> String {
    let piece = |c: char| -> std::borrow::Cow<'static, str> {
        match c {
            '&' if markup => "&amp;".into(),
            '<' if markup => "&lt;".into(),
            '>' if markup => "&gt;".into(),
            c if c.is_control() => " ".into(),
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
    /// Failed once: no notifications for the rest of the run.
    Off,
}

/// Notifications for one run. As the run's [`Observer`] it shows a
/// "running" notification with progress and Cancel once the run takes
/// longer than [`RUNNING_AFTER`]; [`Notifier::finish`] shows the result
/// and waits for Undo. Connects to the bus only when it first has
/// something to show. Any bus error turns it off for the rest of the run
/// with one line on stderr; it never changes the run's result.
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
    /// Inputs done, and the count on screen.
    done: usize,
    shown: usize,
    /// When the last progress update went out.
    sent: Instant,
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
            shown: 0,
            sent: now,
        }
    }

    /// The open connection, connecting first if not tried yet.
    fn bus(&mut self) -> Option<&mut Notifications> {
        if let Bus::Idle = self.bus {
            match dbus::session_address() {
                Some(addr) => match Notifications::open(&addr, Instant::now() + CONNECT_WAIT) {
                    Ok(n) => self.bus = Bus::On(n),
                    Err(e) => self.off(&e),
                },
                None => self.off(&"no unix session bus address"),
            }
        }
        match &mut self.bus {
            Bus::On(n) => Some(n),
            _ => None,
        }
    }

    fn off(&mut self, e: &dyn std::fmt::Display) {
        eprintln!("inacircle: notifications unavailable: {e}");
        self.bus = Bus::Off;
    }

    /// Shows `n`; an error turns notifications off.
    fn show(&mut self, n: &Notice, wait: Duration) -> Option<u32> {
        let r = self.bus()?.notify(n, Instant::now() + wait);
        match r {
            Ok(id) => Some(id),
            Err(e) => {
                self.off(&e);
                None
            }
        }
    }

    fn markup(&mut self) -> bool {
        self.bus().is_some_and(|n| n.has("body-markup"))
    }

    /// Shows or updates the "running" notification.
    fn running(&mut self) {
        let (summary, body, value) = running_text(&self.label, self.done, self.total);
        let n = Notice {
            replaces: self.id,
            summary: &summary,
            body: &body,
            actions: &[("cancel", "Cancel")],
            progress: Some(value),
        };
        // The first one may connect (with its own deadlines); updates and
        // the rest must not hold up the supervisor.
        if let Some(id) = self.show(&n, QUICK_WAIT) {
            self.id = id;
            self.shown = self.done;
            self.sent = Instant::now();
        }
    }

    /// Due work: the first "running" notification, or a progress update
    /// held back by the rate limit.
    fn tick(&mut self) {
        let now = Instant::now();
        let first = self.id == 0 && now >= self.start + RUNNING_AFTER;
        let update = self.id != 0
            && !self.closed
            && self.done != self.shown
            && now >= self.sent + PROGRESS_EVERY;
        if matches!(self.bus, Bus::Off) || !(first || update) {
            return;
        }
        self.running();
    }

    /// Reads what the server sent; true for a Cancel click on ours.
    fn drain(&mut self) -> bool {
        let id = self.id;
        let Bus::On(n) = &mut self.bus else {
            return false;
        };
        let mut cancel = false;
        loop {
            match n.poll_event(id) {
                Ok(Some(Event::Action(key))) => cancel |= key == "cancel",
                Ok(Some(Event::Closed(_))) => self.closed = true,
                Ok(Some(Event::Timeout)) => {}
                Ok(None) if n.pending() => {}
                Ok(None) => return cancel,
                Err(e) => {
                    self.off(&e);
                    return cancel;
                }
            }
        }
    }

    /// The result of the run with Undo, then the Undo window: an Undo click
    /// undoes `job` and says how it went; at the end of the window the
    /// notification is sent again without the button. `counts` are the
    /// run's `(done, skipped, failed)` inputs. The run's history and job
    /// registry entry must be closed before this is called.
    pub fn finish(mut self, job: &JobId, sandboxed: bool, counts: (usize, usize, usize)) {
        if matches!(self.bus, Bus::Off) {
            return;
        }
        let mut s = std::mem::take(&mut self.summary);
        s.set_counts(counts);
        s.sandboxed = sandboxed;
        let markup = self.markup();
        let (summary, body) = final_text(&s, markup);
        let undo: &[(&str, &str)] = if s.done_count > 0 {
            &[("undo", "Undo")]
        } else {
            &[]
        };
        let mut n = Notice {
            replaces: self.id,
            summary: &summary,
            body: &body,
            actions: undo,
            progress: None,
        };
        let Some(id) = self.show(&n, CALL_WAIT) else {
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
            match bus.next_event(id, deadline) {
                Ok(Event::Action(key)) if key == "undo" => break,
                // A Cancel click that came too late, after the run.
                Ok(Event::Action(_)) => {}
                Ok(Event::Closed(_)) => return,
                Ok(Event::Timeout) => {
                    // No dead button left in the notification centre.
                    n.replaces = id;
                    n.actions = &[];
                    self.show(&n, CALL_WAIT);
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
        self.show(
            &Notice {
                replaces: 0,
                summary: &summary,
                body: &body,
                actions: &[],
                progress: None,
            },
            CALL_WAIT,
        );
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

    fn progress(&mut self, done: usize, _total: usize, _fraction: f32) {
        self.done = done;
        self.tick();
    }

    fn wake_fd(&self) -> Option<RawFd> {
        match &self.bus {
            Bus::On(n) if self.id != 0 => Some(n.as_raw_fd()),
            _ => None,
        }
    }

    fn wake_at(&self) -> Option<Instant> {
        match self.bus {
            Bus::Off => None,
            _ if self.id == 0 => Some(self.start + RUNNING_AFTER),
            _ if !self.closed && self.done != self.shown => Some(self.sent + PROGRESS_EVERY),
            _ => None,
        }
    }

    fn wake(&mut self) -> Wake {
        let cancel = self.drain();
        self.tick();
        if cancel { Wake::Cancel } else { Wake::Continue }
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
        assert_eq!(
            running_text("Markdown", 1, 3),
            ("Markdown".into(), "1 / 3 files".into(), 33)
        );
        assert_eq!(running_text("Markdown", 0, 0).2, 0);
        assert_eq!(running_text("Markdown", 5, 5).2, 100);
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
        s.set_counts((1, 0, 0));
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
        s.set_counts((1, 3, 2));
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
        let s = summary(&[
            done("/d/a.txt", "/d/a.md"),
            fail("/d/b.txt", "cancelled"),
            fail("/d/c.txt", "cancelled"),
        ]);
        assert!(s.cancelled);
        assert_eq!(
            final_text(&s, false),
            ("Cancelled".into(), "1 done, 2 not converted".into())
        );
    }

    #[test]
    fn lossy_first_line() {
        let mut s = summary(&[done("/d/a.txt", "/d/a.md"), done("/d/b.txt", "/d/b.md")]);
        s.lossy = true;
        assert_eq!(lines(&final_text(&s, false).1), ["Formatting was lost."]);
        s.set_counts((2, 1, 0));
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
            ["Formatting was lost.", "b.txt: bad", "Ran without sandbox."]
        );
        let mut s = summary(&[done("/d/a.txt", "/d/a.md")]);
        s.sandboxed = false;
        assert_eq!(
            final_text(&s, false),
            ("a.md".into(), "Ran without sandbox.".into())
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
        let (sum, _, _) = running_text(&"x".repeat(300), 0, 1);
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
