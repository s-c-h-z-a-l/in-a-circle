//! Launches `probe` N times, either through niri's spawn path (what a keybind
//! does) or directly, and prints per-phase latency percentiles from t0.
//!
//! Usage: launch <niri|direct> <runs> [--noop]

use std::process::Command;
use std::time::Duration;
use std::{fs, thread};

fn mono() -> u64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: valid out-pointer.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64
}

fn pct(v: &mut [f64], p: f64) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[((v.len() - 1) as f64 * p).round() as usize]
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let mode = args[1].clone();
    let runs: usize = args[2].parse().unwrap();
    let noop = args.iter().any(|a| a == "--noop");
    let exe = std::env::current_exe().unwrap();
    let probe = exe.with_file_name("probe");
    let out = std::env::temp_dir().join(format!("inacircle-probe-{}.txt", std::process::id()));
    let _ = fs::remove_file(&out);

    let mut msg_ms = Vec::new();
    for i in 0..runs {
        let t0 = mono();
        let mut pargs = vec![
            probe.to_str().unwrap().to_string(),
            "--t0".into(),
            t0.to_string(),
            "--out".into(),
            out.to_str().unwrap().to_string(),
        ];
        if noop {
            pargs.push("--noop".into());
        }
        if mode == "niri" {
            let st = Command::new("niri")
                .args(["msg", "action", "spawn", "--"])
                .args(&pargs)
                .status()
                .unwrap();
            assert!(st.success());
            msg_ms.push((mono() - t0) as f64 / 1e6);
        } else {
            Command::new(&pargs[0]).args(&pargs[1..]).status().unwrap();
        }
        // Wait for the probe to report.
        for _ in 0..2000 {
            if fs::read_to_string(&out).map_or(0, |s| s.lines().count()) > i {
                break;
            }
            thread::sleep(Duration::from_millis(1));
        }
        thread::sleep(Duration::from_millis(250));
    }

    let text = fs::read_to_string(&out).unwrap();
    let rows: Vec<Vec<(String, u64)>> = text
        .lines()
        .map(|l| {
            l.split_whitespace()
                .filter_map(|kv| {
                    let (k, v) = kv.split_once('=')?;
                    Some((k.to_string(), v.parse().ok()?))
                })
                .collect()
        })
        .collect();
    let get = |r: &[(String, u64)], k: &str| r.iter().find(|(n, _)| n == k).map(|(_, v)| *v);

    println!("mode={mode} runs={} noop={noop}", rows.len());
    if let Some(r) = rows.first()
        && let (Some(c), Some(rf)) = (get(r, "clk"), get(r, "refresh"))
    {
        println!(
            "presentation clk_id={c} (1=MONOTONIC) refresh_ns={rf} flags={:?}",
            get(r, "flags")
        );
    }
    if !msg_ms.is_empty() {
        println!(
            "{:<22} p50={:7.3} p90={:7.3} ms",
            "niri msg returned",
            pct(&mut msg_ms, 0.5),
            pct(&mut msg_ms, 0.9)
        );
    }
    let mut prev = "t0".to_string();
    for k in [
        "main",
        "connect",
        "registry",
        "buffer",
        "configure",
        "commit",
        "frame",
        "present",
    ] {
        let mut abs: Vec<f64> = rows
            .iter()
            .filter_map(|r| Some((get(r, k)?.checked_sub(get(r, "t0")?)?) as f64 / 1e6))
            .collect();
        let mut step: Vec<f64> = rows
            .iter()
            .filter_map(|r| Some((get(r, k)?.checked_sub(get(r, &prev)?)?) as f64 / 1e6))
            .collect();
        if abs.is_empty() {
            continue;
        }
        println!(
            "{k:<10} from t0 p50={:7.3} p90={:7.3} ms | step from {prev:<9} p50={:7.3} p90={:7.3} ms",
            pct(&mut abs, 0.5),
            pct(&mut abs, 0.9),
            pct(&mut step, 0.5),
            pct(&mut step, 0.9),
        );
        prev = k.to_string();
    }
    let _ = fs::remove_file(&out);
}
