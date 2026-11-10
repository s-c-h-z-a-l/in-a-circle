# Subtitle actions

What the subtitle family (SRT, WebVTT, ASS/SSA and TTML conversions) costs in
memory, binary size and classifier time, what its limits protect, and what is
still open. Gates and rules: [acceptance.md](acceptance.md); limits:
[`resource-limits.md`](../resource-limits.md#subtitles); the TTML rows of the
reader's accounting are under [architecture.md](../architecture.md#ttml-accounting).

**Status: measured here, on one machine.** The memory series, the size check
and the classifier micro-measurement below were run for this page. The test,
coverage, fuzzing, mutation and differential figures are the recorded results
of the closing runs on this branch (the workspace suite, the coverage and the
mutation runs were not repeated for this page). The two-machine drag timings,
the second machine and the TV check are **open** (see [Verdict](#verdict)).

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

Six actions, one format in and one out: `subtitles.srt-to-vtt`,
`subtitles.vtt-to-srt`, `subtitles.ass-to-srt`, `subtitles.ass-to-vtt`,
`subtitles.ttml-to-srt` and `subtitles.ttml-to-vtt`. The classifier types a
file by its head: a `.srt` with a `-->` timing (`application/x-subrip`), a
`.vtt` with the `WEBVTT` signature (`text/vtt`), a `.ass` or `.ssa` with a
`[Script Info]` section (`text/x-ssa`), and a `.ttml` or `.dfxp` (or a `.xml`
whose root is `tt` in a TTML namespace) as `application/ttml+xml`. The readers
are in `crates/subtitles` (standard library, and the `xml` crate for TTML); the
action binary is `inacircle-subtitles`.

Every conversion is lossy where the target cannot carry a feature. The notice
names the loss, one line per kind: colours, fonts and sizes; exact positions;
karaoke effects; drawings; style and region blocks; speaker names written as
`Name:` prefixes; characters that look like markup, written as look-alikes;
and unsupported elements ignored (TTML elements the reader does not convert,
and images).

| Limit | Value | Past it |
|---|---|---|
| input | 8 MiB (`INPUT_BYTES`) | refused before parsing |
| output | 32 MiB (`OUTPUT_BYTES`) | refused while writing; the file is removed |
| time | 99:59:59.999 (`MAX_MS`) | the cue is skipped with a warning |
| items | 2,000,000 (`ITEM_BUDGET`) | refused whole (`too many lines and styled runs (more than 2,000,000)`); nothing is truncated. TTML also charges its elements, pieces, slice copies, style and region entries, and the animated-visibility scopes (see [TTML accounting](../architecture.md#ttml-accounting)) |
| TTML element depth | 256 (`xml::MAX_DEPTH`) | refused whole as `not well-formed TTML (line N): elements nested too deeply` |
| TTML style chain | 32 entries (`MAX_CHAIN`) | a longer chain or a loop is not followed; the element gets `Styling` |
| warnings | eight loss kinds and one for skipped items | at most nine lines per input |

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

### Dense TTML shapes

The dense TTML test (`dense_ttml_shapes_stay_within_the_data_limit`) runs
fourteen inputs, each the densest its shape allows in the 8 MiB limit, through
the release binary (`subtitles.ttml-to-srt`), five runs each, under
`RLIMIT_DATA` 1 GiB and `RLIMIT_FSIZE` 256 MiB. The design's counts that do not
fit are replaced by the densest that do: 599,176 spans (not 4 M: 56 MB), 1,677,693
breaks (not 4 M: 20 MB), and 391,791 named styles (a named style costs 18 B plus
its name; the unnamed `<style/>` reaches 784,960). The units are the reader's
charges; bytes per unit is (median peak minus the baseline's median) / units,
the baseline being one cue in the same padded input (10.70 MiB). A refused
shape's units are the most the budget keeps, found by bisection on the release
binary.

Measured with a minimal C parent (about 1 MB resident) that forks, sets the
limits and execs the binary, reaping it with `wait4`: a fork reports at least
its parent's resident set, and a parent that held the inputs in memory made
every child report about 130 MiB, so the inputs are written to disk and the
parent holds none. Run alone, the test prints peaks up to 3 MB higher on the
small shapes and within 0.2 % on the large ones. In the full CLI suite its
process is shared with the parallel tests, so the small shapes can read higher
(67 MiB in one debug run); the table is from the isolated runs.

| shape (8 MiB input) | units | min / median / max (MiB) | outcome | B per unit |
|---|---|---|---|---|
| baseline: one cue, padded | 1 | 10.68 / 10.70 / 10.71 | kept | |
| run-dense: one paragraph of 599,176 `<span>x</span>` | 599,176 spans | 100.73 / 100.98 / 101.10 | kept | 158 per span |
| cue-dense: 299,589 `<p begin="0s" end="1s">x</p>` | 299,589 cues | 124.81 / 124.97 / 125.11 | kept | 400 per cue |
| depth-dense: 2,738 blocks, each 254 elements, 256 open at the deepest | 695,452 elements | 11.71 / 11.71 / 11.91 | kept | 1.5 per element |
| table-dense: 391,791 named `<style xml:id>` | 391,791 styles | 82.09 / 82.30 / 82.34 | kept | 192 per style |
| table-dense: 784,960 unnamed `<style/>` | 784,960 styles | 83.24 / 83.36 / 83.39 | kept | 97 per style |
| region-dense: 374,757 named `<region>` | 374,757 regions | 79.08 / 79.18 / 79.21 | kept | 192 per region |
| reference-dense, in a paragraph: 1,723,886 distinct undefined ids in one `style` | 1,723,886 ids | 10.64 / 10.70 / 10.75 | kept | 0 (not charged) |
| reference-dense, in the head: 1,723,876 distinct undefined ids on a `style` element | 999,994 kept at most | 200.05 / 200.10 / 200.16 | refused, `TooComplex` (exit 1) | 199 per id (2 items each) |
| break-dense: 1,677,693 `<br/>` in one paragraph | 666,664 kept at most | 63.96 / 64.09 / 64.25 | refused, `TooComplex` (exit 1) | 84 per `br` (about 3 items each) |
| one 1 MiB attribute value: a style id, referenced by a paragraph | 1 value | 11.61 / 11.71 / 11.79 | kept | 1.0 per byte of value |
| chain-dense: 149,920 styles, each referring to the one before | 149,920 styles | 44.80 / 44.93 / 44.98 | kept | 239 per style |
| overlap-dense, nested: 177,074 spans, each inside the one before | 1,411 spans kept at most | 36.42 / 36.51 / 36.63 | refused, `TooComplex` (exit 1) | about 14 per copy (N² copies, about 2,000,000) |
| overlap-dense, staggered: 179,389 spans, 10 ms apart, 15 ms long | 179,389 spans | 170.56 / 170.73 / 170.82 | kept | 935 per span |

Reading: the highest kept peak is 170.8 MiB (the staggered overlaps, kept at
the most spans the input holds), the highest refused one 200.2 MiB
(the head reference list at the budget). Both are under the 400 MiB ceiling and
a fifth of the 1 GiB net. The nested overlap is bounded by the copy charge:
the budget stops it at 1,411 spans (about N² = 2,000,000 copies). A paragraph's
`style` references are not charged, and cost one lookup and one merge each,
bounded by the input. The 1 MiB attribute value costs its own size once.

## Binary size

`scripts/check-sizes.sh`, release builds of the shipped binaries at commit
`379f4bdb5` (clean tree; the refresh commit changes only documentation and
`scripts/sizes.baseline`). File bytes against the `inacircle` limit of 1 572 864 B
(1.5 MB); `.text` bytes against the 1 MiB family limit (`CLAUDE.md`). Growth
is against the previous line in `scripts/sizes.baseline` (the line before this
refresh, the TTML line).

| Binary | file B, baseline → now (growth) | `.text` B, baseline → now (growth) |
|---|---|---|
| `inacircle` | 1,201,128 → 1,200,856 (−272) | 925,992 → 925,144 (−848) |
| `inacircle-text` | 1,268,024 → 1,268,024 (0) | 901,043 → 901,043 (0) |
| `inacircle-pages` | 514,648 → 514,648 (0) | 412,179 → 412,179 (0) |
| `inacircle-subtitles` | 794,368 → 804,792 (+10,424) | 497,717 → 507,381 (+9,664) |

Every growth is under the 100 KB explanation threshold, and every `.text` stays
under the 1 MiB family limit. `inacircle-subtitles` grew since the TTML line by
the TTML reader's fixes (hidden text, refused animated visibility, the values of
`initial`, the reasons for no subtitles). `inacircle` shrank a little, net, from
the classifier's TTML root scan and the menu's TTML rows. The `inacircle-text`
and `inacircle-pages` binaries are unchanged. The aarch64 baseline was not
refreshed.

The HTML5 entity table was then made relocation-free (see the open items).
Against the TTML line: `inacircle-subtitles` 804,792 → 663,768 B (−141,024: the
table −151,152, the subtitle code's changes since the TTML line +10,128), `.text`
507,381 → 516,773 (+9,392); `inacircle-text` 1,268,024 → 1,116,872 (−151,152),
`.text` 901,043 → 900,819 (−224). `inacircle` and `inacircle-pages` are
unchanged. Every growth is under the 100 KB threshold.

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

### TTML and `.xml`

The TTML rules add a head search for `.xml`: the root start tag is read after
the prolog (`<?…?>`, comments, a `<!DOCTYPE …>`), with `find` for the ends of
those declarations. A `.ttml` or `.dfxp` costs nothing new (its head is checked
for a leading `<`). Base is `c7125a9a`, the head before the TTML classifier
work, a `git archive` with its own target directory. The harness is the same as above (`classify_bench`,
2,000 drops, two alternating rounds with the order swapped in round 2).

Inputs: `nottml`, a `.xml` of 3,992 B (a 3,900 B XML comment, then
`<catalog xmlns="urn:example:catalog">`, so the root is inside the head but
is not `tt`); `ttml-xml`, a 403 B TTML document as `.xml`; `ttml`, the same as
`.ttml`; `srt`, 94 B; `txt`, 51 B. "1 file" is one file per drop; "50 files" is
50 copies per drop. Times in µs, round 1 / round 2. The delta is the mean of
the two medians against the mean of base.

**History.** The first TTML head searched the head byte by byte, one slice
compare per position, with `find()`: `.xml` without TTML, 50 files, +91 %
(94 → 180 µs). The cost is the comment's `-->` search across about 3.9 KB, the
same window scan that the subtitle head search had replaced in `in_head`. The fix is the shared
block search (`first_match`, used by `find` and `in_head`): 128-byte blocks,
an OR-fold of first-byte compares, and a full compare only in a block that
holds the first byte. Three variants were measured in the same harness:

| Variant | `.xml` without TTML, 50 files, delta (median) |
|---|---|
| window scan (first TTML head) | +91.3 % (one run, two rounds) |
| block scan in `find()` only, scratch copy | +9.5 % (two rounds, same session as base) |
| block scan, comparison by a closure on the bytes after the first | +13.1 % (four rounds, same session) |
| block scan, comparison of the whole window (committed) | +8.2 % (four rounds, same session); +11.8 % in the final full run |

The closure over the rest of the window was consistently slower than the
whole-window compare, so the whole-window form is the one committed. The
behaviour is the window scan's: `find_matches_the_naive_scan` checks the
positions, not only whether a needle occurs, on every length up to 4 KiB, with
needles at the block edges and random bytes, against the old scan as the oracle.

**Final numbers** (the committed tree; base `c7125a9a`):

| Case | Tree | median | p95 | median delta |
|---|---|---|---|---|
| `.xml` without TTML, 1 file | base | 1.87 / 1.87 | 1.93 / 1.92 | |
| `.xml` without TTML, 1 file | new | 2.07 / 2.09 | 2.12 / 2.16 | +11.2 % |
| `.xml` without TTML, 50 files | base | 94.27 / 93.91 | 95.87 / 96.04 | |
| `.xml` without TTML, 50 files | new | 104.33 / 106.13 | 106.59 / 107.69 | +11.8 % |
| `.xml` TTML, 1 file | base | 1.58 / 1.61 | 1.64 / 1.68 | |
| `.xml` TTML, 1 file | new | 1.65 / 1.64 | 2.12 / 1.76 | +3.1 % |
| `.xml` TTML, 50 files | base | 79.75 / 78.93 | 82.01 / 81.20 | |
| `.xml` TTML, 50 files | new | 82.76 / 81.59 | 84.55 / 83.20 | +3.6 % |
| `.ttml`, 1 file | base | 1.58 / 1.58 | 1.67 / 1.65 | |
| `.ttml`, 1 file | new | 1.56 / 1.57 | 1.61 / 1.65 | -0.9 % |
| `.ttml`, 50 files | base | 78.56 / 78.91 | 80.45 / 80.71 | |
| `.ttml`, 50 files | new | 76.84 / 76.95 | 78.28 / 79.01 | -2.3 % |
| `.srt`, 1 file | base | 1.58 / 1.54 | 1.98 / 1.58 | |
| `.srt`, 1 file | new | 1.54 / 1.54 | 1.99 / 1.92 | -1.3 % |
| `.srt`, 50 files | base | 77.09 / 76.65 | 78.28 / 80.34 | |
| `.srt`, 50 files | new | 76.75 / 77.28 | 78.78 / 78.48 | +0.2 % |
| `.txt`, 1 file | base | 1.50 / 1.51 | 1.91 / 1.96 | |
| `.txt`, 1 file | new | 1.50 / 1.50 | 1.56 / 1.87 | -0.3 % |
| `.txt`, 50 files | base | 74.00 / 74.05 | 75.45 / 76.60 | |
| `.txt`, 50 files | new | 74.11 / 73.72 | 119.33 / 75.83 | -0.1 % |

The `.txt` 50-file p95 of 119.33 µs is one outlier of a single round; the
medians of that row agree. The `.ttml`, `.srt` and `.txt` paths are within the
noise of the two trees, and the `.xml` TTML drop costs about 0.06 µs per file.

**Verdict.** The shared block search takes the TTML-less `.xml` 50-file drop
from +91 % to between +8 % and +12 % (p50) against the base, on this machine,
in runs of the committed code. Stated plainly: that is above the 10 % goal in
part of the runs and under the 15 % level that blocks a merge, so it sits at the
investigation line. It is not robustly under the goal, and it is left open.
What remains is the scan of the 3.9 KB comment: about 0.25 µs per file, which
is the block fold's speed on this machine (roughly 15 bytes per ns). A cheaper
scan would need a wider compare (AVX2 with a runtime check, which is unsafe
code), and it was not done. The input is a worst case for this path: a long
comment in a `.xml` that is not TTML, read to its end before the root is found.
A TTML `.xml` drop costs +3.6 % (the root is found early), and the `.ttml`,
`.srt` and `.txt` drops are within the noise of the two trees.

These are CPU micro-measurements on this machine (Ryzen 7 5700X, Linux
7.3.1-1-cachyos, rustc 1.98.1, release examples). They are not the acceptance
measurement of the wheel's budget, which is still open: two machines, about 50
manual drags each.

## Test and verification evidence

Recorded results of the closing runs on this branch (not re-run for this page):

- Full workspace suite, with `INACIRCLE_REQUIRE_LANDLOCK=1`,
  `INACIRCLE_REQUIRE_PDF_TOOLS=1` and `INACIRCLE_REQUIRE_SUBTITLE_TOOLS=1`:
  debug 2475 passed / 0 failed / 9 ignored; release 2475 passed / 0 failed /
  9 ignored. The ignored tests are measurement tests, run by hand.
- Line coverage (`cargo llvm-cov`, clean profile, all tests): `crates/subtitles`
  98.51 %; `bins/inacircle-subtitles` 93.87 %; `crates/engine` 92.90 %;
  `bins/inacircle` 97.05 %; `crates/xml` 99.40 %. The TTML modules:
  `ttml/mod.rs` 98.15 %, `ttml/slice.rs` 100 %, `ttml/style.rs` 97.36 %,
  `ttml/time.rs` 99.35 %.
- Fuzzing (`scripts/fuzz.sh`, targets `srt`, `vtt`, `ass`, `ttml`): the final
  600 s campaign per target found no crash: 331,856 runs (ttml), 773,669 (srt),
  189,920 (vtt), 831,116 (ass). The round-trip oracle found eight product
  defects on the way before it was green. Six were found in the first smoke
  runs: whitespace-only lines, NUL, white-space-only cues in the readers, a
  tag-like `<`, `{\` override blocks, and timing-shaped text lines in the SRT
  writer. Two were found in the first 600 s campaign of the final tree: a
  WebVTT speaker name ending in dashes wrote an arrow, and the reader trimmed a
  speaker name before decoding with a Unicode trim. The committed seed corpora
  are 1936 (srt), 1507 (vtt), 1845 (ass) and 338 (ttml) inputs.
- The TTML target carries a model of its own. It was written right after the
  reader core, and its first plain run did not find a deliberately broken
  end-time computation in 138,000 runs: the oracle had no time model, so the
  shared checks (no panic, the round trip) could not see a wrong end. A small
  independent model of
  one paragraph's timing (TTML2 12.2.2: the interval is the begin to the earlier
  of the end and begin plus dur) was added to the target, and it found the
  break at once. That is why the target carries the model
  (`check_paragraph_timing`, in `fuzz/fuzz_targets/ttml.rs`).
- Mutation testing (`scripts/mutants.sh diff`, against `main`). The first run
  had 550 mutants: 501 caught, 4 timeouts, 35 unviable, 10 missed. Six of the
  ten were killed afterwards by new tests. Four are equivalent: the first-skip
  line is only ever moved forward by strictly increasing line numbers, and no
  test pins that ordering. The run on the final tree of the subtitle work had
  632 mutants: 592 caught, 3 timeouts (counted as caught), 35 unviable, 2
  missed. One of the two was killed by a new boundary test (a WebVTT start of
  exactly 99:59:59.999). The other is equivalent: the text of an internal
  over-limit error type is never shown, because the size message is built
  elsewhere and pinned end to end. Survivors of the earlier fix rounds were
  also resolved, either killed by tests or shown equivalent.
- Mutation testing of the TTML work (`scripts/mutants.sh diff`, against the
  head before the TTML work, whole workspace tests): 660 mutants: 593 caught,
  7 timeouts, 35 unviable, 25 missed. All 25 survivors were killed afterwards by
  new tests: three classifier ones by a classify test, the rest by TTML tests,
  one of them a pure `Ratio` lowest-terms test. The 7 timeouts are legitimate
  catches: each mutation makes a loop stop advancing, so the hang is the
  detection (the `time.rs` gcd one now also has a direct assertion test). The
  run took six hours, because every mutant ran the whole workspace suite.

## Review

Two rounds of whole-branch review, each by a reviewer independent of the
implementation. The first found 2 critical findings (silent content loss in
the SRT reader and in the WebVTT reader), 4 important and 5 minor. The second
found no critical or important finding and 4 minor ones. All were fixed.

The whole-branch review of the TTML work found these, all fixed:

- white space between timed spans ran together into one word (critical);
- a line break decided a paragraph's end, so a paragraph timed only by its
  spans was skipped with no end time;
- text in pruned elements, and untimed documents, failed without a reason;
- attributes in foreign namespaces, the `animate` attribute, and conditional
  styles and regions were not handled;
- a stray image hid the reason that text was skipped;
- a profile declaration gave a false warning;
- a preserved line was not trimmed across its runs.

The independent checks were extended with a text-conservation model (a property
test and a fuzz target): the first checks had left span-timed and untimed
paragraphs unchecked.

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
  - ttconv 1.2.3 (`tt convert`, CI only; a pinned wheel, verified by its
    SHA-256 in the workflow, with no dependencies of its own): 229 files are
    compared in SRT, and 224 of them in WebVTT (five nested-division files are
    SRT only), on start and end in milliseconds and on the text with its
    italic, bold and underline. Only the white space at the two ends of a line
    is trimmed, on both sides, as every reader of the crate trims lines; colour
    and font markup, which the reader drops and counts, is the only markup
    removed. The files left out and why are at the top of
    `ttml_conformance.rs`.
    - The first CI run that really executed the comparison reported 240 files
      compared and 35 differences. Each was reviewed against the standard and
      against ttconv's source. One was a real product gap: the values of the
      `initial` element were not applied (fixed). The others are ttconv
      behaviours, comparison rules, or the reader's own stated choices (the
      refusals and skips in the accounting table). Files are left out of the
      comparison by structure, for the reason the test header gives: cues that
      overlap (22), timed spans (12), a style animation (10), and a paragraph
      with no end time (2); the 22 files the reader refuses are not compared
      either. The 24 files of the divergence list are compared with a stated
      reason: visibility (three files: two have a paragraph refused by a
      `set`, and one has `visibility="hidden"` text that the reader hides and
      ttconv keeps); oblique and underline with other decorations (three
      files); ruby text, which ttconv drops (sixteen files); times of 100 hours
      or more, which the reader skips and counts (one file); white space before
      a line end in preserve mode (one file). Five nested-division files are
      left out of the WebVTT comparison alone: ttconv's WebVTT writer visits
      only the direct children of a `div`.
    - The second run passed on both runners, with the test required to run
      (`INACIRCLE_REQUIRE_TTCONV=1`), not skipped: 229 files compared in SRT,
      224 in WebVTT.

  Static visibility is honoured: `display="none"`, `visibility="hidden"` and
  `opacity="0"` hide the text they reach (10.2.14, 10.2.49, 10.2.30), and each
  hidden paragraph or outermost hidden span is one `skipped` item. A paragraph
  that a `set` or `animate` changes in visibility is refused, one `skipped`
  item, rather than shown or hidden for part of its time.

## What the TTML reader does that you may not expect

Each line is checked against the reader's accounting table
([TTML accounting](../architecture.md#ttml-accounting)) and the tests of
`crates/subtitles/tests/ttml.rs`.

- A SMPTE time code (`ttp:timeBase="smpte"`) is read under either
  `ttp:markerMode`, as continuous: an approximation for discontinuous markers,
  which carry no media time of their own.
- A `clock` time base, a `wallclock(...)` time and a drop-frame
  `ttp:dropMode` are refused: `unsupported TTML`, the whole file.
- A paragraph with no end time is skipped and counted (`no end time`). When no
  paragraph is usable, the whole file fails with the count and the first
  reason.
- Text the document hides (`tts:display="none"`, `tts:visibility="hidden"`,
  `tts:opacity="0"`) is not shown. It is counted (`hidden by the document`).
- A paragraph whose visibility a `set` or `animate` changes is refused and
  counted (`visibility changes over time`). An opacity fade is one of these,
  so a faded paragraph loses its text, and the count says so.
- A `condition` on `body`, `div`, `p`, `span`, `br` or `ruby` is not evaluated.
  The element is skipped and counted (`conditional element`).
- Text beside an image is converted; the image is counted. A file with only
  images fails with `no text subtitles found (image-based)`.
- Styles: a paragraph's own declarations and those of its content ancestors
  come before its region's (10.4.2). The values of `initial` are the lowest
  layer, and each property takes the last `initial` that names it.
- `fontWeight` is `normal` or `bold` only; any other value is counted as
  `Styling`. `fontStyle="oblique"` is carried as italic.
- A duplicate `xml:id` among styles or among regions: the first definition
  stands, and a reference to the name counts the dropped one's declarations.
- A paragraph with no region (a document with no `layout`, or none named) is
  never `top`.
- A style reference chain past 32 entries, or a loop, is not followed; the
  element gets `Styling`.
- A paragraph with no visible text has nothing to lose, so no count is made
  for it whatever else it holds.

## Verdict

Holds on this machine:

- The item budget bounds the readers. The densest 8 MiB SRT, WebVTT and ASS
  shapes peak at about 129 MiB or below for the kept inputs, and at about
  216.5 MiB for the refused ones. The dense TTML shapes peak at 170.8 MiB at
  most when kept (the staggered overlaps) and at 200.2 MiB when refused (the
  head reference list). All are under the 1 GiB net, and the refused output is
  never written.
- The cue-shaped inputs (the shapes a real subtitle has) run at 89 to 129 MiB,
  far from the net. A dense TTML cue shape (299,589 cues) peaks at 125.0 MiB.
- `inacircle-subtitles` is 663,768 B (516,773 B of `.text`), under the family
  limit; `inacircle` is 1,200,856 B, under its 1.5 MB limit.
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
  per [acceptance.md](acceptance.md). The TTML series is the same measurement
  for an `.xml` TTML and a `.ttml`, about 50 drags each. The classifier figure
  above is not that measurement.
- **Second-machine memory and size check**: the series here ran on one
  machine, including the dense TTML shapes. The aarch64 size baseline was not
  refreshed.
- **Manual try on a television** of the converted files, TTML ones included.
- **The classifier cost of a TTML-less `.xml`**: +8 % to +12 % p50 on a 50-file
  drop against the head before the TTML work (see the classifier section). That
  is at the 10 % investigation line and under the 15 % blocking line. The
  residual is the scan of a long head comment, about 0.25 µs per file. It is
  left open, not fixed.
- **The HTML5 entity table of the `xml` crate**, which TTML reads with
  `html_entities` off, was 77 KB of relocated data and 114,576 B of `.rela.dyn`
  in the subtitle binary. It is now relocation-free (one string of names and
  characters and a `(u32, u16, u16)` offset table): `.rela.dyn` 12,672 B,
  `.data.rel.ro` 9,616 B. What remains is 37 KB of read-only `.rodata` (17,000 B
  of offsets, 20,220 B of names and characters), never relocated or written at
  startup. A feature switch in the `xml` crate would save that file size and no
  startup time: not made.
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

# Dense TTML shapes: the test runs the fourteen inputs (five runs each) and
# prints min / median / max per shape. The table above was measured with the
# release build (`--release`) from a minimal parent (a C program that forks,
# sets the limits and execs the binary, reading `ru_maxrss` by `wait4`). That
# parent is a throw-away and not committed.
$MEM cargo test --release -p inacircle-subtitles --test cli dense_ttml -- --nocapture

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

# Fuzzing: the four targets, 600 s each, in parallel (the script runs the
# nightly from its pin; the seed corpora are in fuzz/corpus/<target>)
$MEM scripts/fuzz.sh close srt vtt ass ttml

# Mutation testing of the change only, against the commit it started from
# (`main` by default); the whole workspace suite runs for each mutant
$MEM scripts/mutants.sh diff <base>

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
