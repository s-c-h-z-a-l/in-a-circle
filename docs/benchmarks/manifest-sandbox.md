# The sandboxed manifest run

**Question:** `<family> --manifest` now runs in a sandbox (Landlock ruleset
build, `pre_exec` with rlimits, `no_new_privs`, seccomp, `close_range`).
What does that add to a manifest run, and does it fit the choice-to-start
budget of 1 ms of our CPU?

**Verdict:** within budget. The sandbox adds about 0.22 ms to the time until
`spawn` returns (p50 285-302 µs against 63-67 µs for a plain spawn) and
about 0.16 ms to the time until the manifest is read and the child reaped
(p50 1.58-1.62 ms against 1.42-1.45 ms). These are wall times of the
supervisor, which is blocked on the child's `exec` during `spawn`, so our
CPU is lower still. A manifest runs only for a new or changed binary (a
cache hit costs a few `stat` calls and no spawn), so a normal choice pays
nothing; the first start after an install or update pays this once per
binary. The worst `spawn` seen was 479 µs.

## Method

`manifest_sandbox_cost` (an `#[ignore]` test in
`crates/engine/src/catalog.rs`) runs the built-in `inacircle-text` binary
with `--manifest` 300 times per variant, after 20 warm-up runs, alternating
the variants within one loop so drift hits both:

- **sandboxed:** `spawn_manifest`, the code `capture` uses (ruleset build,
  `pre_exec` limits, Landlock, seccomp), then `read_limited` and
  `wait_until`.
- **plain:** the old unsandboxed spawn: `Command` with `--manifest`, null
  stdin, piped stdout, null stderr, then the same read and wait.

Two times from the start of each run (monotonic `Instant`): until `spawn`
returns, and until the output is read to EOF and the child is reaped.
Reported as p50 and max over the 300 runs. The binary is warm in the page
cache.

## Machine

The reference machine of [README.md](README.md#reference-machine),
including `mitigations=off`: syscall-heavy costs like these will be
somewhat higher on a stock kernel.

## Numbers

Three consecutive runs, microseconds:

| run | variant | spawn p50 | spawn max | output p50 | output max |
|---|---|---|---|---|---|
| 1 | sandboxed | 302 | 377 | 1617 | 1758 |
| 1 | plain | 67 | 105 | 1452 | 1585 |
| 2 | sandboxed | 285 | 345 | 1588 | 1727 |
| 2 | plain | 63 | 89 | 1419 | 1525 |
| 3 | sandboxed | 281 | 479 | 1581 | 1816 |
| 3 | plain | 64 | 439 | 1421 | 1646 |

The difference is about 220 µs at `spawn` and 160-170 µs at the output; the
output time is dominated by the child's own start (exec, loader, printing),
which the filter and ruleset barely slow down.

## Reproduce

```
cargo build --release --bins
INACIRCLE_REQUIRE_LANDLOCK=1 cargo test --release -p engine --lib \
    manifest_sandbox_cost -- --ignored --nocapture
```

`INACIRCLE_BENCH_BIN` names another binary that answers `--manifest`
(default: `target/release/inacircle-text`). The test prints one `sandboxed`
and one `plain` line with the four figures.
