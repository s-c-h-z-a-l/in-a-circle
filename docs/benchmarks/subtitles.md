# Subtitle actions

What the subtitle family (SRT, WebVTT, ASS/SSA conversions) costs in memory,
binary size and classifier time, what its limits protect, and what is still
open. Gates and rules: [acceptance.md](acceptance.md); limits:
[`resource-limits.md`](../resource-limits.md#subtitles).

**Status: measured here, on one machine.** The memory series, the size check
and the classifier micro-measurement below were run for this page. The test,
coverage, fuzzing, mutation and differential figures are the recorded results
of the closing runs on this branch. The two-machine drag timings, the second
machine and the TV check are **open** (see [Verdict](#verdict)).

## Machine

| | |
|---|---|
| CPU | AMD Ryzen 7 5700X 8-Core (16 threads), x86_64; the tuned reference desktop's CPU model |
| Kernel | Linux 7.3.1-1-cachyos; `mitigations=off`, `split_lock_detect=off`, `tsc=reliable`; governor `performance` (read from sysfs; a fixed clock was not verified) |
| RAM | 94 GB |
| Toolchain | rustc and cargo 1.98.1 (`rust-toolchain.toml`); release profile of the workspace (fat LTO, one codegen unit, `panic=abort`, stripped) |
| Reference | ffmpeg n9.0.2 for the differential tests |

This is not the reference acceptance environment recorded in
[wheel-reference.md](wheel-reference.md): the kernel differs, and no
acceptance environment record was made for this page. The figures are
diagnostic for memory and size, and a CPU micro-measurement for the classifier.

## What was built

Four actions, one format in and one out: `subtitles.srt-to-vtt`,
`subtitles.vtt-to-srt`, `subtitles.ass-to-srt` and `subtitles.ass-to-vtt`.
The classifier types a file by its head: a `.srt` with a `-->` timing
(`application/x-subrip`), a `.vtt` with the `WEBVTT` signature (`text/vtt`),
and a `.ass` or `.ssa` with a `[Script Info]` section (`text/x-ssa`). The
readers are in `crates/subtitles` (standard library, and the `xml` crate for
TTML); the action binary is
`inacircle-subtitles`.

Every conversion is lossy where the target cannot carry a feature. The notice
names the loss, one line per kind: colours, fonts and sizes; exact positions;
karaoke effects; drawings; style and region blocks; speaker names written as
`Name:` prefixes; and characters that look like markup, written as look-alikes.

| Limit | Value | Past it |
|---|---|---|
| input | 8 MiB (`INPUT_BYTES`) | refused before parsing |
| output | 32 MiB (`OUTPUT_BYTES`) | refused while writing; the file is removed |
| time | 99:59:59.999 (`MAX_MS`) | the cue is skipped with a warning |
| items | 2,000,000 (`ITEM_BUDGET`) | refused whole (`too many lines and styled runs (more than 2,000,000)`); nothing is truncated |
| warnings | seven loss kinds and one for skipped cues | at most eight lines per input |

## Memory: why an item budget

An 8 MiB input can hold about 4.2 million two-byte lines (`x\n`). Each line
is a reader `Line` with a run list, and a run holds its own text, so the
reader's memory follows the number of lines and runs, not the bytes. A byte
limit alone did not protect the process: the one-cue, two-byte-line SRT shape
peaked at 906.3 MiB, which is about 227 B per line (906.3 MiB over 4,194,303
lines). The item budget refuses it at 2,000,000 items, before most of that
memory is taken.

The budget is 2,000,000 items. A cue, a line and a run are one item each. At
the budget the densest refused shape peaked at 216.5 MiB, about 113 B per item
over the whole process (the input buffer and the allocator included). The
budget keeps the peak at about a fifth of the 1 GiB `RLIMIT_DATA` floor that
the sandbox sets. Scale against real files: a film subtitle is about 10,000
items (2,000 cues of two lines of two runs), and the densest real 8 MiB file,
the smallest SRT cues (27 B each, about 310,000 cues of three items), is about
930,000 items. The budget sits above both.

### Peak RSS of the real binary, dense 8 MiB inputs

Peak RSS is the child's own `ru_maxrss` from `wait4`, with `RLIMIT_DATA` 1 GiB
and `RLIMIT_FSIZE` 256 MiB, as the test helper sets them. "Before" and
"after" are the figures recorded with the change (test build). The "final"
column is the series of the final tree (`666f61b18`): the release binary, five
runs per shape, MiB, min / median / max. It was run with a throw-away parent
that mirrors the dense test's inputs and limits (see Reproduce). The test-build
column of the earlier series was not re-run on the final tree and is not shown.

| Shape (8 MiB input) | before | after (recorded) | final, release | Outcome |
|---|---|---|---|---|
| SRT, about 310,000 cues of 27 B | 138.4 | 129.0 | 128.9 / 129.0 / 129.0 | kept, exit 0 |
| SRT, one cue of 2-byte lines (`x\n`) | 906.3 | 216.5 | 216.2 / 216.3 / 216.5 | refused (`TooComplex`), exit 1 |
| WebVTT, about 310,000 cues of 27 B | 138.4 | 129.0 | 128.8 / 128.9 / 128.9 | kept, exit 0 |
| ASS, 186,410 cues of 45 B (the dense test's shape) | 87.2 | 81.6 | 81.4 / 81.6 / 81.6 (srt), 81.5 / 81.6 / 81.6 (vtt) | kept, exit 0 |
| ASS, 204,597 cues of 41 B (not in the dense test) | not recorded | not recorded | 88.3 / 88.4 / 88.5 (srt), 88.3 / 88.5 / 88.5 (vtt) | kept, exit 0 |
| ASS, one cue of `x\N` lines (3 B each) | 607.8 | 216.7 | 216.3 / 216.4 / 216.5 | refused (`TooComplex`), exit 1 |

The two ASS cue rows were re-measured at `038bb5c46`, the tree before this
correction, five runs each. The ASS reader is the same as at `666f61b18`, and
the dense shape measures the same with the `666f61b18` binary (81.3 / 81.3 / 81.5
srt, 81.3 / 81.4 / 81.4 vtt). The SRT writer changes since then move it by less than
0.3 MiB.

The earlier release series gave the dense ASS shape 81.2 / 81.5 / 81.6 MiB, and
it measures the same now. The 89.5 MiB this row showed does not reproduce: no
input shape tried gave it, and its input is not available. Peak RSS grows with
the cue count, about 400 B per cue (80.0 MiB at 182,358 cues of 46 B with CRLF
line ends, 81.5 at 186,410, 88.5 at 204,597). So the higher figure belongs to an
input of about 205,000 cues, like the 41-byte-line row, not to the dense shape.
Nothing changed the dense shape's figure, so the earlier attribution to reader
changes is withdrawn: the look-alike rules are writer-only. Both rows are still
far under the 1 GiB net.

The two refused shapes are refused whole: their output is not written. The
speaker-name probe (a 1 MiB name over 400 lines, `vtt-to-srt`) held 406.4 MiB
before the change and 12.1 MiB after. It was recorded with the change and not
re-run here; its output is past the 32 MiB ceiling, so it still fails, after
parsing.

Wall times of the final release series are 0.10 to 0.18 s per shape (`Popen`
to `wait4`). The earlier test-build wall times (0.6 to 1.8 s, which include the
harness's pipe reads in a debug build) are not comparable to them.

## Binary size

`scripts/check-sizes.sh`, release builds of the shipped binaries at HEAD
(`666f61b18`, clean tree). File bytes against the `inacircle` limit of 1 572 864 B
(1.5 MB); `.text` bytes against the 1 MiB family limit (`CLAUDE.md`). Growth
is against the previous line in `scripts/sizes.baseline` (the line before this
refresh).

| Binary | file B, baseline → now (growth) | `.text` B, baseline → now (growth) |
|---|---|---|
| `inacircle` | 1,199,304 → 1,199,368 (+64) | 924,600 → 924,440 (−160) |
| `inacircle-text` | 1,268,024 → 1,268,024 (0) | 901,043 → 901,043 (0) |
| `inacircle-pages` | 514,648 → 514,648 (0) | 412,179 → 412,179 (0) |
| `inacircle-subtitles` | 444,176 → 451,016 (+6,840) | 355,091 → 361,363 (+6,272) |

Every growth is under the 100 KB explanation threshold. The `inacircle` change
is the engine's subtitle head search in the classifier and its check of
subtitle outputs as text, both since the previous line; the net change is
+64 B file and −160 B `.text`. `inacircle-subtitles` grew since its previous
line, and that growth is the subtitle code: the reader fixes from the fuzzing
and review rounds. The `inacircle-text` and `inacircle-pages` binaries are
unchanged. Every `.text` stays under the 1 MiB family limit. The aarch64
baseline was not refreshed.

## Classifier cost

`classify_with` runs on every dropped file before the wheel shows it. The
subtitle types add a search of the 4 KiB head: for `-->` in a `.srt`, for
`[Script Info]` (ASCII case-insensitive) in a `.ass` or `.ssa`; a `.vtt` is a
prefix check. This is a CPU micro-measurement on this machine, **not** the
acceptance measurement of the wheel's budget (drop to second wheel, two
machines, about 50 manual drags each); that is still open.

Method: a throw-away `classify_bench` example, not committed, built in release
mode from two trees: the base `bdb66403` (`main`) and the new tree (the commit
that adds the block search, `classify: a faster head search for the subtitle
names`). Each "drop" is one `Reread::per_drop()` budget and one `classify_with`
call per file, timed with `Instant`. Files: 300 KiB of ASCII, in four sets of
50: *arrow* (`00:00:01,000 --> 00:00:02,000` as the first line of a `.srt`),
*no arrow* (a `.srt` with no `-->`), and `.ass` files with and without a
`[Script Info]` header first. Each run is 2,000 drops; each tree and case ran
in two alternating rounds (base then new, then new then base). Median is the
lower middle, p95 is nearest-rank. The per-drop times include the file open
and the reads that the read-on performs.

**The first version was slower.** The first subtitle search was a plain
`head.windows(needle.len()).any(|w| w.eq_ignore_ascii_case(needle))` scan.
Measured against base, a `.srt` with no arrow cost +92.7 µs per 50-file drop
(+54 %), about 1.9 µs per file, because a window with a case-folded compare
runs at every one of the ~4,000 positions. The search now checks the head in
128-byte blocks for the needle's first byte (both needles open with a byte
that is not a letter, so the byte match is exact) with an OR-fold of byte
compares that the compiler vectorises; only a block that contains the first
byte is compared in full. Results are unchanged: a differential unit test
(`in_head_matches_the_naive_scan`) checks it against the old scan.

A throw-away harness on one 4 KiB head with no match: the window scan
3.5 µs, the block search 0.13 to 0.15 µs. In the wheel's classifier, the
search itself (measured by timing the build with the search stubbed out)
costs about 6 µs per 50-file drop. Block sizes of 256 and 512 bytes measured
within 1 to 2 µs per 50 files of 128 bytes, so 128 stays.

| Case | Tree | median µs (round 1 / 2) | p95 µs (round 1 / 2) |
|---|---|---|---|
| `.srt` arrow, 1 file | base | 21.2 / 21.1 | 26.4 / 22.4 |
| `.srt` arrow, 1 file | new | 25.0 / 24.2 | 26.5 / 26.8 |
| `.srt` arrow, 50 files | base | 170.8 / 168.4 | 174.2 / 172.2 |
| `.srt` arrow, 50 files | new | 432.2 (see below) / 185.8 | 437.9 / 189.2 |
| `.srt` no arrow, 1 file | base | 21.3 / 22.3 | 21.9 / 23.4 |
| `.srt` no arrow, 1 file | new | 24.1 / 24.9 | 30.1 / 25.8 |
| `.srt` no arrow, 50 files | base | 171.2 / 172.2 | 175.5 / 176.0 |
| `.srt` no arrow, 50 files | new | 190.6 / 190.2 | 195.4 / 194.0 |
| `.ass` with `[Script Info]`, 1 file | base | 23.0 / 22.4 | 23.2 / 23.6 |
| `.ass` with `[Script Info]`, 1 file | new | 29.3 / 26.3 | 31.3 / 26.5 |
| `.ass` with `[Script Info]`, 50 files | base | 180.5 / 180.3 | 184.5 / 185.1 |
| `.ass` with `[Script Info]`, 50 files | new | 194.5 / 195.6 | 198.8 / 198.9 |
| `.ass` without header, 1 file | base | 23.1 / 22.1 | 27.5 / 28.9 |
| `.ass` without header, 1 file | new | 25.8 / 25.8 | 27.0 / 28.1 |
| `.ass` without header, 50 files | base | 178.9 / 180.4 | 201.0 / 184.9 |
| `.ass` without header, 50 files | new | 199.7 / 202.6 | 204.7 / 207.0 |

The round-1 arrow 50-file run of the new tree (432.2 µs median, minimum
430.2 µs) was a slowdown of the whole run, not a tail of single drops. Three
repeats of the same command gave medians of 184.0, 186.0 and 183.5 µs; the
table keeps the first value, and the difference below uses the repeats.

Difference of the medians (new minus base, mean of the two rounds):

- no arrow, 50 files: +18.7 µs (+10.9 %). Base 171.7 µs, new 190.4 µs.
  The first version added +92.7 µs (+54 %).
- arrow, 50 files: about +15 µs (+9 %), using the repeats, with the search
  exiting in the first block.
- `.ass` with `[Script Info]`, 50 files: about +15 µs (+8 %).
- one file: +2.7 to +3.4 µs for arrow and no arrow; +3.2 µs for `.ass`
  without a header and +5.1 µs for `.ass` with one (the noisiest pair).

Reading: the no-arrow 50-file drop is now about 11 % above base, a little over
the goal of 10 %. The search accounts for about 6 µs of that (see the method
above). The rest, about 13 µs per 50 files, also shows up when the search
exits at its first block (arrow, `[Script Info]` first) and when the search
is stubbed out of the build, so it comes from the subtitle branch in the
classifier, not from the scan. I did not find its cause; code layout is the
likely suspect, and that is not verified. The no-arrow case is a broken or
non-subtitle `.srt` with a text head, which the wheel would classify as text
anyway, so this is the worst case of the search, not the common one.

## Test and verification evidence

Recorded results of the closing runs on this branch (not re-run for this page):

- Full workspace suite, with `INACIRCLE_REQUIRE_LANDLOCK=1`,
  `INACIRCLE_REQUIRE_PDF_TOOLS=1` and `INACIRCLE_REQUIRE_SUBTITLE_TOOLS=1`:
  debug 2330 passed / 0 failed / 6 ignored; release 2330 passed / 0 failed /
  6 ignored.
- Line coverage (`cargo llvm-cov`, clean profile, all tests, final): `crates/subtitles`
  98.68 %; `bins/inacircle-subtitles` 92.61 %; `crates/engine` 92.82 %;
  `bins/inacircle` 97.04 %.
- Fuzzing (`scripts/fuzz.sh`, targets `srt`, `vtt`, `ass`): the final 600 s
  campaign per target found no crash: 659,122 runs (srt), 184,031 (vtt),
  877,251 (ass). The round-trip oracle found eight product defects on the way
  before it was green. Six were found in the first smoke runs: whitespace-only
  lines, NUL, white-space-only cues in the readers, a tag-like `<`, `{\`
  override blocks, and timing-shaped text lines in the SRT writer. Two were
  found in the first 600 s campaign of the final tree: a WebVTT speaker name
  ending in dashes wrote an arrow, and the reader trimmed a speaker name before
  decoding with a Unicode trim. The committed seed corpora are 1936 (srt), 1507
  (vtt) and 1845 (ass) inputs.
- Mutation testing (`scripts/mutants.sh diff`, against `main`). The first run
  had 550 mutants: 501 caught, 4 timeouts, 35 unviable, 10 missed. Six of the
  ten were killed afterwards by new tests. Four are equivalent: the first-skip
  line is only ever moved forward by strictly increasing line numbers, and no
  test pins that ordering. The final run, on the final tree, had 632 mutants:
  592 caught, 3 timeouts (counted as caught), 35 unviable, 2 missed. One of the
  two was killed by a new boundary test (a WebVTT start of exactly
  99:59:59.999). The other is equivalent: the text of an internal over-limit
  error type is never shown, because the size message is built elsewhere and
  pinned end to end. Survivors of the earlier fix rounds were also resolved,
  either killed by tests or shown equivalent.

## Review

Two rounds of whole-branch review, each by a reviewer independent of the
implementation. The first found 2 critical findings (silent content loss in
the SRT reader and in the WebVTT reader), 4 important and 5 minor. The second
found no critical or important finding and 4 minor ones. All were fixed.

## Differential and conformance evidence

- **SRT against ffmpeg** (times and plain text): `tests/data/srt/ordinary.srt`,
  `irregular.srt`, `markup.srt`. `wild.srt` is excluded: hand-written edge
  cases, with each difference pinned by a named test. Known differences
  (`crates/subtitles/tests/srt.rs`, header):
  - a cue that ends before it starts is skipped (ffmpeg moves its end to the
    next cue's start);
  - a cue at 100 hours or more is skipped (ffmpeg writes a three-digit hour);
  - a `<` that is not a tag is kept as text (ffmpeg reads it as a tag);
  - a broken timing line after a blank line skips its block (ffmpeg merges the
    block into the previous cue);
  - a block of stray text is counted and not shown (ffmpeg appends it to the
    previous cue);
  - `<font>` is dropped (ffmpeg keeps it); the comparison strips tags on both
    sides, so this does not show in it.
- **ASS against ffmpeg** (times and plain text): `ordinary.ass`, `reordered.ass`,
  `reset.ass`, `v4.ass`. `wild.ass` is excluded, as above. Known differences
  (`crates/subtitles/tests/ass.rs`, header):
  - `\h` is a no-break space (ffmpeg keeps the two characters);
  - `\n` is a space unless `WrapStyle: 2` (ffmpeg breaks at every `\n`);
  - drawing text (`\p1` to `\p0`) is dropped (ffmpeg keeps it as text);
  - a reordered `Format:` line is read (ffmpeg drops every dialogue of such a file);
  - three-digit centiseconds are skipped and counted (ffmpeg drops the cue silently);
  - a zero-length cue is kept (ffmpeg drops it);
  - a non-dialogue line in `[Events]` is counted (ffmpeg ignores it silently);
  - an undefined style name takes `Default` (ffmpeg applies no style; styling
    is not compared);
  - section names in other letter cases and with a trailing space are read
    (ffmpeg reads none of a file whose `[Script Info]` is in lowercase).
- **WebVTT against web-platform-tests**: 42 files of
  `webvtt/parsing/file-parsing/tests` at commit `0fe2d70`, with the cue count
  and first cue of each checked against `expected.tsv`. Every file whose HTML
  states a count matches it, except `timings-negative`: 1 cue here against the
  test's 4, because a cue that ends before it starts is skipped, as in the SRT
  reader. `stylesheets` has no count in its test (it checks that no stylesheet
  applies). The reader's other choices where the standard leaves room
  (`crates/subtitles/src/vtt.rs`, header): the signature must be a word of its
  own; `NOTE` blocks are silent; `STYLE` and `REGION` blocks are reported as
  `Settings`; a `-->` in cue text starts the next cue; `<v>` names become
  `VoiceNames`; timing hours of any length, milliseconds exactly three digits.
- **TTML against the W3C imsc-tests and ttconv**: 321 files of the `imsc1`,
  `imsc1_1`, `imsc1_2` and `imsc1_3` suites (commit `b7f88fe`, in
  `crates/subtitles/tests/data/imsc-tests`). Three checks, with different weight:
  - a regression guard (`expected.tsv`): the outcome, cue count and first cue of
    each file as the reader gives them. It catches a change; it is not evidence
    that the reader is right;
  - hand-checks (`hand-checked.tsv`): 46 files read against their own
    description and the rule that applies. 37 agree. 9 differ, each counted and
    not silent: 3 by a rule the spec decides (a paragraph with no end; an oblique
    carried as italic), and 6 that are refused because a `set` changes their
    visibility over time (the reader does not play that timeline);
  - ttconv 1.2.3 (`tt convert`, CI only): 229 files are compared in SRT, and
    224 of them in WebVTT (five nested-division files are SRT only), on start
    and end in milliseconds and on the text with its italic, bold and
    underline. Only the white space at the two ends of a line is trimmed, on
    both sides, as every reader of the crate trims lines; colour and font
    markup, which the reader drops and counts, is the only markup removed. The
    files left out and why are at the top of `ttml_conformance.rs`.

  Static visibility is honoured: `display="none"`, `visibility="hidden"` and
  `opacity="0"` hide the text they reach (10.2.14, 10.2.49, 10.2.30), and each
  hidden paragraph or outermost hidden span is one `skipped` item. A paragraph
  that a `set` or `animate` changes in visibility is refused, one `skipped`
  item, rather than shown or hidden for part of its time.

## Verdict

Holds on this machine:

- The item budget bounds the readers. The densest 8 MiB shapes peak at about
  129 MiB or below for the kept inputs, and at about 216.5 MiB for the two
  refused ones, under the 1 GiB net, with the refused output never written.
- The cue-shaped inputs (the shapes a real subtitle has) run at 89 to 129 MiB,
  far from the net.
- `inacircle-subtitles` is 451,016 B (361,363 B of `.text`), under the family
  limit; `inacircle` is 1,199,368 B, under its 1.5 MB limit.
- The classifier adds about 3 µs per file, on this machine, in a
  micro-measurement, and about 19 µs to a drop of 50 arrow-less `.srt` files
  (base 172 µs; the first version added 93 µs). The new head search is a
  block scan over the first byte, see the classifier section. The
  arrow-less `.srt` is a case the wheel would see only for broken or
  non-subtitle files.

Open (not measured here, not recorded as passing):

- **Two-machine drag timings** (the user's manual series): a `.srt` and an
  `.ass` dragged into the circle, about 50 manual drags per machine, measuring
  the drop to the stage-2 wheel commit and the choice to the action's start,
  per [acceptance.md](acceptance.md). The classifier figure above is not that
  measurement.
- **Second-machine memory and size check**: the series here ran on one
  machine. The aarch64 size baseline was not refreshed.
- **Manual try on a television** of the converted files.
- **Two known deviations from the standards**: a WebVTT speaker name's internal
  runs of white space are not collapsed, as the standard collapses them; and
  WebVTT `STYLE` is recognised only as a block's first line, while the standard
  also recognises it on the second line, when that line has no arrow.

## Reproduce

Every test or benchmark run goes through the memory prefix (the test memory
rule of this repository). Builds run without it.

```sh
MEM="systemd-run --user --scope -q --slice=inacircle-tests.slice -p MemoryMax=60G -p MemorySwapMax=0 --"

# Shipped binaries and the size check
cargo build --release -p inacircle-subtitles
scripts/check-sizes.sh

# Dense memory shapes, test build (prints one peak-RSS line per shape), 5 runs
for i in 1 2 3 4 5; do
  $MEM cargo test -p inacircle-subtitles --test cli dense_8_mib_input -- --nocapture
done

# Release binary: the same five shapes from a parent that sets RLIMIT_DATA
# (1 GiB) and RLIMIT_FSIZE (256 MiB) on each child, runs
#   target/release/inacircle-subtitles run <action> --work <dir> -- <input>
# and reads ru_maxrss from wait4. The parent is a throw-away script that
# generates the inputs as the dense test does (bins/inacircle-subtitles/tests/cli.rs);
# it is not committed. The ASS cue inputs are the dense test's head (`[Script Info]`,
# `ScriptType: v4.00+`, `[Events]`, the `Format:` line of the test) followed by
# `Dialogue: 0,0:00:00.00,0:00:00.01,,,0,0,0,,x` lines (45 B with the newline; the
# dense row), or by `Dialogue:0,0:00:00.00,0:00:00.01,,,,,,,x` lines (41 B; the
# 41-byte row), padded with newlines to 8 MiB. Run it under the prefix:
#   $MEM python3 -I <parent>.py <binary> 5 <input dir> <work dir>

# Refused-shape check (no peak figure): one run per format
$MEM cargo test -p inacircle-subtitles --test cli too_complex_fails_and_leaves_nothing

# Conformance: SRT and ASS against ffmpeg, WebVTT against the WPT files
INACIRCLE_REQUIRE_SUBTITLE_TOOLS=1 $MEM cargo test -p subtitles --test srt --test ass --test vtt

# TTML against the imsc-tests guard, the hand-checks and ttconv (`tt` on PATH)
INACIRCLE_REQUIRE_TTCONV=1 $MEM cargo test -p subtitles --test ttml_conformance -- --nocapture

# Full suite, debug and release, with the required-tool switches
INACIRCLE_REQUIRE_LANDLOCK=1 INACIRCLE_REQUIRE_PDF_TOOLS=1 INACIRCLE_REQUIRE_SUBTITLE_TOOLS=1 \
  $MEM cargo test --workspace
INACIRCLE_REQUIRE_LANDLOCK=1 INACIRCLE_REQUIRE_PDF_TOOLS=1 INACIRCLE_REQUIRE_SUBTITLE_TOOLS=1 \
  $MEM cargo test --release --workspace

# Classifier micro-measurement: the throw-away example in a copy of each tree
#   git archive bdb66403 | tar -x -C <base>   # then add crates/engine/examples/classify_bench.rs
#   git archive <classify commit> | tar -x -C <head>  # the same file
#   (cd <tree> && cargo build --release -p engine --example classify_bench)
# Inputs: 300 KiB ASCII files, 50 per set: .srt with the arrow line first, .srt
# with no `-->`, and .ass with and without a `[Script Info]` header before the
# same plain text; the example takes <drops> <files...> and prints
# the median and p95 of the per-drop time (one Reread::per_drop() and one
# classify_with per file). Run it under the prefix:
#   $MEM <tree>/target/release/examples/classify_bench 2000 <files...>
```
