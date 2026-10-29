//! The supervisor loop against misbehaving fake actions.
//!
//! Every wait loop in a fixture script is bounded (3000 x 10 ms = 30 s, or
//! 45 s of `$SECONDS` for the free-running writer): actions run in their own
//! process group, so a harness that is killed from outside (CI timeout,
//! Ctrl-C, the memory cap) cannot reach them, and an unbounded loop would
//! live on as an orphan. 30 s is far past what a loaded machine needs for
//! one handshake; a fixture that hits the bound exits 1 and fails its test.

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
    // In the foreground: its exit code is the fixture's proof that it ran
    // (util-linux `setsid` exits 1 when the syscall fails).
    let bin = t.script(
        "escape",
        &format!(
            "setsid sleep {token}; s=$?\n\
             in=$(head -n1 \"$6\"); printf 'warn\\t%s\\tsetsid:%s\\n' \"$in\" $s\n\
             printf 'skip\\t%s\\tnope\\n' \"$in\""
        ),
    );
    let act = action(&bin, "md");
    let (r, _) = run(&[&act], std::slice::from_ref(&a), None);
    assert!(
        matches!(
            &r.outcomes[..],
            [Outcome::Warned { message, .. }, Outcome::Skipped { .. }] if message == "setsid:1"
        ),
        "{:?}",
        r.outcomes
    );
    assert!(!process_with(&token), "escaped sleep {token} still running");
    assert!(t.work_dirs_left().is_empty());
}

/// A background descendant still running when the leader exits fails the
/// action's input, though the leader reported its output: killed, the
/// descendant may have been mid-write to it. Nothing is placed, and the
/// descendant is dead when `run` returns. It proves it ran by its own
/// warning.
#[test]
fn background_descendant_fails_the_action_and_is_killed() {
    let t = Tmp::new("bg");
    let a = t.input("a.txt", "a");
    let token = format!("32.{}{}", std::process::id(), line!());
    // bash: dash never starts a background job in the sandbox (see
    // `Tmp::script_in`).
    let bin = t.script_in(
        "/bin/bash",
        "bg",
        &format!(
            "work=$4; in=$(head -n1 \"$6\")\n\
             {{ printf 'warn\\t%s\\tbg up\\n' \"$in\"; exec sleep {token}; }} &\n\
             n=0; until [ \"$(cat \"$in\")\" = ack ]; do n=$((n+1)); [ $n -le 3000 ] || exit 1; sleep 0.01; done\n\
             mkdir -p \"$work/0\"; echo x > \"$work/0/a.md\"\n\
             printf 'done\\t%s\\t0/a.md\\n' \"$in\""
        ),
    );
    let act = action(&bin, "md");
    let mut w = Watch::new(&token);
    w.ack = Some(("bg up".to_owned(), a.clone()));
    // Ends a broken fixture (no ack) instead of hanging.
    w.cancel_at = Some(std::time::Instant::now() + Duration::from_secs(10));
    run_with(&[&act], std::slice::from_ref(&a), &mut w);
    let r = w.ran;
    assert!(
        r.outcomes
            .iter()
            .any(|o| matches!(o, Outcome::Warned { message, .. } if message == "bg up")),
        "the background job never ran: {:?}",
        r.outcomes
    );
    assert_eq!(
        failed(&r),
        [(a.clone(), "action left processes behind".to_owned())]
    );
    assert!(
        !r.outcomes.iter().any(|o| matches!(o, Outcome::Done { .. })),
        "{:?}",
        r.outcomes
    );
    assert!(!a.with_extension("").exists(), "an output was placed");
    assert!(
        !process_with(&token),
        "background sleep {token} survived run()"
    );
    assert!(t.work_dirs_left().is_empty());
}

