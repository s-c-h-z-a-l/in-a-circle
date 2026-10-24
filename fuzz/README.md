# Fuzzing

libFuzzer targets through `cargo-fuzz`. Not part of the root workspace and
not shipped; the shipped dependency tree is unchanged.

- Toolchain: `nightly-2026-09-01` (cargo-fuzz and libFuzzer need a nightly;
  the date is more than two weeks older than the day it was chosen,
  2026-10-24). The root `rust-toolchain.toml` pins stable, so always go
  through `scripts/fuzz.sh`, which names the nightly explicitly.
- Tool: `cargo install --locked cargo-fuzz@0.13.2`.
- Modes: `scripts/fuzz.sh smoke|close|baseline|cmin [target...]`. Durations
  and per-target `-max_len`, `-timeout`, `-rss_limit_mb` are explained in the
  script.
- Corpus: `fuzz/corpus/<target>/`, seeded from real manifests and the
  CommonMark examples; keep it minimized with `scripts/fuzz.sh cmin`.
- A finding is fixed in the crate and its minimized input becomes a
  regression test there.
