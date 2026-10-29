# Wheel startup, teardown and failure-notice deadlines

Decision: the wheel's start has its own 2 s clock, apart from the user's
30 s wait; its close waits at most 1 s for the compositor; the failure
notice costs at most 500 ms to open and 500 ms to send. Policy ceilings,
chosen from the numbers below. They are not a presentation guarantee: no
physical frame was sampled.

## Method

A fake compositor (`bins/inacircle/tests/fake_compositor`) and a fake
notification server (`crates/dbus/tests/common`) on private sockets, real
`inacircle` binary, debug build. The fake answers on a script: the
`configure` at once, after a delay, or never; the answer to the teardown's
`sync` the same way; the seat's devices. Delays are timers polled by the
fake's own loop. Each ceiling has a named pair of cases, 100 ms below and
above it (the RPC pair 50 ms), and the notice ordering is checked with a
barrier, not a timing guess.

Machine: the development machine (x86_64, Arch-based, kernel 7.x, niri
session running beside the tests but never touched). A second, untuned
machine was not available for this fake-compositor policy check; the
budgeted paths it does not change (start CPU, hover, memory) were
re-measured on the release build (below).

## Numbers

Normal startup on the fake (HEAD before the change, 3 x 40 spawns): process
spawn to the overlay's first buffer 4.3 ms, to the wheel's first commit
5.3 ms (p50; the fake's accept loop is part of it); a close to the process
exit 0.18 ms. The 2 s ceiling is 400 times the normal start.

| Case (10 runs each) | Result |
|---|---|
| configure at 1900 ms | wheel commits at 1905.4 - 1907.3 ms |
| configure at 2100 ms, or never | gives up at 2003.0 - 2008.9 ms, exit 1, cause named |
| teardown answer at 900 ms | waits, leaves 901.3 - 901.9 ms after the layer destroy |
| teardown answer at 1100 ms | gives up, leaves 999.5 - 1000.6 ms after the destroy |
| `Notify` answered after 450 ms | notice shown |
| `Notify` answered after 550 ms | given up (D-Bus timeout), one stderr line |
| open in 3 steps of 140 ms (420 ms) | notice shown |
| open in 3 steps of 200 ms (600 ms) | given up: one 500 ms frame, not per step |

Release build, fake compositor, with the change (measured at `86a9b4a`,
before the startup paint moved; for the current numbers see `wheel.md` and
`hardening-performance.md`): main to the wheel's
commit 604-620 us CPU (budget 1 ms), PSS at stage 2 with a 1024-file drop
2.56 MB (budget 5 MB), `inacircle` 1,076,736 B (+6.9 kB over the previous
commit; budget 1.5 MB).

Hover repaint, from an independent run of 122 hover events per run, 10 runs
per build, fake compositor, policy evidence (not a presentation
measurement): before the hardening: p50 17 us, p95 35-39 us, max 123-156 us;
this commit: p50 17-18 us, p95 35-38 us, max 129-169 us. The typical event
is far inside the 100 us budget; the max is the first one or two repaints
after a stage change (about 140 us) and exceeds it in both builds. That
overrun predates the hardening and is handled by the startup-paint work in `wheel.md`, not here.

## Verdict

2000 / 1000 / 500 / 500 ms hold: each pair separates cleanly, with the
measured edge within 10 ms of the ceiling. 30 s (user wait), 500 ms (drop
pipe) and 500 ms (soft margin) are unchanged and unmixed with the start.

## Reproduce

```
cargo test -p inacircle --test wheel --test notify startup_ -- --nocapture
```

The `PREFLIGHT` lines are the figures above.
