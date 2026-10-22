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
    assert!(
        latency < Duration::from_millis(500),
        "cancel took {latency:?}"
    );
    assert!(!process_with(&token));
    assert!(t.work_dirs_left().is_empty());
}
