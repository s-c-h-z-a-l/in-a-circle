use std::fs;
use std::io::Read;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

const MANIFEST: &str = "inacircle-manifest 0\n\
action\tsubtitles.srt-to-vtt\tfamily=subtitles\tin=application/x-subrip\tout=text/vtt\text=vtt\tlabel=VTT\tgroup=subtitles\tcost=10\tlossy=yes\tbatch=yes\n\
action\tsubtitles.vtt-to-srt\tfamily=subtitles\tin=text/vtt\tout=application/x-subrip\text=srt\tlabel=SRT\tgroup=subtitles\tcost=10\tlossy=yes\tbatch=yes\n\
action\tsubtitles.ass-to-srt\tfamily=subtitles\tin=text/x-ssa\tout=application/x-subrip\text=srt\tlabel=SRT\tgroup=subtitles\tcost=10\tlossy=yes\tbatch=yes\n\
action\tsubtitles.ass-to-vtt\tfamily=subtitles\tin=text/x-ssa\tout=text/vtt\text=vtt\tlabel=VTT\tgroup=subtitles\tcost=10\tlossy=yes\tbatch=yes\n\
action\tsubtitles.ttml-to-srt\tfamily=subtitles\tin=application/ttml+xml\tout=application/x-subrip\text=srt\tlabel=SRT\tgroup=subtitles\tcost=10\tlossy=yes\tbatch=yes\n\
action\tsubtitles.ttml-to-vtt\tfamily=subtitles\tin=application/ttml+xml\tout=text/vtt\text=vtt\tlabel=VTT\tgroup=subtitles\tcost=10\tlossy=yes\tbatch=yes\n";

/// The input limit and the output ceiling, as the action states them.
const MIB: usize = 1 << 20;
const INPUT_BYTES: usize = 8 * MIB;
const OUTPUT_BYTES: usize = 32 * MIB;

/// The memory net the sandbox sets on an action: `RLIMIT_DATA` at its floor,
/// 1 GiB (`DATA_CAP_MIN` in the engine). A larger machine gets more, so this
/// is the strictest case.
const DATA_LIMIT: u64 = 1 << 30;
/// The engine's `RLIMIT_FSIZE` floor, 256 MiB.
const FSIZE_LIMIT: u64 = 256 << 20;

/// The `fail` text of an input over the reader's item budget.
const TOO_COMPLEX: &str = "too many lines and styled runs (more than 2,000,000)";

/// Peak RSS a run may reach. The item budget bounds the readers at about
/// 216 MiB (`ITEM_BUDGET`, 216.5 MiB measured), plus the 8 MiB input and the allocator's slack;
/// 400 MiB is that with room to spare and far under the 1 GiB net. Past it the
/// budget no longer bounds memory, so the run fails.
const PEAK_CEILING: u64 = 400 << 20;

const BOM: &[u8] = b"\xEF\xBB\xBF";

/// A unique temporary directory, removed on drop.
struct Tmp(PathBuf);

impl std::ops::Deref for Tmp {
    type Target = Path;
    fn deref(&self) -> &Path {
        &self.0
    }
}