/// The positive control of the test above: a background job the action
/// waits for is finished at its exit, and the result is placed.
#[test]
fn background_job_waited_for_keeps_the_result() {
    let t = Tmp::new("bg-wait");
    let a = t.input("a.txt", "a");
    let bin = t.script_in(
        "/bin/bash",
        "bgwait",
        "work=$4; in=$(head -n1 \"$6\"); mkdir -p \"$work/0\"\n\
         { echo x > \"$work/0/a.md\"; printf 'warn\\t%s\\tbg up\\n' \"$in\"; } &\n\
         wait\n\
         printf 'done\\t%s\\t0/a.md\\n' \"$in\"",
    );
    let act = action(&bin, "md");
    let (r, _) = run(&[&act], std::slice::from_ref(&a), None);
    assert!(
        matches!(
            &r.outcomes[..],
            [Outcome::Warned { message, .. }, Outcome::Done { .. }] if message == "bg up"
        ),
        "{:?}",
        r.outcomes
    );
    assert_eq!(
        std::fs::read_to_string(a.with_extension("").join("a.md")).unwrap(),
        "x\n"
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
        std::fs::read_to_string(docs.join("a/a.md")).unwrap(),
        "a\n+\n"
    );
    assert_eq!(
        std::fs::read_to_string(docs.join("b/b.md")).unwrap(),
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

/// Nothing of the secret reached the user's folder, nor any output folder
/// in it.
fn assert_no_secret_placed(t: &Tmp) {
    let mut files = Vec::new();
    for e in std::fs::read_dir(t.0.join("docs")).unwrap().flatten() {
        match std::fs::read_dir(e.path()) {
            Ok(sub) => files.extend(sub.flatten().map(|f| f.path())),
            Err(_) => files.push(e.path()),
        }
    }
    for f in files {
        let body = std::fs::read(&f).unwrap_or_default();
        assert!(
            !String::from_utf8_lossy(&body).contains("SECRET"),
            "secret leaked into {}",
            f.display()
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
        std::fs::read_to_string(docs.join("b/b.md")).unwrap(),
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

/// Watches the fixture processes (command line carries `token`) while
/// the engine publishes: every outcome published while one of them was
/// still alive is recorded.
struct Watch {
    token: String,
    ran: common::Ran,
    /// The fixture processes alive when the action's first event arrived.
    seen: Vec<(u32, std::os::fd::OwnedFd)>,
    /// Outcomes published while a fixture process was alive.
    while_alive: Vec<String>,
    /// Each placed output's content at the moment it was published.
    placed: Vec<(std::path::PathBuf, Vec<u8>)>,
    cancel_at: Option<std::time::Instant>,
    /// On a warning with this message, write `ack` into this file (an
    /// input, so the sandboxed action can read it): the fixture's leader
    /// waits for it before it exits, so its proof line is surely read.
    /// Lines still unread at the exit of a process that left others
    /// behind are never read.
    ack: Option<(String, std::path::PathBuf)>,
}

impl Watch {
    fn new(token: &str) -> Watch {
        Watch {
            token: token.to_owned(),
            ran: common::Ran::default(),
            seen: Vec::new(),
            while_alive: Vec::new(),
            placed: Vec::new(),
            cancel_at: None,
            ack: None,
        }
    }

    /// Fixture processes still alive: those seen at the start, by pidfd,
    /// and any other carrying the token now.
    fn alive(&self) -> Vec<u32> {
        let mut pids: Vec<u32> = self
            .seen
            .iter()
            .filter(|(_, fd)| common::alive(fd))
            .map(|&(p, _)| p)
            .collect();
        pids.extend(common::pidfds_with(&self.token).iter().map(|&(p, _)| p));
        pids.sort_unstable();
        pids.dedup();
        pids
    }
}

impl Observer for Watch {
    fn event(&mut self, o: &Outcome) {
        let alive = self.alive();
        if !alive.is_empty() {
            self.while_alive
                .push(format!("{o:?} while {alive:?} alive"));
        }
        if let (Outcome::Warned { message, .. }, Some((on, file))) = (o, &self.ack)
            && message == on
        {
            std::fs::write(file, "ack").unwrap();
        }
        if let Outcome::Done { output, .. } = o {
            self.placed
                .push((output.clone(), std::fs::read(output).unwrap_or_default()));
        }
        self.ran.event(o);
    }
    fn dir(&mut self, path: &std::path::Path) {
        self.ran.dir(path);
    }
    fn progress(&mut self, d: usize, t: usize, f: f32) {
        self.ran.progress(d, t, f);
    }
    fn started(&mut self, _pid: u32) {
        self.seen = common::pidfds_with(&self.token);
    }
    fn wake_at(&self) -> Option<std::time::Instant> {
        self.cancel_at
    }
    fn wake(&mut self) -> Wake {
        Wake::Cancel
    }
}

/// A descendant holds the stdout pipe and an fd on the output, and forks
/// a grandchild that holds both too. Its barrier is the leader's exit:
/// it reads a FIFO only the leader holds open for writing (the leader's
/// open waits for the descendant's), so EOF comes when the leader's fds
/// are closed at exit; then, a margin later (the engine has seen the exit
/// by then), it would write to the pipe and the output for as long as it
/// lives. Alive after the leader's exit, it fails the input: nothing is
/// placed, the failure is published only once every fixture process is
/// dead, and none is left. The descendant proves it ran by its own
/// warning, sent once the FIFO is open.
#[test]
fn descendant_writer_outlives_leader() {
    let t = Tmp::new("desc-writer");
    let a = t.input("a.txt", "a");
    let token = format!("41.{}{}", std::process::id(), line!());
    let bin = t.script_in(
        "/bin/bash",
        "writer",
        &format!(
            "work=$4; in=$(head -n1 \"$6\"); mkdir -p \"$work/0\"\n\
             mkfifo \"$work/bar\"\n\
             bash -c 'exec 7>>\"$1/0/a.md\" <\"$1/bar\"; printf \"warn\\t%s\\tup\\n\" \"$2\"\n\
             sleep \"$0\" & while read -r _; do :; done; sleep 0.1\n\
             while [ $SECONDS -lt 45 ]; do printf late >&7; printf \"warn\\t%s\\tlate\\n\" \"$2\"; done' \
             {token} \"$work\" \"$in\" &\n\
             exec 5>\"$work/bar\"\n\
             n=0; until [ \"$(cat \"$in\")\" = ack ]; do n=$((n+1)); [ $n -le 3000 ] || exit 1; sleep 0.01; done\n\
             printf leader > \"$work/0/a.md\"\n\
             printf 'done\\t%s\\t0/a.md\\n' \"$in\""
        ),
    );
    let act = action(&bin, "md");
    let mut w = Watch::new(&token);
    w.ack = Some(("up".to_owned(), a.clone()));
    // A broken fixture (the descendant never opens the FIFO, or no ack)
    // would leave the leader waiting: cancelled then, not a hung test.
    w.cancel_at = Some(std::time::Instant::now() + Duration::from_secs(10));
    let took = run_with(&[&act], std::slice::from_ref(&a), &mut w);
    assert!(
        w.ran
            .outcomes
            .iter()
            .any(|o| matches!(o, Outcome::Warned { message, .. } if message == "up")),
        "the fixture descendant never ran: {:?}",
        w.ran.outcomes
    );
    assert_eq!(
        failed(&w.ran),
        [(a.clone(), "action left processes behind".to_owned())]
    );
    assert!(w.placed.is_empty(), "{:?}", w.placed);
    assert!(!a.with_extension("").exists(), "an output was placed");
    // Only the final failure counts here: the descendant's `up` warning
    // was streamed while the leader ran, as every event is.
    let late: Vec<_> = w
        .while_alive
        .iter()
        .filter(|e| !e.contains("message: \"up\""))
        .collect();
    assert!(late.is_empty(), "{late:#?}");
    assert!(w.alive().is_empty(), "left alive: {:?}", w.alive());
    // Not held up by the descendant: it was killed, not waited for.
    assert!(took < Duration::from_secs(2), "{took:?}");
    assert!(t.work_dirs_left().is_empty());
}

/// Leaving the process group is denied in the sandbox: `setsid` and a new
/// pid or user namespace fail with EPERM, and the descendants (the denied
/// tries and a background one, which fails the input) all end with the
/// action. Positive control:
/// the same `setsid` succeeds outside the sandbox.
#[test]
fn descendant_cannot_escape_group() {
    let st = std::process::Command::new("setsid")
        .args(["sleep", "0"])
        .status()
        .unwrap();
    assert!(st.success(), "control: setsid outside the sandbox failed");
    let t = Tmp::new("desc-escape");
    let a = t.input("a.txt", "a");
    let token = format!("42.{}{}", std::process::id(), line!());
    let bin = t.script_in(
        "/bin/bash",
        "escape",
        &format!(
            "in=$(head -n1 \"$6\")\n\
             setsid sleep {token}; s=$?\n\
             unshare --pid --fork sleep {token}; p=$?\n\
             unshare --user sleep {token}; u=$?\n\
             printf 'warn\\t%s\\tsetsid:%s pidns:%s userns:%s\\n' \"$in\" $s $p $u\n\
             {{ printf 'warn\\t%s\\tbg up\\n' \"$in\"; exec sleep {token}; }} &\n\
             n=0; until [ \"$(cat \"$in\")\" = ack ]; do n=$((n+1)); [ $n -le 3000 ] || exit 1; sleep 0.01; done"
        ),
    );
    let act = action(&bin, "md");
    let mut w = Watch::new(&token);
    w.ack = Some(("bg up".to_owned(), a.clone()));
    w.cancel_at = Some(std::time::Instant::now() + Duration::from_secs(10));
    run_with(&[&act], std::slice::from_ref(&a), &mut w);
    let warned: Vec<&str> = w
        .ran
        .outcomes
        .iter()
        .filter_map(|o| match o {
            Outcome::Warned { message, .. } => Some(message.as_str()),
            _ => None,
        })
        .collect();
    // util-linux exits 1 when the syscall fails ("setsid failed",
    // "unshare failed"); 127 would be a missing tool, 0 an escape.
    assert!(
        warned.contains(&"setsid:1 pidns:1 userns:1"),
        "{:?}",
        w.ran.outcomes
    );
    // The background descendant's own proof that it ran.
    assert!(warned.contains(&"bg up"), "{:?}", w.ran.outcomes);
    // Still running at the leader's exit: the input fails (with no final
    // event of its own, which would already have settled it).
    assert_eq!(
        failed(&w.ran),
        [(a.clone(), "action left processes behind".to_owned())]
    );
    assert!(w.alive().is_empty(), "left alive: {:?}", w.alive());
    assert!(t.work_dirs_left().is_empty());
}

/// On cancel the leader dies of its SIGTERM, but a descendant and its
/// forked grandchild ignore it and hold the pipes: both are killed with
/// the group, and the cancellation is published only once they are dead.
/// Each proves it ran by its own marker file, which the leader waits for
/// (a 300 ms cancel ends the wait of a broken fixture).
#[test]
fn descendant_reaped_on_cancel() {
    let t = Tmp::new("desc-cancel");
    let a = t.input("a.txt", "a");
    let token = format!("43.{}{}", std::process::id(), line!());
    let bin = t.script_in(
        "/bin/bash",
        "cancel",
        &format!(
            "work=$4\n\
             bash -c 'trap \"\" TERM; {{ : > \"$1/g\"; exec sleep \"$0\"; }} & : > \"$1/up\"; exec sleep \"$0\"' {token} \"$work\" &\n\
             n=0; while [ ! -e \"$work/up\" ] || [ ! -e \"$work/g\" ]; do n=$((n+1)); [ $n -le 3000 ] || exit 1; sleep 0.01; done\n\
             printf 'progress\\t0.1\\n'\n\
             sleep {token}"
        ),
    );
    let act = action(&bin, "md");
    let mut w = Watch::new(&token);
    let start = std::time::Instant::now();
    w.cancel_at = Some(start + Duration::from_millis(300));
    let took = run_with(&[&act], std::slice::from_ref(&a), &mut w);
    assert_eq!(failed(&w.ran), [(a, "cancelled".to_owned())]);
    // The descendant, its grandchild and the leader's sleep.
    assert!(w.seen.len() >= 2, "fixture not up: {:?}", w.seen);
    assert!(w.while_alive.is_empty(), "{:#?}", w.while_alive);
    assert!(w.alive().is_empty(), "left alive: {:?}", w.alive());
    // The leader obeys SIGTERM: no TERM_GRACE wait for the ignoring ones.
    assert!(took < Duration::from_secs(2), "{took:?}");
    assert!(t.work_dirs_left().is_empty());
}

/// A descendant that hops pids: each process forks the next and exits,
/// so at any moment one or two are alive and none for long. It closes the
/// pipes first, so only the kernel's child list can find it: each link's
/// child is reparented to us before the link is a zombie, so reaping the
/// links leads to the live one. The input fails, nothing is placed, no
/// hop survives. The chain proves it ran by its first process's warning.
///
/// The chain is bounded (100 000 links, at the ~1-2 thousand forks a second
/// a shell manages: one to two minutes), so a harness killed from outside
/// in this test leaves an orphan that ends by itself, not one that burns a
/// core for ever. The bound is far above what the test needs: the chain is
/// killed within a fraction of a second of the ack.
#[test]
fn descendant_fork_chain_is_caught() {
    let t = Tmp::new("desc-hop");
    let a = t.input("a.txt", "a");
    let token = format!("44.{}{}", std::process::id(), line!());
    let bin = t.script_in(
        "/bin/bash",
        "hop",
        &format!(
            "work=$4; in=$(head -n1 \"$6\"); mkdir -p \"$work/0\"\n\
             bash -c 'printf \"warn\\t%s\\thop\\n\" \"$1\"; exec >&- 2>&-; hop() {{ [ $1 -lt 100000 ] || exit 0; ( hop $(($1+1)) ) & exit 0; }}; hop 0' {token} \"$in\" &\n\
             n=0; until [ \"$(cat \"$in\")\" = ack ]; do n=$((n+1)); [ $n -le 3000 ] || exit 1; sleep 0.01; done\n\
             echo x > \"$work/0/a.md\"\n\
             printf 'done\\t%s\\t0/a.md\\n' \"$in\""
        ),
    );
    let act = action(&bin, "md");
    let mut w = Watch::new(&token);
    w.ack = Some(("hop".to_owned(), a.clone()));
    w.cancel_at = Some(std::time::Instant::now() + Duration::from_secs(10));
    run_with(&[&act], std::slice::from_ref(&a), &mut w);
    assert!(
        w.ran
            .outcomes
            .iter()
            .any(|o| matches!(o, Outcome::Warned { message, .. } if message == "hop")),
        "the chain never ran: {:?}",
        w.ran.outcomes
    );
    assert_eq!(
        failed(&w.ran),
        [(a.clone(), "action left processes behind".to_owned())]
    );
    assert!(!a.with_extension("").exists(), "an output was placed");
    // Give a hop that escaped the kill time to show itself.
    std::thread::sleep(Duration::from_millis(100));
    assert!(w.alive().is_empty(), "left alive: {:?}", w.alive());
    assert!(t.work_dirs_left().is_empty());
}

/// Cancels from inside the run, as the action reports its result and
/// exits: the observer sends the cancel, then waits on the action's pidfd
/// for its exit, so the cancel and the exit reach the supervisor in the
/// same wake. The cancel wins: nothing is placed.
#[test]
fn cancel_in_the_same_wake_as_the_exit_is_kept() {
    struct CancelAtStart(common::Ran);
    impl Observer for CancelAtStart {
        fn event(&mut self, o: &Outcome) {
            self.0.event(o);
        }
        fn dir(&mut self, path: &std::path::Path) {
            self.0.dir(path);
        }
        fn progress(&mut self, d: usize, t: usize, f: f32) {
            self.0.progress(d, t, f);
        }
        fn started(&mut self, pid: u32) {
            use std::os::fd::FromRawFd;
            // SAFETY: SIGTERM to this (the supervisor's) thread, where the
            // cancel token blocks it: it only becomes readable on the
            // token's signalfd.
            unsafe { libc::pthread_kill(libc::pthread_self(), libc::SIGTERM) };
            // Then wait, on the action's own pidfd, until it has exited:
            // the supervisor's next poll finds both ready, whatever the
            // machine's speed. (Bounded: a broken fixture fails the test.)
            // SAFETY: pidfd_open takes a pid and flags 0; the supervisor
            // has not reaped the action yet (it is running this callback).
            let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0u32) };
            assert!(fd >= 0, "pidfd_open: {}", std::io::Error::last_os_error());
            // SAFETY: a new fd that we now own.
            let fd = unsafe { std::os::fd::OwnedFd::from_raw_fd(fd as i32) };
            let mut p = libc::pollfd {
                fd: fd.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: one valid pollfd.
            let r = unsafe { libc::poll(&mut p, 1, 5000) };
            assert_eq!(r, 1, "the action did not exit");
        }
    }
    let t = Tmp::new("cancel-exit");
    let a = t.input("a.txt", "a");
    let bin = t.script(
        "quick",
        "work=$4; in=$(head -n1 \"$6\"); mkdir -p \"$work/0\"; echo x > \"$work/0/a.md\"\n\
         printf 'progress\\t0.5\\ndone\\t%s\\t0/a.md\\n' \"$in\"",
    );
    let act = action(&bin, "md");
    let mut obs = CancelAtStart(common::Ran::default());
    run_with(&[&act], std::slice::from_ref(&a), &mut obs);
    assert_eq!(failed(&obs.0), [(a.clone(), "cancelled".to_owned())]);
    assert!(!a.with_extension("").exists(), "an output was placed");
    assert!(t.work_dirs_left().is_empty());
}

/// A contract violation gets the group our SIGTERM; a descendant that
/// ignores it is still alive at the leader's exit. Nothing is placed,
/// but the reason stays the violation, not "left processes behind". The
/// descendant proves it ran by its marker file, relayed by the leader.
#[test]
fn violation_keeps_its_reason_over_a_surviving_descendant() {
    let t = Tmp::new("viol-bg");
    let a = t.input("a.txt", "a");
    let token = format!("45.{}{}", std::process::id(), line!());
    let bin = t.script_in(
        "/bin/bash",
        "viol",
        &format!(
            "work=$4; in=$(head -n1 \"$6\")\n\
             (trap '' TERM; : > \"$work/up\"; exec sleep {token}) &\n\
             n=0; while [ ! -e \"$work/up\" ]; do n=$((n+1)); [ $n -le 3000 ] || exit 1; sleep 0.01; done\n\
             printf 'warn\\t%s\\tbg up\\n' \"$in\"\n\
             printf 'bogus\\n'\n\
             sleep {token}"
        ),
    );
    let act = action(&bin, "md");
    let mut w = Watch::new(&token);
    // Ends a broken fixture instead of hanging.
    w.cancel_at = Some(std::time::Instant::now() + Duration::from_secs(10));
    let took = run_with(&[&act], std::slice::from_ref(&a), &mut w);
    assert!(
        w.ran
            .outcomes
            .iter()
            .any(|o| matches!(o, Outcome::Warned { message, .. } if message == "bg up")),
        "the descendant never ran: {:?}",
        w.ran.outcomes
    );
    let f = failed(&w.ran);
    assert!(
        matches!(&f[..], [(_, r)] if r.starts_with("contract violation: ")),
        "{f:?}"
    );
    assert!(w.alive().is_empty(), "left alive: {:?}", w.alive());
    assert!(took < Duration::from_secs(5), "{took:?}");
    assert!(t.work_dirs_left().is_empty());
}

// ---- Per-input publication ----

type Prepare = Box<dyn FnMut(&engine::exec::OutputGroup, &std::path::Path) -> std::io::Result<()>>;

/// Records the publication callbacks next to the outcomes; `on_prepare`
/// runs in `prepare_group` (the last point before an input's commit).
#[derive(Default)]
struct Pub {
    ran: common::Ran,
    prepared: Vec<(engine::exec::OutputGroup, std::path::PathBuf, u32)>,
    groups: Vec<(
        engine::exec::OutputGroup,
        engine::exec::LocationEvidence,
        engine::exec::Durability,
    )>,
    on_prepare: Option<Prepare>,
}

impl Observer for Pub {
    fn event(&mut self, o: &Outcome) {
        self.ran.event(o);
    }
    fn dir(&mut self, path: &std::path::Path) {
        self.ran.dir(path);
    }
    fn progress(&mut self, d: usize, t: usize, f: f32) {
        self.ran.progress(d, t, f);
    }
    fn prepare_group(
        &mut self,
        g: &engine::exec::OutputGroup,
        private: &std::path::Path,
    ) -> std::io::Result<()> {
        use std::os::unix::fs::MetadataExt;
        let mode = std::fs::symlink_metadata(private).map_or(0, |m| m.mode() & 0o7777);
        self.prepared.push((g.clone(), private.to_owned(), mode));
        match &mut self.on_prepare {
            Some(f) => f(g, private),
            None => Ok(()),
        }
    }
    fn group(
        &mut self,
        g: &engine::exec::OutputGroup,
        l: &engine::exec::LocationEvidence,
        d: &engine::exec::Durability,
    ) {
        self.groups.push((g.clone(), l.clone(), d.clone()));
        if let Some(p) = &l.path_at_check {
            self.ran.dir(p);
        }
    }
}

/// Each input copied to `<work>/<i>/o.md`, except `c.txt`, which fails.
const COPY_BUT_C: &str = "work=$4; i=0\n\
     while IFS= read -r in; do case \"$in\" in\n\
     *c.txt) printf 'fail\\t%s\\tbroken\\n' \"$in\";;\n\
     *) mkdir -p \"$work/$i\"; cat \"$in\" > \"$work/$i/o.md\"; printf 'done\\t%s\\t%s/o.md\\n' \"$in\" $i;;\n\
     esac; i=$((i+1)); done < \"$6\"";

fn names(dir: &std::path::Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    v.sort();
    v
}

/// Four inputs, one fails: the other three are published, each in its own
/// folder, each by its own commit; the failure takes none of them back.
#[test]
fn publication_is_per_input() {
    let t = Tmp::new("pub-per-input");
    let ins: Vec<_> = ["a", "b", "c", "d"]
        .iter()
        .map(|n| t.input(&format!("{n}.txt"), n))
        .collect();
    let bin = t.script("copy", COPY_BUT_C);
    let mut obs = Pub::default();
    run_with(&[&action(&bin, "md")], &ins, &mut obs);
    assert_eq!(failed(&obs.ran), [(ins[2].clone(), "broken".to_owned())]);
    assert_eq!(dones(&obs.ran), 3, "{:?}", obs.ran.outcomes);
    let docs = t.0.join("docs");
    assert_eq!(
        names(&docs),
        ["a", "a.txt", "b", "b.txt", "c.txt", "d", "d.txt"]
    );
    for n in ["a", "b", "d"] {
        assert_eq!(names(&docs.join(n)), [format!("{n}.md")]);
        assert_eq!(
            std::fs::read_to_string(docs.join(n).join(format!("{n}.md"))).unwrap(),
            n
        );
    }
    assert_eq!(obs.prepared.len(), 3);
    assert_eq!(obs.groups.len(), 3);
    assert_eq!(
        obs.ran.dirs,
        [docs.join("a"), docs.join("b"), docs.join("d")]
    );
    assert!(t.work_dirs_left().is_empty());
}

/// One input's second output fails its preparation (not UTF-8 text): of
/// that input nothing becomes visible and nothing is `Done`; the input
/// before it keeps its publication.
#[test]
fn publication_is_atomic_for_one_input() {
    let t = Tmp::new("pub-atomic");
    let a = t.input("a.txt", "a");
    let b = t.input("b.txt", "b");
    let bin = t.script(
        "two",
        "work=$4; i=0\n\
         while IFS= read -r in; do mkdir -p \"$work/$i\"; cat \"$in\" > \"$work/$i/p1.md\"\n\
         if [ $i = 1 ]; then printf '\\377' > \"$work/$i/p2.md\"; else cat \"$in\" > \"$work/$i/p2.md\"; fi\n\
         printf 'done\\t%s\\t%s/p1.md\\n' \"$in\" $i; printf 'done\\t%s\\t%s/p2.md\\n' \"$in\" $i\n\
         i=$((i+1)); done < \"$6\"",
    );
    let mut obs = Pub::default();
    run_with(&[&action(&bin, "md")], &[a.clone(), b.clone()], &mut obs);
    let f = failed(&obs.ran);
    assert_eq!(f.len(), 1, "{:?}", obs.ran.outcomes);
    assert_eq!(f[0].0, b);
    assert!(f[0].1.starts_with("cannot publish output: "), "{}", f[0].1);
    let done_inputs: Vec<_> = obs
        .ran
        .outcomes
        .iter()
        .filter_map(|o| match o {
            Outcome::Done { input, .. } => Some(input.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(done_inputs, [a.clone(), a.clone()]);
    let docs = t.0.join("docs");
    assert_eq!(names(&docs), ["a", "a.txt", "b.txt"]);
    assert_eq!(names(&docs.join("a")), ["p1.md", "p2.md"]);
    assert!(obs.prepared.len() == 1 && obs.groups.len() == 1);
    assert!(t.work_dirs_left().is_empty());
}

/// The folder can be written but not read, so its sync after the commit is
/// impossible: the output stays, `Done` stands, and the durability is
/// reported as unconfirmed (to the group callback and as a warning).
#[test]
fn publication_keeps_committed_outputs_on_sync_error() {
    use std::os::unix::fs::PermissionsExt;
    // SAFETY: geteuid has no preconditions and cannot fail.
    if unsafe { libc::geteuid() } == 0 {
        eprintln!("SKIPPED publication_keeps_committed_outputs_on_sync_error: root reads anything");
        return;
    }
    let t = Tmp::new("pub-sync");
    let a = t.input("a.txt", "a");
    let bin = t.script("copy", COPY_BUT_C);
    let docs = t.0.join("docs");
    std::fs::set_permissions(&docs, std::fs::Permissions::from_mode(0o300)).unwrap();
    let mut obs = Pub::default();
    run_with(&[&action(&bin, "md")], std::slice::from_ref(&a), &mut obs);
    std::fs::set_permissions(&docs, std::fs::Permissions::from_mode(0o700)).unwrap();
    let [(_, location, engine::exec::Durability::Unconfirmed(why))] = &obs.groups[..] else {
        panic!("{:?}", obs.groups)
    };
    assert!(why.contains("Permission denied"), "{why}");
    assert_eq!(location.path_at_check, Some(docs.join("a")));
    assert!(
        matches!(&obs.ran.outcomes[..], [Outcome::Warned { message, .. }, Outcome::Done { output, .. }]
            if message.starts_with("saved, but not confirmed on disk: ") && *output == docs.join("a/a.md")),
        "{:?}",
        obs.ran.outcomes
    );
    assert_eq!(std::fs::read_to_string(docs.join("a/a.md")).unwrap(), "a");
}

/// A failure of the observer's preparation (the journal intent) stops only
/// that input, before its commit.
#[test]
fn publication_refused_by_the_observer_stops_only_that_input() {
    let t = Tmp::new("pub-prepare");
    let ins: Vec<_> = ["a", "b", "d"]
        .iter()
        .map(|n| t.input(&format!("{n}.txt"), n))
        .collect();
    let bin = t.script("copy", COPY_BUT_C);
    let mut n = 0;
    let mut obs = Pub {
        on_prepare: Some(Box::new(move |_, _| {
            n += 1;
            if n == 2 {
                Err(std::io::Error::other("intent not written"))
            } else {
                Ok(())
            }
        })),
        ..Pub::default()
    };
    run_with(&[&action(&bin, "md")], &ins, &mut obs);
    assert_eq!(
        failed(&obs.ran),
        [(
            ins[1].clone(),
            "cannot publish output: intent not written".to_owned()
        )]
    );
    assert_eq!(
        names(&t.0.join("docs")),
        ["a", "a.txt", "b.txt", "d", "d.txt"]
    );
    assert_eq!(obs.groups.len(), 2);
    // The refused input's private copy is gone with it.
    assert!(!obs.prepared[1].1.exists());
    assert!(t.work_dirs_left().is_empty());
}

/// A cancel that arrives while the outputs are published stops at the
/// next input's commit: what was published stays, the rest fails.
#[test]
fn publication_cancel_stops_before_the_next_commit() {
    let t = Tmp::new("pub-cancel");
    let a = t.input("a.txt", "a");
    let b = t.input("b.txt", "b");
    let bin = t.script("copy", COPY_BUT_C);
    let tok = engine::exec::CancelToken::install().unwrap();
    let mut n = 0;
    let mut obs = Pub {
        on_prepare: Some(Box::new(move |_, _| {
            n += 1;
            if n == 2 {
                // SAFETY: SIGTERM to this thread, where the token blocks it.
                unsafe { libc::pthread_kill(libc::pthread_self(), libc::SIGTERM) };
            }
            Ok(())
        })),
        ..Pub::default()
    };
    engine::exec::run(
        &[&action(&bin, "md")],
        &[a.clone(), b.clone()],
        &mut obs,
        &tok,
    );
    assert!(tok.is_cancelled());
    assert_eq!(failed(&obs.ran), [(b, "cancelled".to_owned())]);
    assert_eq!(dones(&obs.ran), 1);
    assert_eq!(names(&t.0.join("docs")), ["a", "a.txt", "b.txt"]);
    assert!(t.work_dirs_left().is_empty());
}

/// The folder's parent is renamed right before the commit: the group is
/// published where the engine's fd points (now under the new name), kept,
/// and reported as `PublishedUnknown` with no path, never as a `Done`
/// with the old one. The work dir is still cleaned (fd-anchored).
#[test]
fn publication_moved_ancestor_is_published_unknown() {
    let t = Tmp::new("pub-moved");
    let docs = t.0.join("deep/docs");
    std::fs::create_dir_all(&docs).unwrap();
    let a = docs.join("a.txt");
    std::fs::write(&a, "a").unwrap();
    let bin = t.script("copy", COPY_BUT_C);
    let (from, to) = (t.0.join("deep"), t.0.join("deep2"));
    let mut obs = Pub {
        on_prepare: Some(Box::new(move |_, _| std::fs::rename(&from, &to))),
        ..Pub::default()
    };
    run_with(&[&action(&bin, "md")], std::slice::from_ref(&a), &mut obs);
    let [
        Outcome::PublishedUnknown {
            input,
            group,
            location,
            reason,
            ..
        },
    ] = &obs.ran.outcomes[..]
    else {
        panic!("{:?}", obs.ran.outcomes)
    };
    assert_eq!(input, &a);
    assert_eq!(*reason, engine::exec::PublicationReason::AncestorChanged);
    assert_eq!(location.path_at_check, None);
    assert_eq!(*group, obs.prepared[0].0.group);
    assert_eq!(location.anchor.op, obs.prepared[0].0.op);
    let moved = t.0.join("deep2/docs");
    assert_eq!(std::fs::read_to_string(moved.join("a/a.md")).unwrap(), "a");
    assert!(obs.ran.dirs.is_empty(), "{:?}", obs.ran.dirs);
    assert_eq!(names(&moved), ["a", "a.txt"]);
}

/// The group's baseline is fixed in private (the final umask modes, one
/// link) and is the same, with the same ids, after the commit; the
/// private path is gone once published.
#[test]
fn publication_baseline_reaches_the_observer_unchanged() {
    use std::os::unix::fs::MetadataExt;
    let t = Tmp::new("pub-baseline");
    let a = t.input("a.txt", "a");
    let bin = t.script("copy", COPY_BUT_C);
    let mut obs = Pub::default();
    run_with(&[&action(&bin, "md")], std::slice::from_ref(&a), &mut obs);
    let [(prepared, private, mode)] = &obs.prepared[..] else {
        panic!("{:?}", obs.prepared)
    };
    let [(group, location, durability)] = &obs.groups[..] else {
        panic!("{:?}", obs.groups)
    };
    // The final modes follow the umask (D-23), set before the baseline.
    let umask = std::fs::read_to_string("/proc/self/status")
        .unwrap()
        .lines()
        .find_map(|l| l.strip_prefix("Umask:"))
        .map(|v| u32::from_str_radix(v.trim(), 8).unwrap())
        .unwrap();
    let (dmode, fmode) = (0o777 & !umask, 0o666 & !umask);
    assert_eq!(*mode, dmode);
    assert!(!private.exists());
    assert_eq!(prepared, group);
    assert_eq!(*durability, engine::exec::Durability::Synced);
    let w = t.0.join("docs/a");
    assert_eq!(location.path_at_check.as_deref(), Some(w.as_path()));
    let wm = std::fs::symlink_metadata(&w).unwrap();
    assert_eq!(
        (group.id.dev, group.id.ino, group.id.mode),
        (wm.dev(), wm.ino(), dmode)
    );
    let [c] = &group.children[..] else {
        panic!("{:?}", group.children)
    };
    let cm = std::fs::symlink_metadata(w.join(&c.rel)).unwrap();
    assert_eq!(c.kind, engine::exec::NodeKind::File);
    assert_eq!((c.uid, c.mode, c.nlink), (cm.uid(), fmode, 1));
    assert_eq!(
        (c.file_id.dev, c.file_id.ino, c.file_id.size),
        (cm.dev(), cm.ino(), 1)
    );
    assert!(
        matches!(&obs.ran.outcomes[..], [Outcome::Done { output, id, .. }] if *output == w.join("a.md") && *id == c.file_id),
        "{:?}",
        obs.ran.outcomes
    );
}
