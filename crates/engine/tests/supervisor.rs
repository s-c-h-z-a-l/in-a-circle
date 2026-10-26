//! The supervisor loop against misbehaving fake actions.

mod common;

use common::{Tmp, action, failed, process_with, run, run_with};
use engine::exec::{Observer, Outcome, Wake};
use std::os::fd::{AsRawFd, RawFd};
use std::time::Duration;

#[test]
fn escaping_the_process_group_fails_and_nothing_survives() {
    let t = Tmp::new("escape");
    let a = t.input("a.txt", "a");
    let token = format!("31.{}{}", std::process::id(), line!());
    let bin = t.script(
        "escape",
        &format!(
            "setsid -f sleep {token}\n\
             in=$(head -n1 \"$6\"); printf 'skip\\t%s\\tnope\\n' \"$in\""
        ),
    );
    let act = action(&bin, "md");
    let (r, _) = run(&[&act], std::slice::from_ref(&a), None);
    assert!(
        matches!(&r.outcomes[..], [Outcome::Skipped { .. }]),
        "{:?}",
        r.outcomes
    );
    assert!(!process_with(&token), "escaped sleep {token} still running");
    assert!(t.work_dirs_left().is_empty());
}

#[test]
fn background_descendants_are_killed_before_placement() {
    let t = Tmp::new("bg");
    let a = t.input("a.txt", "a");
    let token = format!("32.{}{}", std::process::id(), line!());
    let bin = t.script(
        "bg",
        &format!(
            "sleep {token} &\n\
             work=$4; in=$(head -n1 \"$6\"); mkdir -p \"$work/0\"; echo x > \"$work/0/a.md\"\n\
             printf 'done\\t%s\\t0/a.md\\n' \"$in\""
        ),
    );
    let act = action(&bin, "md");
    let (r, _) = run(&[&act], std::slice::from_ref(&a), None);
    assert!(
        matches!(&r.outcomes[..], [Outcome::Done { .. }]),
        "{:?}",
        r.outcomes
    );
    assert!(
        !process_with(&token),
        "background sleep {token} survived run()"
    );
    assert!(t.work_dirs_left().is_empty());
}

#[test]
fn unterminated_last_line_after_normal_exit_is_a_violation() {
    let t = Tmp::new("unterminated");
    let a = t.input("a.txt", "a");
    let bin = t.script(
        "cut",
        "in=$(head -n1 \"$6\"); printf 'skip\\t%s\\tunsupported' \"$in\"",
    );
    let act = action(&bin, "md");
    let (r, _) = run(&[&act], std::slice::from_ref(&a), None);
    assert_eq!(
        failed(&r),
        [(a, "contract violation: unterminated event line".to_owned())]
    );
}

#[test]
fn second_terminal_event_is_a_violation() {
    let t = Tmp::new("twice");
    let a = t.input("a.txt", "a");
    let b = t.input("b.txt", "b");
    let bin = t.script(
        "twice",
        "in=$(head -n1 \"$6\"); work=$4; mkdir -p \"$work/0\"; echo x > \"$work/0/a.md\"\n\
         printf 'skip\\t%s\\tunsupported\\n' \"$in\"\n\
         printf 'done\\t%s\\t0/a.md\\n' \"$in\"",
    );
    let act = action(&bin, "md");
    let (r, _) = run(&[&act], &[a.clone(), b.clone()], None);
    assert!(matches!(&r.outcomes[0], Outcome::Skipped { input, .. } if *input == a));
    let f = failed(&r);
    assert_eq!(f.len(), 1, "{:?}", r.outcomes);
    assert_eq!(f[0].0, b);
    assert!(f[0].1.starts_with("contract violation: "), "{}", f[0].1);
    assert!(!r.outcomes.iter().any(|o| matches!(o, Outcome::Done { .. })));
}

