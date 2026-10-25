# Fuzzing

libFuzzer targets through `cargo-fuzz`. Not part of the root workspace and
not shipped; the shipped dependency tree is unchanged.

- Toolchain: `nightly-2026-09-01` (cargo-fuzz and libFuzzer need a nightly;
  the date is more than two weeks older than the day it was chosen,
  2026-10-24). The root `rust-toolchain.toml` pins stable, so always go
  through `scripts/fuzz.sh`, which names the nightly explicitly.
- Tool: `cargo install --locked cargo-fuzz@0.13.2`.
- Modes: `scripts/fuzz.sh smoke|close|baseline|cmin [target...]`. Durations
  and per-target `-max_len`, `-timeout`, `-malloc_limit_mb` (per input), `-rss_limit_mb` (whole process) are explained in the
  script.
- Corpus: `fuzz/corpus/<target>/`, seeded from real manifests, a real cache
  and history, fake-compositor messages and the CommonMark examples; keep it minimized with `scripts/fuzz.sh cmin`.
- Corpus size policy, in content bytes (`du -sb fuzz/corpus`; plain `du`
  counts 4 KiB disk blocks per file and overstates a corpus of many tiny
  files about 9x): 5,631,819 bytes (5.4 MiB) in 11923 files at the end of
  plan 7; markdown is 3.6 MB of it (CommonMark examples and its own campaign
  finds). After every `close` or `baseline` campaign run `scripts/fuzz.sh
  cmin` and commit the minimized corpus, never the raw grown one. Upper
  bound: 16 MiB of content in total, about 3x today. libFuzzer loads the
  whole corpus into RAM at start and keeps per-input overhead on top, so a
  smoke run's peak RSS (450-583 MB measured, ceiling 1024 MB in
  `scripts/fuzz.sh`) grows with the content; 16 MiB leaves that margin
  intact and keeps the clone small.
  Past it, drop seeds that cmin keeps only for marginal coverage, or
  re-seed that target. The `wire_events` seed files encode interface
  indices, so they depend on the order of `Iface::ALL`
  (`crates/wire/src/proto.rs`): reordering or inserting an interface there
  invalidates them, and the seeds must be regenerated.
- A finding is fixed in the crate and its minimized input becomes a
  regression test there.
