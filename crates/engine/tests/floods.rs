//! Actions that flood stdout: memory stays bounded and a cancel still
//! lands. Its own test binary, so the peak RSS is not shared with other
//! tests; the two floods run one at a time.

mod common;

use common::{Tmp, action, failed, run, vm_hwm_kb};
use engine::exec::Outcome;
use std::sync::Mutex;
use std::time::Duration;

static SERIAL: Mutex<()> = Mutex::new(());

#[test]
fn warning_flood_keeps_memory_bounded() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let t = Tmp::new("warnflood");
    let a = t.input("a.txt", "a");
    let bin = t.script(
        "warnflood",
        &format!(
            "in=$(head -n1 \"$6\"); exec yes \"$(printf 'warn\\t%s\\t{}' \"$in\")\"",
            "m".repeat(80)
        ),
    );
    let act = action(&bin, "md");
    let before = vm_hwm_kb();
    let (r, latency) = run(
        &[&act],
        std::slice::from_ref(&a),
        Some(Duration::from_millis(1500)),
    );
    let grown = vm_hwm_kb().saturating_sub(before);
    // 16 warnings, the "<n> more warnings" summary, the cancel.
    assert!(r.outcomes.len() <= 18, "{} outcomes", r.outcomes.len());
    assert!(
        r.outcomes
            .iter()
            .any(|o| matches!(o, Outcome::Failed { reason, .. } if reason == "cancelled")),
        "{:?}",
        r.outcomes.iter().rev().take(3).collect::<Vec<_>>()
    );
    assert!(r.outcomes.iter().any(
        |o| matches!(o, Outcome::Warned { message, .. } if message.ends_with(" more warnings"))
    ));
    assert!(grown < 32 * 1024, "peak RSS grew by {grown} KiB");
    assert!(
        latency < Duration::from_millis(500),
        "cancel took {latency:?}"
    );
}

#[test]
fn progress_flood_still_cancels_fast() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let t = Tmp::new("flood");
    let a = t.input("a.txt", "a");
    let bin = t.script("flood", "exec yes \"$(printf 'progress\\t0.5')\"");
    let act = action(&bin, "md");
    let before = vm_hwm_kb();
    let (r, latency) = run(
        &[&act],
        std::slice::from_ref(&a),
        Some(Duration::from_millis(300)),
    );
    let grown = vm_hwm_kb().saturating_sub(before);
    assert_eq!(failed(&r), [(a, "cancelled".to_owned())]);
    assert!(
        latency < Duration::from_millis(500),
        "cancel took {latency:?}"
    );
    assert!(grown < 32 * 1024, "peak RSS grew by {grown} KiB");
}

#[test]
fn many_long_outputs_keep_memory_bounded() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let t = Tmp::new("longout");
    let a = t.input("a.txt", "a");
    // 1024 outputs (the cap) with paths of about 1000 bytes.
    let deep: Vec<String> = (0..4).map(|k| format!("{k}{}", "x".repeat(239))).collect();
    let deep = deep.join("/");
    let bin = t.script(
        "longout",
        &format!(
            "in=$(head -n1 \"$6\"); rel=0/{deep}; d=\"$4/$rel\"; mkdir -p \"$d\"\n\
             i=0; while [ $i -lt 1024 ]; do : > \"$d/f$i\"; printf 'done\\t%s\\t%s\\n' \"$in\" \"$rel/f$i\"; i=$((i+1)); done"
        ),
    );
    let act = action(&bin, "md");
    let before = vm_hwm_kb();
    let (r, _) = run(&[&act], std::slice::from_ref(&a), None);
    let grown = vm_hwm_kb().saturating_sub(before);
    let done = r
        .outcomes
        .iter()
        .filter(|o| matches!(o, Outcome::Done { .. }))
        .count();
    assert_eq!(
        done,
        1024,
        "{:?}",
        r.outcomes.iter().take(3).collect::<Vec<_>>()
    );
    assert!(grown < 32 * 1024, "peak RSS grew by {grown} KiB");
}