#[test]
fn warnings_capped_per_input() {
    let t = Tmp::new("warns");
    let a = t.input("a.txt", "a");
    let bin = t.script(
        "warns",
        "in=$(head -n1 \"$6\")\n\
         i=0; while [ $i -lt 20 ]; do printf 'warn\\t%s\\tw%s\\n' \"$in\" $i; i=$((i+1)); done\n\
         printf 'skip\\t%s\\tunsupported\\n' \"$in\"",
    );
    let act = action(&bin, "md");
    let (r, _) = run(&[&act], std::slice::from_ref(&a), None);
    let warns: Vec<&str> = r
        .outcomes
        .iter()
        .filter_map(|o| match o {
            Outcome::Warned { message, .. } => Some(message.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(warns.len(), 17, "{warns:?}");
    assert_eq!(warns[0], "w0");
    assert_eq!(warns[15], "w15");
    assert_eq!(warns[16], "4 more warnings");
    assert_eq!(r.outcomes.len(), 18);
}

#[test]
fn two_step_fan_out_fails_an_input_once() {
    let t = Tmp::new("fanout");
    let a = t.input("a.txt", "a");
    // Step 1: two outputs for the one input. Step 2: fails every input.
    let s1 = t.script(
        "split",
        "in=$(head -n1 \"$6\"); work=$4; mkdir -p \"$work/0\"\n\
         echo 1 > \"$work/0/p1.md\"; echo 2 > \"$work/0/p2.md\"\n\
         printf 'done\\t%s\\t0/p1.md\\ndone\\t%s\\t0/p2.md\\n' \"$in\" \"$in\"",
    );
    let s2 = t.script(
        "failall",
        "while IFS= read -r in; do printf 'fail\\t%s\\tbroken\\n' \"$in\"; done < \"$6\"",
    );
    let (a1, a2) = (action(&s1, "md"), action(&s2, "txt"));
    let (r, _) = run(&[&a1, &a2], std::slice::from_ref(&a), None);
    assert_eq!(failed(&r), [(a, "broken".to_owned())], "{:?}", r.outcomes);
    assert!(t.work_dirs_left().is_empty());
}

#[test]
fn cancel_fails_unfinished_inputs_quickly() {
    let t = Tmp::new("cancel");
    let a = t.input("a.txt", "a");
    let b = t.input("b.txt", "b");
    let token = format!("33.{}{}", std::process::id(), line!());
    let bin = t.script("slow", &format!("sleep {token}"));
    let act = action(&bin, "md");
    let (r, latency) = run(
        &[&act],
        &[a.clone(), b.clone()],
        Some(Duration::from_millis(200)),
    );
    assert_eq!(
        failed(&r),
        [(a, "cancelled".to_owned()), (b, "cancelled".to_owned())]
    );
    // Generous: a loaded VM needs more than the 200 ms plus teardown.
    assert!(latency < Duration::from_secs(2), "cancel took {latency:?}");
    assert!(!process_with(&token));
    assert!(t.work_dirs_left().is_empty());
}

/// Copy of `TERM_GRACE` in `exec.rs`: how long a cancelled action may
/// ignore SIGTERM. Keep in step with it (and with `GROUP_GONE`, below).
const TERM_GRACE: Duration = Duration::from_secs(5);

#[test]
fn cancel_of_a_term_ignoring_action_ends_within_the_grace() {
    let t = Tmp::new("cancel-ign");
    let a = t.input("a.txt", "a");
    let token = format!("34.{}{}", std::process::id(), line!());
    let bin = t.script("stubborn", &format!("trap '' TERM\nsleep {token}"));
    let act = action(&bin, "md");
    let (r, latency) = run(
        &[&act],
        std::slice::from_ref(&a),
        Some(Duration::from_millis(200)),
    );
    assert_eq!(failed(&r), [(a, "cancelled".to_owned())]);
    assert!(
        // The grace, then up to `GROUP_GONE` (1 s, `exec.rs`) for the group to vanish.
        latency < TERM_GRACE + Duration::from_secs(2),
        "cancel took {latency:?}"
    );
    assert!(!process_with(&token));
    assert!(t.work_dirs_left().is_empty());
}

/// A script that reports `n` outputs for its first input, each under
/// `0/<deep>/f<k>` where `<deep>` makes the relative path about `len` bytes.
fn many_outputs_script(n: usize, len: usize) -> String {
    let seg = 200;
    let depth = len.saturating_sub(12) / (seg + 1);
    let deep: String = (0..depth)
        .map(|k| format!("/{k}{}", "x".repeat(seg - 1)))
        .collect();
    format!(
        "in=$(head -n1 \"$6\"); rel=0{deep}; d=\"$4/$rel\"; mkdir -p \"$d\"\n\
         i=0; while [ $i -lt {n} ]; do : > \"$d/f$i\"; printf 'done\\t%s\\t%s\\n' \"$in\" \"$rel/f$i\"; i=$((i+1)); done"
    )
}

#[test]
fn more_than_1024_outputs_is_a_violation() {
    let t = Tmp::new("outcap");
    let a = t.input("a.txt", "a");
    let b = t.input("b.txt", "b");
    let bin = t.script("many", &many_outputs_script(1025, 10));
    let act = action(&bin, "md");
    let (r, _) = run(&[&act], &[a.clone(), b.clone()], None);
    let f = failed(&r);
    assert_eq!(f.len(), 2, "{f:?}");
    // The offender fails as such, nothing of it placed; b had no result.
    assert_eq!(f[0], (a, "too many outputs".to_owned()));
    assert_eq!(f[1].0, b);
    assert!(f[1].1.starts_with("contract violation: "), "{}", f[1].1);
    assert_eq!(dones(&r), 0);
    assert_eq!(placed_files(&t), 0);
    assert!(t.work_dirs_left().is_empty());
}

fn dones(r: &common::Ran) -> usize {
    r.outcomes
        .iter()
        .filter(|o| matches!(o, Outcome::Done { .. }))
        .count()
}

/// Entries in `<tmp>/docs` other than the inputs (`*.txt`).
fn placed_files(t: &Tmp) -> usize {
    std::fs::read_dir(t.0.join("docs"))
        .unwrap()
        .flatten()
        .filter(|e| !e.file_name().to_string_lossy().ends_with(".txt"))
        .count()
}

#[test]
fn single_input_over_the_output_cap_fails_and_places_nothing() {
    let t = Tmp::new("outcap1");
    let a = t.input("a.txt", "a");
    let bin = t.script("many", &many_outputs_script(1025, 10));
    let act = action(&bin, "md");
    let (r, _) = run(&[&act], std::slice::from_ref(&a), None);
    assert_eq!(failed(&r), [(a, "too many outputs".to_owned())]);
    assert_eq!(
        r.outcomes.len(),
        1,
        "{:?}",
        &r.outcomes[..r.outcomes.len().min(3)]
    );
    assert_eq!(placed_files(&t), 0);
    assert!(t.work_dirs_left().is_empty());
}

#[test]
fn second_input_over_the_cap_fails_first_keeps_its_result() {
    let t = Tmp::new("outcap2");
    let a = t.input("a.txt", "a");
    let b = t.input("b.txt", "b");
    let bin = t.script(
        "many2",
        "in0=$(sed -n 1p \"$6\"); in1=$(sed -n 2p \"$6\"); mkdir -p \"$4/0\" \"$4/1\"\n\
         : > \"$4/0/a.md\"; printf 'done\\t%s\\t0/a.md\\n' \"$in0\"\n\
         i=0; while [ $i -lt 1024 ]; do : > \"$4/1/f$i\"; printf 'done\\t%s\\t1/f%s\\n' \"$in1\" $i; i=$((i+1)); done",
    );
    let act = action(&bin, "md");
    let (r, _) = run(&[&act], &[a.clone(), b.clone()], None);
    assert_eq!(failed(&r), [(b, "too many outputs".to_owned())]);
    assert!(
        matches!(&r.outcomes[..], [Outcome::Failed { .. }, Outcome::Done { input, .. }] if *input == a),
        "{:?}",
        &r.outcomes[..r.outcomes.len().min(3)]
    );
    assert_eq!(placed_files(&t), 1);
    assert!(t.work_dirs_left().is_empty());
}

#[test]
fn more_than_1_mib_of_output_paths_is_a_violation() {
    let t = Tmp::new("bytecap");
    let a = t.input("a.txt", "a");
    let b = t.input("b.txt", "b");
    // 1000 paths of about 1200 bytes: under the count cap, over 1 MiB.
    let bin = t.script("long", &many_outputs_script(1000, 1400));
    let act = action(&bin, "md");
    let (r, _) = run(&[&act], &[a.clone(), b.clone()], None);
    let f = failed(&r);
    assert_eq!(f.len(), 2, "{f:?}");
    assert_eq!(f[0], (a, "too many outputs".to_owned()));
    assert_eq!(f[1].0, b);
    assert!(f[1].1.contains("bytes"), "{}", f[1].1);
    assert_eq!(dones(&r), 0);
    assert_eq!(placed_files(&t), 0);
}

/// Step 1 of a two-step route: copies input `i` to `<work>/i/a.mid`, reports
/// it, then runs `after` (with `$work`).
fn copy_step(after: &str) -> String {
    format!(
        "work=$4; i=0\n\
         while IFS= read -r in; do mkdir -p \"$work/$i\"; cat \"$in\" > \"$work/$i/a.mid\"\n\
         printf 'done\\t%s\\t%s/a.mid\\n' \"$in\" $i; i=$((i+1)); done < \"$6\"\n\
         {after}"
    )
}

/// Step 2: appends `+` to each input, as `<work>/j/out.md`.
const APPEND_STEP: &str = "work=$4; j=0\n\
     while IFS= read -r in; do mkdir -p \"$work/$j\"; { cat \"$in\"; echo +; } > \"$work/$j/out.md\"\n\
     printf 'done\\t%s\\t%s/out.md\\n' \"$in\" $j; j=$((j+1)); done < \"$6\"";

#[test]
fn two_step_route_places_the_last_steps_output() {
    let t = Tmp::new("twostep");
    let a = t.input("a.txt", "a\n");
    let b = t.input("b.txt", "b\n");
    let s1 = t.script("copy", &copy_step(""));
    let s2 = t.script("append", APPEND_STEP);
    let (a1, a2) = (action(&s1, "mid"), action(&s2, "md"));
    let (r, _) = run(&[&a1, &a2], &[a.clone(), b.clone()], None);
    assert_eq!(dones(&r), 2, "{:?}", r.outcomes);
    let docs = t.0.join("docs");
    assert_eq!(
        std::fs::read_to_string(docs.join("a.md")).unwrap(),
        "a\n+\n"
    );
    assert_eq!(
        std::fs::read_to_string(docs.join("b.md")).unwrap(),
        "b\n+\n"
    );
    assert_eq!(placed_files(&t), 2);
    assert!(t.work_dirs_left().is_empty());
}

/// A secret outside every sandbox rule: `<tmp>/secret/a.mid`, mode 0600.
fn secret(t: &Tmp) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let d = t.0.join("secret");
    std::fs::create_dir_all(&d).unwrap();
    let p = d.join("a.mid");
    std::fs::write(&p, "SECRET\n").unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600)).unwrap();
    p
}

