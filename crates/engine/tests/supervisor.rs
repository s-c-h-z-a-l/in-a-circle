//! The supervisor loop against misbehaving fake actions.

mod common;

use common::{Tmp, action, failed, process_with, run};
use engine::exec::Outcome;
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

/// `TERM_GRACE` in `exec.rs`: how long a cancelled action may ignore SIGTERM.
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
        // The grace, then up to `GROUP_GONE` (1 s) for the group to vanish.
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
    assert_eq!(
        failed(&r),
        [(a, "intermediate output changed".to_owned())],
        "{:?}",
        r.outcomes
    );
    assert_eq!(dones(&r), 0);
    assert_eq!(placed_files(&t), 0);
    assert_no_secret_placed(&t);
    assert!(t.work_dirs_left().is_empty());
}