impl Drop for Tmp {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn tmp(name: &str) -> Tmp {
    let d = std::env::temp_dir().join(format!(
        "inacircle-subtitles-test-{}-{name}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    Tmp(d)
}

fn bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_inacircle-subtitles"))
}

fn run(id: &str, work: &Path, inputs: &[&Path]) -> Output {
    bin()
        .args(["run", id, "--work"])
        .arg(work)
        .arg("--")
        .args(inputs)
        .output()
        .unwrap()
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn lines<'a>(out: &'a str, kind: &str) -> Vec<Vec<&'a str>> {
    out.lines()
        .map(|l| l.split('\t').collect::<Vec<_>>())
        .filter(|f| f[0] == kind)
        .collect()
}

/// The `warn` messages of an input, in the order they were printed.
fn warns(out: &str) -> Vec<String> {
    lines(out, "warn").iter().map(|f| f[2].to_owned()).collect()
}

/// Every file under `dir`, with its bytes, sorted.
fn files(dir: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    let mut v = Vec::new();
    if let Ok(rd) = fs::read_dir(dir) {
        for e in rd {
            let p = e.unwrap().path();
            if p.is_dir() {
                v.extend(files(&p));
            } else {
                let c = fs::read(&p).unwrap();
                v.push((p, c));
            }
        }
    }
    v.sort();
    v
}

/// The one output file of a run, read whole.
fn produced(work: &Path, out: &str) -> Vec<u8> {
    let done = lines(out, "done");
    assert_eq!(done.len(), 1, "{out}");
    fs::read(work.join(done[0][2])).unwrap()
}

/// Writes `name` into `dir` and returns its path.
fn put(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
    let p = dir.join(name);
    fs::write(&p, bytes).unwrap();
    p
}

#[test]
fn manifest_lists_the_six_actions() {
    let o = bin().arg("--manifest").output().unwrap();
    assert_eq!(o.status.code(), Some(0));
    assert_eq!(stdout(&o), MANIFEST);
}

#[test]
fn srt_to_vtt_converts_and_keeps_formatting() {
    let root = tmp("srt-vtt");
    let work = root.join("work");
    fs::create_dir_all(&work).unwrap();
    let a = put(
        &root,
        "a.srt",
        b"1\n00:00:01,000 --> 00:00:02,500\n<i>Hello</i> <b>bold</b> <u>under</u>\n\n\
2\n00:00:03,000 --> 00:00:04,000\n{\\an8}Top line\n",
    );
    let o = run("subtitles.srt-to-vtt", &work, &[&a]);
    assert_eq!(o.status.code(), Some(0), "{}", stdout(&o));
    let out = stdout(&o);
    assert_eq!(
        String::from_utf8(produced(&work, &out)).unwrap(),
        "WEBVTT\n\n\
00:00:01.000 --> 00:00:02.500\n\
<i>Hello</i> <b>bold</b> <u>under</u>\n\n\
00:00:03.000 --> 00:00:04.000 line:0\n\
Top line\n\n"
    );
    assert!(warns(&out).is_empty(), "{out}");
}

#[test]
fn vtt_to_srt_voice_becomes_a_prefix_and_warns() {
    let root = tmp("vtt-srt");
    let work = root.join("work");
    fs::create_dir_all(&work).unwrap();
    let a = put(
        &root,
        "a.vtt",
        b"WEBVTT\n\n00:00:01.000 --> 00:00:02.000\n<v Ann>Hello there</v>\n",
    );
    let o = run("subtitles.vtt-to-srt", &work, &[&a]);
    assert_eq!(o.status.code(), Some(0), "{}", stdout(&o));
    let out = stdout(&o);
    let mut want = BOM.to_vec();
    want.extend_from_slice(b"1\r\n00:00:01,000 --> 00:00:02,000\r\nAnn: Hello there\r\n\r\n");
    assert_eq!(produced(&work, &out), want);
    assert_eq!(
        warns(&out),
        ["speaker names written as 'Name:' prefixes (1 cues)"],
        "{out}"
    );
}

#[test]
fn ass_to_srt_reports_karaoke_and_drawings() {
    let root = tmp("ass-srt");
    let work = root.join("work");
    fs::create_dir_all(&work).unwrap();
    let a = put(
        &root,
        "a.ass",
        b"[Script Info]\nScriptType: v4.00+\n\n\
[V4+ Styles]\nFormat: Name, Fontname, Fontsize, PrimaryColour, SecondaryColour, OutlineColour, BackColour, Bold, Italic, Underline, StrikeOut, ScaleX, ScaleY, Spacing, Angle, BorderStyle, Outline, Shadow, Alignment, MarginL, MarginR, MarginV, Encoding\n\
Style: Default,Arial,20,&H00FFFFFF,&H000000FF,&H00000000,&H00000000,0,0,0,0,100,100,0,0,1,2,0,2,10,10,10,1\n\n\
[Events]\nFormat: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\n\
Dialogue: 0,0:00:01.00,0:00:02.00,Default,,0,0,0,,{\\k50}Ka{\\k50}ra{\\k20}oke\n\
Dialogue: 0,0:00:03.00,0:00:04.00,Default,,0,0,0,,{\\p1}m 0 0 l 100 0 100 100{\\p0}Shape text\n",
    );
    let o = run("subtitles.ass-to-srt", &work, &[&a]);
    assert_eq!(o.status.code(), Some(0), "{}", stdout(&o));
    let out = stdout(&o);
    let text = String::from_utf8(produced(&work, &out)).unwrap();
    assert!(text.contains("Karaoke\r\n"), "{text}");
    assert!(text.contains("Shape text\r\n"), "{text}");
    assert!(!text.contains("m 0 0 l"), "{text}");
    assert_eq!(
        warns(&out),
        [
            "karaoke effects removed (1 cues)",
            "drawings removed (1 cues)"
        ],
        "{out}"
    );
}

#[test]
fn ass_to_vtt_top_line() {
    let root = tmp("ass-vtt");
    let work = root.join("work");
    fs::create_dir_all(&work).unwrap();
    let a = put(
        &root,
        "a.ssa",
        b"[Script Info]\nScriptType: v4.00+\n\n\
[V4+ Styles]\nFormat: Name, Fontname, Fontsize, PrimaryColour, SecondaryColour, OutlineColour, BackColour, Bold, Italic, Underline, StrikeOut, ScaleX, ScaleY, Spacing, Angle, BorderStyle, Outline, Shadow, Alignment, MarginL, MarginR, MarginV, Encoding\n\
Style: Default,Arial,20,&H00FFFFFF,&H000000FF,&H00000000,&H00000000,0,0,0,0,100,100,0,0,1,2,0,8,10,10,10,1\n\n\
[Events]\nFormat: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\n\
Dialogue: 0,0:00:01.50,0:00:02.00,Default,,0,0,0,,Top\n",
    );
    let o = run("subtitles.ass-to-vtt", &work, &[&a]);
    assert_eq!(o.status.code(), Some(0), "{}", stdout(&o));
    let out = stdout(&o);
    assert_eq!(
        String::from_utf8(produced(&work, &out)).unwrap(),
        "WEBVTT\n\n00:00:01.500 --> 00:00:02.000 line:0\nTop\n\n"
    );
}

/// RF 6: an SRT output is UTF-8 with a BOM and CRLF, whatever the input's
/// line ends were (CRLF, LF and a lone CR mixed here).
#[test]
fn srt_output_has_bom_and_crlf() {
    let root = tmp("srt-bom");
    let work = root.join("work");
    fs::create_dir_all(&work).unwrap();
    let a = put(
        &root,
        "mixed.vtt",
        b"WEBVTT\r\n\r\n00:00:01.000 --> 00:00:02.000\nHello\r\n\r\n\
00:00:03.000 --> 00:00:04.000\rWorld\n\n",
    );
    let o = run("subtitles.vtt-to-srt", &work, &[&a]);
    assert_eq!(o.status.code(), Some(0), "{}", stdout(&o));
    let out = stdout(&o);
    let bytes = produced(&work, &out);
    assert!(bytes.starts_with(BOM), "no byte order mark");
    let body = &bytes[BOM.len()..];
    let lf = body.iter().filter(|&&b| b == b'\n').count();
    let cr = body.iter().filter(|&&b| b == b'\r').count();
    let crlf = body.windows(2).filter(|w| *w == b"\r\n").count();
    assert_eq!(lf, crlf, "a bare LF in the SRT output");
    assert_eq!(cr, crlf, "a bare CR in the SRT output");
    // Two cues: index, timing, text and the blank line after each is four.
    assert_eq!(crlf, 8, "{}", String::from_utf8_lossy(&bytes));
}

/// RF 6: a VTT output is UTF-8 without a BOM and LF only, whatever the input
/// had (a BOM, CRLF, LF and a lone CR here).
#[test]
fn vtt_output_has_no_bom_and_lf() {
    let root = tmp("vtt-lf");
    let work = root.join("work");
    fs::create_dir_all(&work).unwrap();
    let a = put(
        &root,
        "mixed.srt",
        b"\xEF\xBB\xBF1\r\n00:00:01,000 --> 00:00:02,000\nHello\r\n\r\n\
2\r00:00:03,000 --> 00:00:04,000\rWorld\n\n",
    );
    let o = run("subtitles.srt-to-vtt", &work, &[&a]);
    assert_eq!(o.status.code(), Some(0), "{}", stdout(&o));
    let out = stdout(&o);
    let bytes = produced(&work, &out);
    assert!(
        !bytes.starts_with(BOM),
        "a byte order mark in the WebVTT output"
    );
    assert!(!bytes.contains(&b'\r'), "a CR in the WebVTT output");
    assert!(bytes.starts_with(b"WEBVTT\n\n"));
}

/// RF 2: a file with no usable cue fails with the reason, and nothing is
/// left in the work folder.
#[test]
fn nothing_usable_fails_and_leaves_nothing() {
    let root = tmp("nothing");
    let work = root.join("work");
    fs::create_dir_all(&work).unwrap();
    let a = put(
        &root,
        "a.srt",
        b"1\n00:00:02,000 --> 00:00:01,000\nBackwards\n",
    );
    let o = run("subtitles.srt-to-vtt", &work, &[&a]);
    assert_eq!(o.status.code(), Some(1), "{}", stdout(&o));
    let out = stdout(&o);
    assert_eq!(lines(&out, "fail")[0][2], "no subtitles found", "{out}");
    assert!(lines(&out, "done").is_empty());
    assert!(files(&work).is_empty());
}

/// A cue that cannot be used is skipped and the rest converts. The warn gives
/// the count and the first skipped cue's line (the timing line, 1-based).
#[test]
fn skipped_cues_warn_with_the_first_line() {
    let root = tmp("skipped");
    let work = root.join("work");
    fs::create_dir_all(&work).unwrap();
    let mut text = String::new();
    for k in 1..=100 {
        // Cue 40 ends before it starts; its timing line is line 158.
        let (start, end) = if k == 40 {
            ("00:00:05,000", "00:00:04,000")
        } else {
            ("00:00:01,000", "00:00:02,000")
        };
        text.push_str(&format!("{k}\n{start} --> {end}\nLine {k}\n\n"));
    }
    let a = put(&root, "a.srt", text.as_bytes());
    let o = run("subtitles.srt-to-vtt", &work, &[&a]);
    assert_eq!(o.status.code(), Some(0), "{}", stdout(&o));
    let out = stdout(&o);
    let vtt = String::from_utf8(produced(&work, &out)).unwrap();
    assert_eq!(vtt.matches(" --> ").count(), 99);
    assert_eq!(
        warns(&out),
        ["1 items skipped (first at line 158: ends before it starts)"],
        "{out}"
    );
}

#[test]
fn non_utf8_input_fails_with_the_encoding_hint() {
    let root = tmp("latin");
    let work = root.join("work");
    fs::create_dir_all(&work).unwrap();
    let bytes = b"1\n00:00:01,000 --> 00:00:02,000\ncaf\xE9\n\n".to_vec();
    let at = bytes.iter().position(|&b| b == 0xE9).unwrap();
    let a = put(&root, "a.srt", &bytes);
    let o = run("subtitles.srt-to-vtt", &work, &[&a]);
    assert_eq!(o.status.code(), Some(1), "{}", stdout(&o));
    let out = stdout(&o);
    assert_eq!(
        lines(&out, "fail")[0][2],
        format!("not UTF-8 text (byte at {at}): convert it with Encoding first")
    );
    assert!(files(&work).is_empty());
}

/// One byte past the input limit fails before any parsing, with the size
/// copy, and nothing is written.
#[test]
fn oversize_input_fails() {
    let root = tmp("oversize");
    let work = root.join("work");
    fs::create_dir_all(&work).unwrap();
    let a = put(&root, "big.srt", &vec![b'x'; INPUT_BYTES + 1]);
    let o = run("subtitles.srt-to-vtt", &work, &[&a]);
    assert_eq!(o.status.code(), Some(1), "{}", stdout(&o));
    let out = stdout(&o);
    assert_eq!(
        lines(&out, "fail")[0][2],
        "subtitle file is larger than 8 MiB"
    );
    assert!(files(&work).is_empty());
}

/// A path that is not a regular file fails with the read's own reason. Only a
/// file over the input limit gets the size message.
#[test]
fn directory_input_fails_as_not_a_regular_file() {
    let root = tmp("notfile");
    let work = root.join("work");
    fs::create_dir_all(&work).unwrap();
    let dir = root.join("dir.srt");
    fs::create_dir_all(&dir).unwrap();
    let o = run("subtitles.srt-to-vtt", &work, &[&dir]);
    assert_eq!(o.status.code(), Some(1), "{}", stdout(&o));
    let out = stdout(&o);
    assert_eq!(lines(&out, "fail")[0][2], "not a regular file", "{out}");
    assert!(files(&work).is_empty());
}

/// A path that is not UTF-8 is unsupported, not failed: a batch of only such
/// inputs exits 3, the code for nothing usable.
#[test]
fn non_utf8_path_alone_exits_unsupported() {
    use std::os::unix::ffi::OsStrExt;
    let root = tmp("nonutf8");
    let work = root.join("work");
    fs::create_dir_all(&work).unwrap();
    let a = root.join(std::ffi::OsStr::from_bytes(b"bad\xff.srt"));
    fs::write(&a, b"1\n00:00:01,000 --> 00:00:02,000\nhi\n\n").unwrap();
    let o = run("subtitles.srt-to-vtt", &work, &[&a]);
    assert_eq!(o.status.code(), Some(3), "{}", stdout(&o));
    let out = stdout(&o);
    assert_eq!(lines(&out, "skip")[0][2], "non-UTF-8 path", "{out}");
    assert!(lines(&out, "fail").is_empty());
    assert!(files(&work).is_empty());
}

/// A small input can make a large output: `&` becomes `&amp;` in WebVTT, so
/// a 7 MiB cue of `&` is 35 MiB. The write past 32 MiB fails the input, and
/// the half-written file is removed.
#[test]
fn output_over_32_mib_fails_and_leaves_nothing() {
    let root = tmp("output");
    let work = root.join("work");
    fs::create_dir_all(&work).unwrap();
    let mut bytes = b"1\n00:00:01,000 --> 00:00:02,000\n".to_vec();
    bytes.extend(std::iter::repeat_n(b'&', 7 * MIB));
    bytes.extend_from_slice(b"\n\n");
    assert!(bytes.len() < INPUT_BYTES);
    let a = put(&root, "a.srt", &bytes);
    let o = run("subtitles.srt-to-vtt", &work, &[&a]);
    assert_eq!(o.status.code(), Some(1), "{}", stdout(&o));
    let out = stdout(&o);
    assert_eq!(
        lines(&out, "fail")[0][2],
        "subtitle output would be larger than 32 MiB"
    );
    assert!(lines(&out, "done").is_empty());
    assert!(files(&work).is_empty());
    // The ceiling is the output's own: 6 MiB of `&` (30 MiB) is written.
    let mut fits = b"1\n00:00:01,000 --> 00:00:02,000\n".to_vec();
    fits.extend(std::iter::repeat_n(b'&', 6 * MIB));
    fits.extend_from_slice(b"\n\n");
    let b = put(&root, "b.srt", &fits);
    let o = run("subtitles.srt-to-vtt", &work, &[&b]);
    assert_eq!(o.status.code(), Some(0), "{}", stdout(&o));
    assert!(produced(&work, &stdout(&o)).len() <= OUTPUT_BYTES);
}

/// Three inputs: a usable one, a bad one and a usable one. The bad one fails
/// alone; the batch goes on and exits 0 because something was done.
#[test]
fn batch_of_three_one_bad_goes_on() {
    let root = tmp("batch");
    let work = root.join("work");
    fs::create_dir_all(&work).unwrap();
    let good = b"1\n00:00:01,000 --> 00:00:02,000\nYes\n\n";
    let a = put(&root, "a.srt", good);
    let b = put(&root, "b.srt", b"not subtitles at all\n");
    let c = put(
        &root,
        "c.vtt",
        b"WEBVTT\n\n00:00:01.000 --> 00:00:02.000\nAlso\n",
    );
    let o = run("subtitles.srt-to-vtt", &work, &[&a, &b, &c]);
    assert_eq!(o.status.code(), Some(0), "{}", stdout(&o));
    let out = stdout(&o);
    assert_eq!(lines(&out, "done").len(), 2, "{out}");
    assert_eq!(lines(&out, "fail").len(), 1, "{out}");
    assert_eq!(lines(&out, "fail")[0][1], b.to_str().unwrap());
    assert_eq!(lines(&out, "fail")[0][2], "no subtitles found");
}

/// `progress 0` opens the run, then one line per input: the fraction of the
/// inputs done, so the last one is 1. A bad input counts as done too.
#[test]
fn progress_reaches_one_after_the_last_input() {
    let root = tmp("progress");
    let work = root.join("work");
    fs::create_dir_all(&work).unwrap();
    let a = put(&root, "a.srt", b"1\n00:00:01,000 --> 00:00:02,000\nYes\n\n");
    let b = put(&root, "b.srt", b"not subtitles at all\n");
    let o = run("subtitles.srt-to-vtt", &work, &[&a, &b]);
    let out = stdout(&o);
    let p: Vec<&str> = lines(&out, "progress").iter().map(|f| f[1]).collect();
    assert_eq!(p, ["0", "0.5", "1"], "{out}");
}

/// The cues are written in start order, whatever order the file gives them
/// in; cues that start together keep the file's order.
#[test]
fn cues_are_written_in_start_order() {
    let root = tmp("order");
    let work = root.join("work");
    fs::create_dir_all(&work).unwrap();
    let a = put(
        &root,
        "a.srt",
        b"1\n00:00:05,000 --> 00:00:06,000\nLate\n\n\
2\n00:00:01,000 --> 00:00:02,000\nEarly\n\n\
3\n00:00:01,000 --> 00:00:03,000\nSame\n\n",
    );
    let o = run("subtitles.srt-to-vtt", &work, &[&a]);
    assert_eq!(o.status.code(), Some(0), "{}", stdout(&o));
    assert_eq!(
        String::from_utf8(produced(&work, &stdout(&o))).unwrap(),
        "WEBVTT\n\n\
00:00:01.000 --> 00:00:02.000\nEarly\n\n\
00:00:01.000 --> 00:00:03.000\nSame\n\n\
00:00:05.000 --> 00:00:06.000\nLate\n\n"
    );
}

/// An output that already exists is never overwritten or removed: the input
/// fails with the create error and the file keeps its bytes.
#[test]
fn existing_output_is_never_overwritten() {
    let root = tmp("exists");
    let work = root.join("work");
    fs::create_dir_all(work.join("0")).unwrap();
    let kept = put(&work.join("0"), "a.vtt", b"keep me");
    let a = put(&root, "a.srt", b"1\n00:00:01,000 --> 00:00:02,000\nNew\n\n");
    let o = run("subtitles.srt-to-vtt", &work, &[&a]);
    assert_eq!(o.status.code(), Some(1), "{}", stdout(&o));
    let out = stdout(&o);
    assert!(
        lines(&out, "fail")[0][2].starts_with("cannot create output"),
        "{out}"
    );
    assert_eq!(fs::read(&kept).unwrap(), b"keep me");
}

/// Past the process's own `RLIMIT_FSIZE` (64 KiB here) a write fails with
/// EFBIG, not SIGXFSZ: the input fails with the size copy, nothing is left,
/// and the process ends normally, so the batch would go on.
#[test]
fn output_past_the_file_size_limit_fails_alone() {
    use std::os::unix::process::CommandExt;
    let root = tmp("fsize");
    let work = root.join("work");
    fs::create_dir_all(&work).unwrap();
    let a = put(
        &root,
        "a.srt",
        format!(
            "1\n00:00:01,000 --> 00:00:02,000\n{}\n\n",
            "x".repeat(200 * 1024)
        )
        .as_bytes(),
    );
    let mut c = bin();
    c.args(["run", "subtitles.srt-to-vtt", "--work"])
        .arg(&work)
        .arg("--")
        .arg(&a);
    // SAFETY: the closure runs between fork and exec and only calls
    // `setrlimit`, which is async-signal-safe, on a local struct.
    unsafe {
        c.pre_exec(|| {
            let rl = libc::rlimit {
                rlim_cur: 64 * 1024,
                rlim_max: 64 * 1024,
            };
            if libc::setrlimit(libc::RLIMIT_FSIZE, &rl) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let o = c.output().unwrap();
    assert_eq!(o.status.code(), Some(1), "{}", stdout(&o));
    let out = stdout(&o);
    assert_eq!(
        lines(&out, "fail")[0][2],
        "subtitle output would be larger than 32 MiB"
    );
    assert!(files(&work).is_empty());
}

/// The densest legal 8 MiB inputs, run under the memory net the sandbox sets
/// (`RLIMIT_DATA` 1 GiB). Each shape is measured: peak RSS (the child's own
/// `ru_maxrss`, from `wait4`) and wall time. A run that exhausts the net
/// aborts on its failed allocation, so it does not exit normally.
///
/// Shapes: the smallest SRT, WebVTT and ASS cues (about 27 B each, so about
/// 310,000 cues), and the most lines per byte (a single cue of `x` lines, two
/// bytes each in SRT and WebVTT, three in ASS with `\N`). The cue shapes are
/// kept; the two line shapes are over the item budget and are refused.
///
/// Peak RSS before the item budget (reference desktop, release and test builds
/// alike, wall): SRT cues 138 MiB, 0.2 s; SRT lines 906 MiB, 0.4 s; WebVTT cues
/// 138 MiB; ASS cues 87 MiB; ASS lines 608 MiB, 0.3 s. The line shapes hold one
/// reader `Line` and one `Vec<Run>` (capacity four) per line, so their peak
/// grows with the line count, not with the cue count.
///
/// The same five shapes after the budget, test build on the development
/// machine (the figures printed by this test): see `docs/resource-limits.md`.
#[test]
fn dense_8_mib_input_stays_within_the_data_limit() {
    let root = tmp("dense");
    let work = root.join("work");
    fs::create_dir_all(&work).unwrap();
    let srt_cue = "0:00:00,0 --> 0:00:00,1\nx\n\n";
    let srt_cues = srt_cue.repeat(INPUT_BYTES / srt_cue.len());
    let vtt_cue = "00:00.000 --> 00:00.001\nx\n\n";
    let vtt_cues = vtt_cue.repeat((INPUT_BYTES - "WEBVTT\n\n".len()) / vtt_cue.len());
    let ass_head = "[Script Info]\nScriptType: v4.00+\n\n[Events]\n\
Format: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\n";
    let ass_line = "Dialogue: 0,0:00:00.00,0:00:00.01,,,0,0,0,,x\n";
    let ass_cues = ass_line.repeat((INPUT_BYTES - ass_head.len()) / ass_line.len());
    let srt_lines = format!(
        "1\n0:00:00,0 --> 0:00:00,1\n{}\n",
        "x\n".repeat((INPUT_BYTES - 27) / 2)
    );
    let ass_lines = format!(
        "{ass_head}Dialogue: 0,0:00:00.00,0:00:00.01,,,0,0,0,,{}\n",
        "x\\N".repeat((INPUT_BYTES - ass_head.len() - 50) / 3)
    );
    // Each shape says whether its reader keeps it. The two line shapes are
    // over the item budget and are refused whole, with nothing written.
    let shapes: [(&str, &str, Vec<u8>, bool); 5] = [
        (
            "subtitles.srt-to-vtt",
            "srt-cues.srt",
            srt_cues.into_bytes(),
            true,
        ),
        (
            "subtitles.srt-to-vtt",
            "srt-lines.srt",
            srt_lines.into_bytes(),
            false,
        ),
        (
            "subtitles.vtt-to-srt",
            "vtt-cues.vtt",
            format!("WEBVTT\n\n{vtt_cues}").into_bytes(),
            true,
        ),
        (
            "subtitles.ass-to-srt",
            "ass-cues.ass",
            format!("{ass_head}{ass_cues}").into_bytes(),
            true,
        ),
        (
            "subtitles.ass-to-vtt",
            "ass-lines.ass",
            ass_lines.into_bytes(),
            false,
        ),
    ];
    for (id, name, mut bytes, kept) in shapes {
        assert!(bytes.len() <= INPUT_BYTES, "{name} is {} B", bytes.len());
        // Pad to the limit exactly with blank lines, which end no cue.
        bytes.resize(INPUT_BYTES, b'\n');
        let input = put(&root, name, &bytes);
        let started = Instant::now();
        let (o, peak_kib) = run_measured(id, &work, &input);
        let elapsed: Duration = started.elapsed();
        let out = stdout(&o);
        if kept {
            assert_eq!(
                o.status.code(),
                Some(0),
                "{name}: {}\n{out}",
                String::from_utf8_lossy(&o.stderr)
            );
            assert_eq!(lines(&out, "done").len(), 1, "{name}: {out}");
        } else {
            assert_eq!(o.status.code(), Some(1), "{name}: {out}");
            assert_eq!(lines(&out, "fail")[0][2], TOO_COMPLEX, "{name}: {out}");
            assert!(files(&work).is_empty(), "{name}: a refused input wrote");
        }
        assert!(
            peak_kib * 1024 <= PEAK_CEILING,
            "{name}: peak RSS {peak_kib} KiB is past the ceiling"
        );
        eprintln!(
            "dense {name}: {} B in, kept {kept}, peak RSS {} KiB ({:.1} MiB), wall {:.2} s",
            bytes.len(),
            peak_kib,
            peak_kib as f64 / 1024.0,
            elapsed.as_secs_f64()
        );
        for f in files(&work) {
            fs::remove_file(f.0).unwrap();
        }
    }
}

/// Each format refuses an input over the item budget with `fail`, and leaves
/// nothing. The VTT line shape is not in the dense test, so it is here.
#[test]
fn too_complex_fails_and_leaves_nothing() {
    let root = tmp("complex");
    let work = root.join("work");
    fs::create_dir_all(&work).unwrap();
    let srt = format!(
        "1\n0:00:00,0 --> 0:00:00,1\n{}",
        "x\n".repeat((INPUT_BYTES - 27) / 2)
    );
    let vtt = format!(
        "WEBVTT\n\n00:00.000 --> 00:00.001\n{}",
        "x\n".repeat((INPUT_BYTES - 40) / 2)
    );
    let ass_head = "[Events]\nFormat: Start, End, Text\nDialogue: 0:00:00.00,0:00:00.01,";
    let ass = format!(
        "{ass_head}{}\n",
        "x\\N".repeat((INPUT_BYTES - ass_head.len() - 1) / 3)
    );
    let cases = [
        ("subtitles.srt-to-vtt", "a.srt", srt),
        ("subtitles.vtt-to-srt", "b.vtt", vtt),
        ("subtitles.ass-to-srt", "c.ass", ass),
    ];
    for (id, name, text) in cases {
        assert!(text.len() <= INPUT_BYTES, "{name}");
        let input = put(&root, name, text.as_bytes());
        let o = run(id, &work, &[&input]);
        let out = stdout(&o);
        assert_eq!(o.status.code(), Some(1), "{name}: {out}");
        assert_eq!(lines(&out, "fail")[0][2], TOO_COMPLEX, "{name}: {out}");
        assert!(lines(&out, "done").is_empty(), "{name}: {out}");
        assert!(files(&work).is_empty(), "{name}: a refused input wrote");
    }
}

/// A speaker's name is shared by the lines of its cue, not copied into each.
/// Here a 2 MiB name spans 200 lines: a copy per line would hold 400 MiB at
/// once, and the shared name holds one copy. The output is then past its
/// ceiling (200 names of 2 MiB), so the run fails after parsing, which is
/// where the memory is spent.
#[test]
fn long_voice_name_is_shared_not_copied() {
    let root = tmp("voice");
    let work = root.join("work");
    fs::create_dir_all(&work).unwrap();
    let name = "N".repeat(2 * MIB);
    let text = format!(
        "WEBVTT\n\n00:00.000 --> 00:01.000\n<v {name}>\n{}",
        "x\n".repeat(200)
    );
    let input = put(&root, "voice.vtt", text.as_bytes());
    let (o, peak_kib) = run_measured("subtitles.vtt-to-srt", &work, &input);
    let out = stdout(&o);
    assert_eq!(o.status.code(), Some(1), "{out}");
    assert_eq!(
        lines(&out, "fail")[0][2],
        "subtitle output would be larger than 32 MiB",
        "{out}"
    );
    assert!(files(&work).is_empty());
    // One copy is about 2 MiB and the rest is the lines: 100 MiB is far
    // above that and far below the 400 MiB of a copy per line.
    assert!(
        peak_kib < 100 * 1024,
        "peak RSS {peak_kib} KiB: the voice name was copied per line"
    );
}

/// Runs the action with the memory net and the file-size floor the sandbox
/// sets, and returns its output and its peak RSS in KiB. The peak is this
/// child's alone (`wait4` on its pid), not the test process's.
fn run_measured(id: &str, work: &Path, input: &Path) -> (Output, u64) {
    let mut c = bin();
    c.args(["run", id, "--work"])
        .arg(work)
        .arg("--")
        .arg(input)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // SAFETY: the closure runs between fork and exec and only calls
    // `setrlimit`, which is async-signal-safe, on local structs; it touches no
    // memory of the parent and takes no lock.
    unsafe {
        c.pre_exec(|| {
            for (res, v) in [
                (libc::RLIMIT_DATA, DATA_LIMIT),
                (libc::RLIMIT_FSIZE, FSIZE_LIMIT),
            ] {
                let rl = libc::rlimit {
                    rlim_cur: v as libc::rlim_t,
                    rlim_max: v as libc::rlim_t,
                };
                if libc::setrlimit(res, &rl) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
            }
            Ok(())
        });
    }
    // Reaped by the `wait4` below, which also returns the child's rusage.
    #[allow(clippy::zombie_processes)]
    let mut child = c.spawn().unwrap();
    let mut out = Vec::new();
    let mut err = Vec::new();
    child.stdout.take().unwrap().read_to_end(&mut out).unwrap();
    child.stderr.take().unwrap().read_to_end(&mut err).unwrap();
    let pid = libc::pid_t::try_from(child.id()).unwrap();
    let mut status: libc::c_int = 0;
    // SAFETY: `ru` is zeroed, a valid `rusage`, and `status` is a valid
    // location; `wait4` writes only into them.
    let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
    // SAFETY: as above; the child is ours and not yet reaped.
    let rc = unsafe { libc::wait4(pid, &mut status, 0, &mut ru) };
    assert_eq!(rc, pid, "wait4");
    let output = Output {
        status: std::os::unix::process::ExitStatusExt::from_raw(status),
        stdout: out,
        stderr: err,
    };
    // `ru_maxrss` is in KiB on Linux.
    (output, u64::try_from(ru.ru_maxrss).unwrap_or(0))
}

/// A TTML document with a bold, italic and underlined paragraph (a `br`, and
/// a red span that is lost), a paragraph in a top region (`top`, no warning)
/// and one in a bottom region (a placement the output cannot carry).
const TTML_BASIC: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<tt xmlns="http://www.w3.org/ns/ttml" xmlns:tts="http://www.w3.org/ns/ttml#styling">
<head>
<styling><style xml:id="s1" tts:fontStyle="italic" tts:fontWeight="bold" tts:textDecoration="underline"/></styling>
<layout><region xml:id="top" tts:origin="10% 5%" tts:extent="80% 20%" tts:displayAlign="before"/><region xml:id="bottom" tts:origin="10% 80%" tts:extent="80% 15%"/></layout>
</head>
<body><div>
<p begin="00:00:01.000" end="00:00:02.500" style="s1">Hello <span tts:color="red">red</span><br/>second line</p>
<p begin="00:00:03.000" end="00:00:04.000" region="top">Top cue</p>
<p begin="00:00:05.000" end="00:00:06.000" region="bottom">Bottom cue</p>
</div></body>
</tt>
"#;

#[test]
fn ttml_to_srt_converts_and_keeps_formatting() {
    let root = tmp("ttml-srt");
    let work = root.join("work");
    fs::create_dir_all(&work).unwrap();
    let a = put(&root, "a.ttml", TTML_BASIC.as_bytes());
    let o = run("subtitles.ttml-to-srt", &work, &[&a]);
    assert_eq!(o.status.code(), Some(0), "{}", stdout(&o));
    let out = stdout(&o);
    let mut want = BOM.to_vec();
    want.extend_from_slice(
        "1\r\n00:00:01,000 --> 00:00:02,500\r\n<i><b><u>Hello red</u></b></i>\r\n\
<i><b><u>second line</u></b></i>\r\n\r\n\
2\r\n00:00:03,000 --> 00:00:04,000\r\n{\\an8}Top cue\r\n\r\n\
3\r\n00:00:05,000 --> 00:00:06,000\r\nBottom cue\r\n\r\n"
            .as_bytes(),
    );
    assert_eq!(produced(&work, &out), want);
    assert_eq!(
        warns(&out),
        [
            "colours, fonts and sizes removed (1 cues)",
            "exact positions removed (1 cues)"
        ],
        "{out}"
    );
}

#[test]
fn ttml_to_vtt_top_region_gives_line_zero() {
    let root = tmp("ttml-vtt");
    let work = root.join("work");
    fs::create_dir_all(&work).unwrap();
    let a = put(&root, "a.ttml", TTML_BASIC.as_bytes());
    let o = run("subtitles.ttml-to-vtt", &work, &[&a]);
    assert_eq!(o.status.code(), Some(0), "{}", stdout(&o));
    let out = stdout(&o);
    assert_eq!(
        String::from_utf8(produced(&work, &out)).unwrap(),
        "WEBVTT\n\n\
00:00:01.000 --> 00:00:02.500\n<i><b><u>Hello red</u></b></i>\n\
<i><b><u>second line</u></b></i>\n\n\
00:00:03.000 --> 00:00:04.000 line:0\nTop cue\n\n\
00:00:05.000 --> 00:00:06.000\nBottom cue\n\n"
    );
    assert_eq!(
        warns(&out),
        [
            "colours, fonts and sizes removed (1 cues)",
            "exact positions removed (1 cues)"
        ],
        "{out}"
    );
}

/// Frames (TTML2 12.3.1: a frame is below the frame rate). On the media time
/// base a frame is a division of a second: at 25 a second, 1:15 is 1.600 s and
/// 2:20 is 2.800 s; at 29.97 (30 x 1000/1001), 1:15 is 1.5005 s, rounded half
/// up to 1.501 s, and 2:20 is 2.66733 s, rounded to 2.667 s. On the SMPTE time
/// base (I.3) the frames are counted at the nominal rate: 1:15 is 45 frames,
/// 45 x 1001/30000 = 1.5015 s, rounded to 1.502 s, and 2:20 is 80 frames,
/// 2.66933 s, rounded to 2.669 s.
#[test]
fn frame_times_become_milliseconds() {
    let root = tmp("ttml-frames");
    let work = root.join("work");
    fs::create_dir_all(&work).unwrap();
    let tt = "<tt xmlns=\"http://www.w3.org/ns/ttml\" \
        xmlns:ttp=\"http://www.w3.org/ns/ttml#parameter\"";
    let body =
        "><body><div><p begin=\"00:00:01:15\" end=\"00:00:02:20\">Frames</p></div></body></tt>";
    let cases = [
        (
            format!("{tt} ttp:frameRate=\"25\"{body}"),
            "WEBVTT\n\n00:00:01.600 --> 00:00:02.800\nFrames\n\n",
        ),
        (
            format!("{tt} ttp:frameRate=\"30\" ttp:frameRateMultiplier=\"1000 1001\"{body}"),
            "WEBVTT\n\n00:00:01.501 --> 00:00:02.667\nFrames\n\n",
        ),
        (
            format!(
                "{tt} ttp:timeBase=\"smpte\" ttp:frameRate=\"30\" ttp:frameRateMultiplier=\"1000 1001\"{body}"
            ),
            "WEBVTT\n\n00:00:01.502 --> 00:00:02.669\nFrames\n\n",
        ),
    ];
    for (i, (text, want)) in cases.iter().enumerate() {
        let a = put(&root, &format!("{i}.ttml"), text.as_bytes());
        let o = run("subtitles.ttml-to-vtt", &work, &[&a]);
        assert_eq!(o.status.code(), Some(0), "{}", stdout(&o));
        assert_eq!(produced(&work, &stdout(&o)), want.as_bytes(), "case {i}");
        for f in files(&work) {
            fs::remove_file(f.0).unwrap();
        }
    }
}

/// The reader's line is the line where the error is found: here the `</body>`
/// on line 5, which closes the `div` that was not closed.
#[test]
fn malformed_ttml_fails_with_line_and_leaves_nothing() {
    let root = tmp("ttml-bad");
    let work = root.join("work");
    fs::create_dir_all(&work).unwrap();
    let a = put(
        &root,
        "a.ttml",
        b"<tt xmlns=\"http://www.w3.org/ns/ttml\">\n<body>\n<div>\n\
<p begin=\"0s\" end=\"1s\">x</p>\n</body></tt>\n",
    );
    let o = run("subtitles.ttml-to-srt", &work, &[&a]);
    assert_eq!(o.status.code(), Some(1), "{}", stdout(&o));
    let out = stdout(&o);
    assert_eq!(
        lines(&out, "fail")[0][2],
        "not well-formed TTML (line 5): an end tag that does not match its start tag",
        "{out}"
    );
    assert!(lines(&out, "done").is_empty());
    assert!(files(&work).is_empty());
}

#[test]
fn not_ttml_fails() {
    let root = tmp("ttml-not");
    let work = root.join("work");
    fs::create_dir_all(&work).unwrap();
    let a = put(
        &root,
        "a.ttml",
        b"<html xmlns=\"http://www.w3.org/1999/xhtml\"><body>x</body></html>",
    );
    let o = run("subtitles.ttml-to-srt", &work, &[&a]);
    assert_eq!(o.status.code(), Some(1), "{}", stdout(&o));
    let out = stdout(&o);
    assert_eq!(lines(&out, "fail")[0][2], "not a TTML document", "{out}");
    assert!(files(&work).is_empty());
}

/// A time base the reader does not convert refuses the whole file, with the
/// reason: a clock time base, and a drop-frame time code.
#[test]
fn unsupported_time_base_fails_with_the_reason() {
    let root = tmp("ttml-unsupported");
    let work = root.join("work");
    fs::create_dir_all(&work).unwrap();
    let cases = [
        (
            "clock.ttml",
            "<tt xmlns=\"http://www.w3.org/ns/ttml\" \
xmlns:ttp=\"http://www.w3.org/ns/ttml#parameter\" ttp:timeBase=\"clock\">\
<body><div><p begin=\"0s\" end=\"1s\">x</p></div></body></tt>",
            "unsupported TTML: clock time base",
        ),
        (
            "drop.ttml",
            "<tt xmlns=\"http://www.w3.org/ns/ttml\" \
xmlns:ttp=\"http://www.w3.org/ns/ttml#parameter\" ttp:timeBase=\"smpte\" \
ttp:dropMode=\"dropNTSC\"><body><div>\
<p begin=\"00:00:01:00\" end=\"00:00:02:00\">x</p></div></body></tt>",
            "unsupported TTML: drop-frame time code",
        ),
    ];
    for (name, text, reason) in cases {
        let a = put(&root, name, text.as_bytes());
        let o = run("subtitles.ttml-to-srt", &work, &[&a]);
        let out = stdout(&o);
        assert_eq!(o.status.code(), Some(1), "{name}: {out}");
        assert_eq!(lines(&out, "fail")[0][2], reason, "{name}: {out}");
        assert!(files(&work).is_empty(), "{name}: a refused input wrote");
    }
}

/// A document whose only subtitles are images has no text to convert: the
/// reason is said, not `no subtitles found`.
#[test]
fn image_only_ttml_fails_with_the_reason() {
    let root = tmp("ttml-image");
    let work = root.join("work");
    fs::create_dir_all(&work).unwrap();
    let a = put(
        &root,
        "a.ttml",
        b"<tt xmlns=\"http://www.w3.org/ns/ttml\"><body><div>\
<p begin=\"0s\" end=\"1s\"><image src=\"a.png\"/></p></div></body></tt>",
    );
    let o = run("subtitles.ttml-to-vtt", &work, &[&a]);
    assert_eq!(o.status.code(), Some(1), "{}", stdout(&o));
    let out = stdout(&o);
    assert_eq!(
        lines(&out, "fail")[0][2],
        "no text subtitles found (image-based)",
        "{out}"
    );
    assert!(files(&work).is_empty());
}

/// A document whose every paragraph is skipped says how many were skipped and
/// the first one's line and reason; the paragraph here has no end time, on
/// line 4.
#[test]
fn ttml_with_every_paragraph_skipped_says_why() {
    let root = tmp("ttml-skipped");
    let work = root.join("work");
    fs::create_dir_all(&work).unwrap();
    let a = put(
        &root,
        "a.ttml",
        b"<tt xmlns=\"http://www.w3.org/ns/ttml\">\n<body>\n<div>\n\
<p>x</p>\n</div></body></tt>\n",
    );
    let o = run("subtitles.ttml-to-srt", &work, &[&a]);
    assert_eq!(o.status.code(), Some(1), "{}", stdout(&o));
    let out = stdout(&o);
    assert_eq!(
        lines(&out, "fail")[0][2],
        "no subtitles found (1 items skipped, first at line 4: no end time)",
        "{out}"
    );
    assert!(files(&work).is_empty());
}

/// Each loss kind is one warn line, in `Loss` order whatever the document's
/// order: here the foreign element comes first in the file and is last in the
/// output. The bracket is written as its look-alike and counted there.
#[test]
fn ttml_warnings_list_losses_in_order() {
    let root = tmp("ttml-warn");
    let work = root.join("work");
    fs::create_dir_all(&work).unwrap();
    let a = put(
        &root,
        "a.ttml",
        b"<tt xmlns=\"http://www.w3.org/ns/ttml\" xmlns:tts=\"http://www.w3.org/ns/ttml#styling\" \
xmlns:foo=\"urn:example:foo\">\n\
<head><styling><style xml:id=\"c\" tts:color=\"red\"/></styling>\n\
<layout><region xml:id=\"low\" tts:origin=\"10% 80%\" tts:extent=\"80% 15%\"/></layout></head>\n\
<body><div>\n\
<p begin=\"00:00:01.000\" end=\"00:00:02.000\"><foo:note>ignored</foo:note>Plain</p>\n\
<p begin=\"00:00:03.000\" end=\"00:00:04.000\" region=\"low\" style=\"c\">Placed &lt;b&gt; text</p>\n\
</div></body></tt>\n",
    );
    let o = run("subtitles.ttml-to-srt", &work, &[&a]);
    assert_eq!(o.status.code(), Some(0), "{}", stdout(&o));
    let out = stdout(&o);
    assert_eq!(
        warns(&out),
        [
            "colours, fonts and sizes removed (1 cues)",
            "exact positions removed (1 cues)",
            "characters that look like markup written as look-alikes (1 cues)",
            "unsupported elements ignored (1 items)",
        ],
        "{out}"
    );
}

/// A UTF-16 document with its byte order mark converts like a UTF-8 one.
#[test]
fn utf16_ttml_converts() {
    let root = tmp("ttml-utf16");
    let work = root.join("work");
    fs::create_dir_all(&work).unwrap();
    let text = "<tt xmlns=\"http://www.w3.org/ns/ttml\"><body><div>\
<p begin=\"0s\" end=\"1s\">Wide</p></div></body></tt>";
    let mut bytes = vec![0xFF, 0xFE];
    bytes.extend(text.encode_utf16().flat_map(u16::to_le_bytes));
    let a = put(&root, "a.ttml", &bytes);
    let o = run("subtitles.ttml-to-srt", &work, &[&a]);
    assert_eq!(o.status.code(), Some(0), "{}", stdout(&o));
    let mut want = BOM.to_vec();
    want.extend_from_slice(b"1\r\n00:00:00,000 --> 00:00:01,000\r\nWide\r\n\r\n");
    assert_eq!(produced(&work, &stdout(&o)), want);
}

/// One byte past the input limit fails before any parsing, with the size copy.
#[test]
fn oversize_ttml_fails() {
    let root = tmp("ttml-oversize");
    let work = root.join("work");
    fs::create_dir_all(&work).unwrap();
    let mut bytes = b"<tt xmlns=\"http://www.w3.org/ns/ttml\"><body/></tt>".to_vec();
    bytes.resize(INPUT_BYTES + 1, b' ');
    let a = put(&root, "big.ttml", &bytes);
    let o = run("subtitles.ttml-to-vtt", &work, &[&a]);
    assert_eq!(o.status.code(), Some(1), "{}", stdout(&o));
    assert_eq!(
        lines(&stdout(&o), "fail")[0][2],
        "subtitle file is larger than 8 MiB"
    );
    assert!(files(&work).is_empty());
}

/// Three TTML inputs, one of them not TTML: the bad one fails alone and the
/// batch goes on.
#[test]
fn ttml_batch_of_three_one_bad_goes_on() {
    let root = tmp("ttml-batch");
    let work = root.join("work");
    fs::create_dir_all(&work).unwrap();
    let good = b"<tt xmlns=\"http://www.w3.org/ns/ttml\"><body><div>\
<p begin=\"0s\" end=\"1s\">Yes</p></div></body></tt>";
    let a = put(&root, "a.ttml", good);
    let b = put(&root, "b.ttml", b"<html/>");
    let c = put(&root, "c.dfxp", good);
    let o = run("subtitles.ttml-to-vtt", &work, &[&a, &b, &c]);
    assert_eq!(o.status.code(), Some(0), "{}", stdout(&o));
    let out = stdout(&o);
    assert_eq!(lines(&out, "done").len(), 2, "{out}");
    assert_eq!(lines(&out, "fail").len(), 1, "{out}");
    assert_eq!(lines(&out, "fail")[0][1], b.to_str().unwrap());
    assert_eq!(lines(&out, "fail")[0][2], "not a TTML document");
}

/// The densest TTML shapes that fit the 8 MiB input, each run five times under
/// the memory net, with the peak RSS of each run (`ru_maxrss`, from `wait4`).
///
/// (1) Run-dense: one paragraph of 599,179 `<span>x</span>` (14 B each). The plan
/// named 4 M spans, but 4 M of these are 56 MB, past the input limit; 599,179 is
/// the most that fit. Each span is an element, a piece and a copy of the piece
/// in its slice, each charged to the item budget.
/// (2) Cue-dense: 299,591 `<p begin="0s" end="1s">x</p>` (28 B each), one cue
/// per paragraph.
///
/// Peak RSS over five runs, KiB (min / median / max). Test build: span-dense
/// 103524 / 103916 / 103996 (101.1 / 101.5 / 101.6 MiB), cue-dense 128228 /
/// 128440 / 128584 (125.2 / 125.4 / 125.6 MiB). Release build (`cargo test
/// --release`): span-dense 103260 / 103444 / 103552 (100.8 / 101.0 / 101.1 MiB),
/// cue-dense 127828 / 127932 / 128088 (124.8 / 124.9 / 125.1 MiB). The wall time
/// is 2.6 s per run in the test build and 0.2 to 0.3 s in the release build.
///
/// The ceiling is `PEAK_CEILING`: the item budget bounds the readers at about
/// 216 MiB (`ITEM_BUDGET`), and a run past 400 MiB is a finding, not a limit to
/// raise. Both shapes stay under 130 MiB, far under that bound.
#[test]
fn dense_ttml_shapes_stay_within_the_data_limit() {
    const RUNS: usize = 5;
    let root = tmp("dense-ttml");
    let work = root.join("work");
    fs::create_dir_all(&work).unwrap();
    let span_head =
        "<tt xmlns=\"http://www.w3.org/ns/ttml\"><body><div><p begin=\"0s\" end=\"1s\">";
    let span_tail = "</p></div></body></tt>";
    let span = "<span>x</span>";
    let spans = span.repeat((INPUT_BYTES - span_head.len() - span_tail.len()) / span.len());
    let para_head = "<tt xmlns=\"http://www.w3.org/ns/ttml\"><body>";
    let para_tail = "</body></tt>";
    let para = "<p begin=\"0s\" end=\"1s\">x</p>";
    let paras = para.repeat((INPUT_BYTES - para_head.len() - para_tail.len()) / para.len());
    let shapes = [
        ("span-dense.ttml", format!("{span_head}{spans}{span_tail}")),
        ("cue-dense.ttml", format!("{para_head}{paras}{para_tail}")),
    ];
    for (name, mut text) in shapes {
        assert!(text.len() <= INPUT_BYTES, "{name}");
        // Padding after the root element: trailing white space is no content.
        let pad = INPUT_BYTES - text.len();
        text.push_str(&"\n".repeat(pad));
        let input = put(&root, name, text.as_bytes());
        let mut peaks = Vec::new();
        for _ in 0..RUNS {
            let started = Instant::now();
            let (o, peak_kib) = run_measured("subtitles.ttml-to-srt", &work, &input);
            let elapsed = started.elapsed();
            let out = stdout(&o);
            // Both shapes are converted, so the run measures the reader and the
            // writer to the end; a refusal would measure less.
            assert_eq!(o.status.code(), Some(0), "{name}: {out}");
            assert_eq!(lines(&out, "done").len(), 1, "{name}: {out}");
            eprintln!(
                "dense TTML {name}: exit {:?}, peak RSS {peak_kib} KiB, wall {:.2} s",
                o.status.code(),
                elapsed.as_secs_f64()
            );
            peaks.push(peak_kib);
            for f in files(&work) {
                fs::remove_file(f.0).unwrap();
            }
        }
        peaks.sort_unstable();
        eprintln!(
            "dense TTML {name}: peak RSS KiB min {} median {} max {} over {RUNS} runs",
            peaks[0],
            peaks[RUNS / 2],
            peaks[RUNS - 1]
        );
        assert!(
            peaks[RUNS - 1] * 1024 <= PEAK_CEILING,
            "{name}: peak RSS {} KiB is past the ceiling",
            peaks[RUNS - 1]
        );
    }
}