/// Nothing of the secret reached the user's folder.
fn assert_no_secret_placed(t: &Tmp) {
    for e in std::fs::read_dir(t.0.join("docs")).unwrap().flatten() {
        let body = std::fs::read(e.path()).unwrap_or_default();
        assert!(
            !String::from_utf8_lossy(&body).contains("SECRET"),
            "secret leaked into {}",
            e.path().display()
        );
    }
}

#[test]
fn two_step_intermediate_swapped_for_a_symlink_fails() {
    let t = Tmp::new("swapfile");
    let a = t.input("a.txt", "a\n");
    let b = t.input("b.txt", "b\n");
    let key = secret(&t);
    // After reporting, input 0's output becomes a link to the secret.
    let s1 = t.script(
        "swapfile",
        &copy_step(&format!(
            "rm \"$work/0/a.mid\"; ln -s '{}' \"$work/0/a.mid\"",
            key.display()
        )),
    );
    let s2 = t.script("append", APPEND_STEP);
    let (a1, a2) = (action(&s1, "mid"), action(&s2, "md"));
    let (r, _) = run(&[&a1, &a2], &[a.clone(), b.clone()], None);
    assert_eq!(
        failed(&r),
        [(a, "intermediate output changed".to_owned())],
        "{:?}",
        r.outcomes
    );
    assert_eq!(dones(&r), 1);
    let docs = t.0.join("docs");
    assert_eq!(
        std::fs::read_to_string(docs.join("b.md")).unwrap(),
        "b\n+\n"
    );
    assert_no_secret_placed(&t);
    assert!(t.work_dirs_left().is_empty());
}

#[test]
fn two_step_intermediate_dir_swapped_for_a_symlink_fails() {
    let t = Tmp::new("swapdir");
    let a = t.input("a.txt", "a\n");
    let key = secret(&t);
    // After reporting, `<work>/0` becomes a link to the secret's folder,
    // which holds a file of the reported name.
    let s1 = t.script(
        "swapdir",
        &copy_step(&format!(
            "mv \"$work/0\" \"$work/moved\"; ln -s '{}' \"$work/0\"",
            key.parent().unwrap().display()
        )),
    );
    let s2 = t.script("append", APPEND_STEP);
    let (a1, a2) = (action(&s1, "mid"), action(&s2, "md"));
    let (r, _) = run(&[&a1, &a2], std::slice::from_ref(&a), None);
    // The step swaps `0/` right after its `done`: if the supervisor reads the
    // event after the swap, the done check itself refuses it (a contract
    // violation); otherwise the re-check before step 2 does. Both fail the
    // input and place nothing, which is what matters.
    let f = failed(&r);
    assert_eq!(f.len(), 1, "{:?}", r.outcomes);
    assert_eq!(f[0].0, a);
    assert!(
        f[0].1 == "intermediate output changed"
            || f[0]
                .1
                .starts_with("contract violation: done output 0/a.mid"),
        "{:?}",
        r.outcomes
    );
    assert_eq!(dones(&r), 0);
    assert_eq!(placed_files(&t), 0);
    assert_no_secret_placed(&t);
    assert!(t.work_dirs_left().is_empty());
}

/// An observer with a wake point: `fd` (drained on each wake; else the raw
/// number `raw`) and/or `at` (on each wake replaced by the next of `then`,
/// or `None`; kept as is with `keep_at`); every wake answers `answer`.
struct Waker {
    ran: common::Ran,
    fd: Option<std::os::fd::OwnedFd>,
    raw: Option<RawFd>,
    at: Option<std::time::Instant>,
    then: Vec<std::time::Instant>,
    keep_at: bool,
    answer: Wake,
    wakes: usize,
}

impl Waker {
    fn new(answer: Wake) -> Waker {
        Waker {
            ran: common::Ran::default(),
            fd: None,
            raw: None,
            at: None,
            then: Vec::new(),
            keep_at: false,
            answer,
            wakes: 0,
        }
    }
}

impl Observer for Waker {
    fn event(&mut self, o: &Outcome) {
        self.ran.event(o);
    }
    fn dir(&mut self, path: &std::path::Path) {
        self.ran.dir(path);
    }
    fn progress(&mut self, d: usize, t: usize, f: f32) {
        self.ran.progress(d, t, f);
    }
    fn wake_fd(&self) -> Option<RawFd> {
        self.fd.as_ref().map(|f| f.as_raw_fd()).or(self.raw)
    }
    fn wake_at(&self) -> Option<std::time::Instant> {
        self.at
    }
    fn wake(&mut self) -> Wake {
        self.wakes += 1;
        if !self.keep_at {
            self.at = (!self.then.is_empty()).then(|| self.then.remove(0));
        }
        if let Some(f) = &self.fd {
            let mut buf = [0u8; 64];
            // SAFETY: f is our non-blocking pipe; buf holds 64 bytes.
            while unsafe { libc::read(f.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) } > 0 {}
        }
        self.answer
    }
}

/// A non-blocking pipe: (read end, write end).
fn pipe() -> (std::os::fd::OwnedFd, std::os::fd::OwnedFd) {
    use std::os::fd::FromRawFd;
    let mut p = [0; 2];
    // SAFETY: p holds two ints for pipe2 to fill.
    let r = unsafe { libc::pipe2(p.as_mut_ptr(), libc::O_NONBLOCK | libc::O_CLOEXEC) };
    assert_eq!(r, 0);
    // SAFETY: pipe2 succeeded, both fds are new and ours.
    unsafe {
        (
            std::os::fd::OwnedFd::from_raw_fd(p[0]),
            std::os::fd::OwnedFd::from_raw_fd(p[1]),
        )
    }
}

/// Writes one byte to `w` after `d`, from another thread. The caller keeps
/// `w` open until the run is over (a closed write end is a hangup, which
/// keeps the read end ready).
fn write_after(w: RawFd, d: Duration) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        std::thread::sleep(d);
        // SAFETY: w is our pipe's write end, open until joined; one byte
        // from a static.
        let n = unsafe { libc::write(w, b"x".as_ptr().cast(), 1) };
        assert_eq!(n, 1);
    })
}

/// Sleeps `secs`, then skips every input.
fn sleep_then_skip(secs: &str) -> String {
    format!(
        "sleep {secs}\n\
         while IFS= read -r in; do printf 'skip\\t%s\\tunsupported\\n' \"$in\"; done < \"$6\""
    )
}

#[test]
fn wake_fd_cancel_cancels_the_run() {
    let t = Tmp::new("wakefd");
    let a = t.input("a.txt", "a");
    let b = t.input("b.txt", "b");
    let token = format!("35.{}{}", std::process::id(), line!());
    let bin = t.script("slow", &format!("sleep {token}"));
    let act = action(&bin, "md");
    let (r, w) = pipe();
    let mut obs = Waker::new(Wake::Cancel);
    obs.fd = Some(r);
    let writer = write_after(w.as_raw_fd(), Duration::from_millis(200));
    let took = run_with(&[&act], &[a.clone(), b.clone()], &mut obs);
    writer.join().unwrap();
    drop(w);
    assert_eq!(
        failed(&obs.ran),
        [(a, "cancelled".to_owned()), (b, "cancelled".to_owned())]
    );
    assert_eq!(obs.wakes, 1);
    assert!(took < Duration::from_secs(6), "cancel took {took:?}");
    assert!(!process_with(&token));
    assert!(t.work_dirs_left().is_empty());
}

#[test]
fn drained_wake_fd_wakes_once() {
    let t = Tmp::new("wakedrain");
    let a = t.input("a.txt", "a");
    let bin = t.script("sleep1", &sleep_then_skip("1"));
    let act = action(&bin, "md");
    let (r, w) = pipe();
    let mut obs = Waker::new(Wake::Continue);
    obs.fd = Some(r);
    let writer = write_after(w.as_raw_fd(), Duration::from_millis(200));
    run_with(&[&act], std::slice::from_ref(&a), &mut obs);
    writer.join().unwrap();
    drop(w);
    assert_eq!(obs.wakes, 1);
    assert!(
        matches!(&obs.ran.outcomes[..], [Outcome::Skipped { .. }]),
        "{:?}",
        obs.ran.outcomes
    );
}

#[test]
fn wake_at_fires_once_per_deadline() {
    let t = Tmp::new("wakeat");
    let a = t.input("a.txt", "a");
    let bin = t.script("sleep1", &sleep_then_skip("1"));
    let act = action(&bin, "md");
    let mut obs = Waker::new(Wake::Continue);
    let start = std::time::Instant::now();
    obs.at = Some(start + Duration::from_millis(300));
    run_with(&[&act], std::slice::from_ref(&a), &mut obs);
    assert_eq!(obs.wakes, 1);
    assert!(
        matches!(&obs.ran.outcomes[..], [Outcome::Skipped { .. }]),
        "{:?}",
        obs.ran.outcomes
    );
}

#[test]
fn default_observer_is_unchanged() {
    // `Ran` implements none of the wake point: no fd, no deadline.
    let mut ran = common::Ran::default();
    assert_eq!(ran.wake_fd(), None);
    assert_eq!(ran.wake_at(), None);
    assert_eq!(ran.wake(), Wake::Continue);
    let t = Tmp::new("default");
    let a = t.input("a.txt", "a");
    let bin = t.script("quick", &sleep_then_skip("0"));
    let act = action(&bin, "md");
    let took = run_with(&[&act], std::slice::from_ref(&a), &mut ran);
    assert!(
        matches!(&ran.outcomes[..], [Outcome::Skipped { .. }]),
        "{:?}",
        ran.outcomes
    );
    assert!(took < Duration::from_secs(2), "took {took:?}");
}

#[test]
fn cancel_after_wake_starts_no_new_process() {
    let t = Tmp::new("wakebatch");
    let inputs = [
        t.input("a.txt", "a"),
        t.input("b.txt", "b"),
        t.input("c.txt", "c"),
    ];
    // Announces its start with a warning, then outlives the wake.
    let bin = t.script(
        "announce",
        "in=$(head -n1 \"$6\"); printf 'warn\\t%s\\tstarted\\n' \"$in\"\n\
         sleep 1\n\
         printf 'skip\\t%s\\tunsupported\\n' \"$in\"",
    );
    let mut act = action(&bin, "md");
    act.batch = false;
    let mut obs = Waker::new(Wake::Cancel);
    obs.at = Some(std::time::Instant::now() + Duration::from_millis(200));
    run_with(&[&act], &inputs, &mut obs);
    let started: Vec<_> = obs
        .ran
        .outcomes
        .iter()
        .filter_map(|o| match o {
            Outcome::Warned { input, message } if message == "started" => Some(input.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(started, [inputs[0].clone()], "{:?}", obs.ran.outcomes);
    let cancelled: Vec<_> = inputs
        .iter()
        .map(|p| (p.clone(), "cancelled".to_owned()))
        .collect();
    assert_eq!(failed(&obs.ran), cancelled);
    assert_eq!(obs.wakes, 1);
    assert!(t.work_dirs_left().is_empty());
}

#[test]
fn hung_up_wake_fd_wakes_once() {
    let t = Tmp::new("wakehup");
    let a = t.input("a.txt", "a");
    let bin = t.script("sleep1", &sleep_then_skip("1"));
    let act = action(&bin, "md");
    let (r, w) = pipe();
    drop(w);
    let mut obs = Waker::new(Wake::Continue);
    obs.fd = Some(r);
    run_with(&[&act], std::slice::from_ref(&a), &mut obs);
    assert_eq!(obs.wakes, 1);
    assert!(
        matches!(&obs.ran.outcomes[..], [Outcome::Skipped { .. }]),
        "{:?}",
        obs.ran.outcomes
    );
}

#[test]
fn closed_wake_fd_wakes_once() {
    let t = Tmp::new("wakenval");
    let a = t.input("a.txt", "a");
    let bin = t.script("sleep1", &sleep_then_skip("1"));
    let act = action(&bin, "md");
    // A number far above what the tests open, so no other thread reuses it.
    let closed = 999;
    // SAFETY: F_GETFD only checks the number.
    assert_eq!(unsafe { libc::fcntl(closed, libc::F_GETFD) }, -1);
    let mut obs = Waker::new(Wake::Continue);
    obs.raw = Some(closed);
    run_with(&[&act], std::slice::from_ref(&a), &mut obs);
    assert_eq!(obs.wakes, 1);
    assert!(
        matches!(&obs.ran.outcomes[..], [Outcome::Skipped { .. }]),
        "{:?}",
        obs.ran.outcomes
    );
}

#[test]
fn past_wake_at_left_in_place_wakes_once() {
    let t = Tmp::new("wakepast");
    let a = t.input("a.txt", "a");
    let bin = t.script("sleep1", &sleep_then_skip("1"));
    let act = action(&bin, "md");
    let mut obs = Waker::new(Wake::Continue);
    obs.at = Some(std::time::Instant::now());
    obs.keep_at = true;
    run_with(&[&act], std::slice::from_ref(&a), &mut obs);
    assert_eq!(obs.wakes, 1);
}

#[test]
fn wake_at_moved_in_wake_is_honoured() {
    let t = Tmp::new("wakemove");
    let a = t.input("a.txt", "a");
    let bin = t.script("sleep1", &sleep_then_skip("1"));
    let act = action(&bin, "md");
    let mut obs = Waker::new(Wake::Continue);
    let start = std::time::Instant::now();
    obs.at = Some(start + Duration::from_millis(300));
    obs.then = vec![start + Duration::from_millis(600)];
    run_with(&[&act], std::slice::from_ref(&a), &mut obs);
    assert_eq!(obs.wakes, 2);
}

/// Records the order of `started` and outcomes.
#[derive(Default)]
struct Starts {
    log: Vec<String>,
    pids: Vec<u32>,
}

impl Observer for Starts {
    fn event(&mut self, o: &Outcome) {
        self.log.push(match o {
            Outcome::Done { .. } => "done".to_owned(),
            _ => "other".to_owned(),
        });
    }
    fn dir(&mut self, _: &std::path::Path) {}
    fn progress(&mut self, _: usize, _: usize, _: f32) {
        self.log.push("progress".to_owned());
    }
    fn started(&mut self, pid: u32) {
        self.pids.push(pid);
        self.log.push("started".to_owned());
    }
}

/// `progress 0` first (flushed by printf), then more events: the callback
/// runs once per child, before the first outcome, and more events do not
/// repeat it.
#[test]
fn first_action_event_marks_start_once() {
    let t = Tmp::new("started");
    let a = t.input("a.txt", "a\n");
    let b = t.input("b.txt", "b\n");
    let s1 = t.script(
        "copy",
        &format!(
            "printf 'progress\\t0\\n'\n{}",
            copy_step("printf 'progress\\t0.5\\n'")
        ),
    );
    let s2 = t.script("append", APPEND_STEP);
    let (a1, a2) = (action(&s1, "mid"), action(&s2, "md"));
    let mut obs = Starts::default();
    run_with(&[&a1, &a2], &[a, b], &mut obs);
    // One per child (two steps), distinct processes, each before its first
    // outcome; the second step's first event is a `done`.
    assert_eq!(obs.pids.len(), 2, "{:?}", obs.log);
    assert_ne!(obs.pids[0], obs.pids[1]);
    assert!(obs.pids.iter().all(|&p| p > 1));
    assert_eq!(obs.log.iter().filter(|l| *l == "started").count(), 2);
    let first_started = obs.log.iter().position(|l| l == "started").unwrap();
    let first_done = obs.log.iter().position(|l| l == "done").unwrap();
    assert!(first_started < first_done, "{:?}", obs.log);
}

/// A child that emits nothing valid was never observed to start: no
/// callback (the marker is absent, not zero).
#[test]
fn no_child_event_means_no_start_marker() {
    let t = Tmp::new("nostart");
    let a = t.input("a.txt", "a\n");
    let silent = t.script("silent", "exit 0");
    let bad = t.script("bad", "printf 'bogus line\\n'");
    for bin in [silent, bad] {
        let act = action(&bin, "md");
        let mut obs = Starts::default();
        run_with(&[&act], std::slice::from_ref(&a), &mut obs);
        assert!(obs.pids.is_empty(), "{:?}", obs.log);
    }
}
