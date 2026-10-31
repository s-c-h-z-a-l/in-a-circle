//! Plain-text reader and writer.

use crate::budget::{Meter, Sink};
use crate::{Align, Block, Document, Inline, LimitError, Limits, Loss, ReadError};
use std::collections::BTreeSet;

/// Reads plain text. Paragraphs are separated by blank (whitespace-only)
/// lines; lines inside a paragraph are joined by `HardBreak`. Runs of more
/// than one blank line between paragraphs are merged, blank lines before
/// the first and after the last paragraph and the spaces of whitespace-only
/// lines are dropped; each is reported as `Loss::BlankLines`.
/// Text is valid UTF-8 without NUL bytes; anything else is
/// `ReadError::NotText`. Linear in the input; no recursion. Unbudgeted
/// compatibility entry point; the shipped conversion uses [`read_limited`].
pub fn read(bytes: &[u8]) -> Result<(Document, BTreeSet<Loss>), ReadError> {
    read_limited(bytes, Limits::UNLIMITED)
}

/// [`read`] under `limits`: the input size is checked first, then every
/// paragraph, `Text` and `HardBreak` is charged as one node and every line
/// as its bytes of owned text before it is built (`ReadError::Limit`).
pub fn read_limited(bytes: &[u8], limits: Limits) -> Result<(Document, BTreeSet<Loss>), ReadError> {
    if bytes.len() > limits.input_bytes {
        return Err(LimitError::Exceeded("input").into());
    }
    let s = std::str::from_utf8(bytes).map_err(|_| ReadError::NotText)?;
    if s.contains('\0') {
        return Err(ReadError::NotText);
    }
    let s = s.strip_prefix('\u{feff}').unwrap_or(s);
    let mut m = Meter::new(limits);
    let mut losses = BTreeSet::new();
    let mut blocks = Vec::new();
    let mut inlines: Vec<Inline> = Vec::new();
    let mut blanks = 0usize;

    // Split on LF, CRLF and lone CR without allocating a normalized copy.
    let mut rest = s;
    while !rest.is_empty() && !m.failed() {
        let end = rest.find(['\n', '\r']).unwrap_or(rest.len());
        let line = &rest[..end];
        let mut next = end;
        if rest[end..].starts_with("\r\n") {
            next += 2;
        } else if end < rest.len() {
            next += 1;
        }
        rest = &rest[next..];

        if line.trim().is_empty() {
            // A whitespace-only line keeps nothing of its spaces.
            if !line.is_empty() {
                losses.insert(Loss::BlankLines);
            }
            blanks += 1;
            continue;
        }
        // A paragraph is charged when its first line starts it.
        if inlines.is_empty() {
            // Blank lines before the first paragraph are dropped.
            if blanks > 0 {
                losses.insert(Loss::BlankLines);
            }
            m.node();
        } else if blanks > 0 {
            if blanks > 1 {
                losses.insert(Loss::BlankLines);
            }
            m.push(&mut blocks, Block::Paragraph(std::mem::take(&mut inlines)));
            m.node();
        } else if m.node() {
            m.push(&mut inlines, Inline::HardBreak);
        }
        blanks = 0;
        if m.node() {
            let t = m.own(line);
            m.push(&mut inlines, Inline::Text(t));
        }
    }
    // Blank lines after the last paragraph (or a text of blank lines only)
    // are dropped. The last line's own terminator is not a blank line.
    if blanks > 0 && !m.failed() {
        losses.insert(Loss::BlankLines);
    }
    if !inlines.is_empty() {
        m.push(&mut blocks, Block::Paragraph(inlines));
    }
    m.result()?;
    Ok((Document { blocks }, losses))
}

/// Writes plain text, reporting every piece of structure that was flattened.
/// Blocks are separated by one blank line (blocks that render to nothing are
/// skipped); non-empty output ends with exactly one `\n`, empty output is `""`.
/// List nesting: content under a bullet item is indented 2 spaces, under an
/// ordered item by the marker's width (3 spaces under `1. `).
/// Unbudgeted compatibility entry point; see [`write_limited`].
pub fn write(doc: &Document) -> (String, BTreeSet<Loss>) {
    write_limited(doc, Limits::UNLIMITED).expect("allocation failed while writing plain text")
}

/// [`write`] under `limits.output_bytes`: every piece is counted before it
/// is appended, so the output never grows past the budget.
pub fn write_limited(
    doc: &Document,
    limits: Limits,
) -> Result<(String, BTreeSet<Loss>), LimitError> {
    let mut w = Writer {
        sink: Sink::new(limits),
        losses: BTreeSet::new(),
    };
    let mut out = w.blocks(&doc.blocks, true, TABLE_WIDTH);
    if !out.is_empty() {
        w.sink.push(&mut out, '\n');
    }
    w.sink.result()?;
    Ok((out, w.losses))
}

/// Full line width a table may use (prefixes of enclosing lists and quotes
/// included): the traditional width of a text file, readable in a terminal
/// window without wrapping.
const TABLE_WIDTH: usize = 80;
/// Narrowest a table column is squeezed to: ten characters still hold a
/// short word or a date, below that nearly every word would split. A column
/// whose natural width is smaller keeps it.
const MIN_COLUMN: usize = 10;
/// Spaces between two table columns (there is no frame): enough to tell
/// columns apart in a monospaced font without spending width.
const COLUMN_GAP: usize = 2;
/// A column is never narrower than its delimiter row of dashes can show.
const NARROWEST_NATURAL: usize = 3;
/// Width of the `> ` a block quote puts before every line.
const QUOTE_PREFIX: usize = 2;
/// Source of padding and dash runs: a column is never wider than the line
/// (`TABLE_WIDTH`), so no run is longer than this.
const SPACES: &str =
    "                                                                                ";
const DASHES: &str =
    "--------------------------------------------------------------------------------";

/// Column widths for a table whose columns are `natural` characters wide
/// when unwrapped, in `available` characters per line. `Some(natural)` when
/// the table fits as is; otherwise the widest columns lose width one
/// character at a time until it fits, none below `min(natural, MIN_COLUMN)`.
/// `None` when even those minimums and the gaps do not fit.
fn column_widths(natural: &[usize], available: usize) -> Option<Vec<usize>> {
    let gaps = natural.len().saturating_sub(1).saturating_mul(COLUMN_GAP);
    let budget = available.checked_sub(gaps)?;
    let minimum = natural
        .iter()
        .fold(0usize, |t, &n| t.saturating_add(n.min(MIN_COLUMN)));
    if minimum > budget {
        return None;
    }
    let total = |cap: usize| {
        natural
            .iter()
            .fold(0usize, |t, &n| t.saturating_add(n.min(cap)))
    };
    let widest = natural.iter().copied().max().unwrap_or(0);
    if total(widest) <= budget {
        return Some(natural.to_vec());
    }
    // Shrinking the widest column first one character at a time ends with
    // every wide column at `cap` or `cap + 1`: the largest cap that fits
    // (binary search; `minimum <= budget` keeps MIN_COLUMN feasible), then
    // the characters left over go back to the rightmost capped columns, as
    // the leftmost of equal columns is shrunk first.
    let (mut lo, mut hi) = (MIN_COLUMN, widest);
    while lo < hi {
        let mid = lo + (hi - lo).div_ceil(2);
        if total(mid) <= budget {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    let mut spare = budget - total(lo);
    let mut widths: Vec<usize> = natural.iter().map(|&n| n.min(lo)).collect();
    for (w, &n) in widths.iter_mut().zip(natural).rev() {
        if spare == 0 {
            break;
        }
        if n > lo {
            *w += 1;
            spare -= 1;
        }
    }
    Some(widths)
}

/// Whether `c` is shown in a monospaced terminal with a width other than
/// one cell: a combining mark (zero) or an East Asian Wide or Fullwidth
/// character, emoji included (two), or a joiner or variation selector that
/// changes how a neighbour is drawn. A small fixed table of the main blocks
/// (Unicode 15.0: EastAsianWidth.txt, emoji-data.txt and the
/// combining-mark blocks), no dependency; it only decides whether to warn,
/// the layout counts characters.
fn is_wide_or_combining(c: char) -> bool {
    matches!(u32::from(c),
        // Combining Diacritical Marks, Extended, Supplement; for Symbols;
        // Half Marks.
        0x0300..=0x036F | 0x1AB0..=0x1AFF | 0x1DC0..=0x1DFF | 0x20D0..=0x20FF | 0xFE20..=0xFE2F
        // Hangul Jamo initials.
        | 0x1100..=0x115F
        // CJK radicals through Yi: punctuation, kana, ideographs.
        | 0x2E80..=0xA4CF
        // Hangul syllables, CJK compatibility ideographs and forms.
        | 0xAC00..=0xD7A3 | 0xF900..=0xFAFF | 0xFE30..=0xFE4F
        // Fullwidth forms.
        | 0xFF00..=0xFF60 | 0xFFE0..=0xFFE6
        // Zero Width Joiner: glues emoji into one glyph (emoji-data.txt,
        // Emoji_Component); width zero.
        | 0x200D
        // Miscellaneous Symbols and Dingbats: the emoji-presentation
        // characters there (check marks, crosses, stars, weather, signs)
        // are wide in EastAsianWidth.txt, and the rest become emoji with
        // VS16. The whole blocks are taken, a few narrow symbols only cost
        // an extra warning.
        | 0x2600..=0x27BF
        // Hourglass, media controls (Misc Technical, wide emoji in
        // EastAsianWidth.txt).
        | 0x231A..=0x231B | 0x23E9..=0x23F3 | 0x23F8..=0x23FA
        // Large squares, star, circle (Misc Symbols and Arrows, wide
        // emoji in EastAsianWidth.txt).
        | 0x2B1B..=0x2B1C | 0x2B50 | 0x2B55
        // Variation selectors: VS16 (U+FE0F) switches a symbol to emoji
        // presentation (width two), the rest alter glyph shapes.
        | 0xFE00..=0xFE0F
        // Mahjong, cards, enclosed alphanumeric and ideographic
        // supplements (wide in EastAsianWidth.txt).
        | 0x1F000..=0x1F2FF
        // Emoji and pictographs; CJK extensions B and later.
        | 0x1F300..=0x1FAFF | 0x20000..=0x3FFFD)
}

/// Splits `text` (no newline) into lines of at most `width` characters
/// (`width >= 1`): at spaces, the space at a break consumed; a word longer
/// than `width` is cut at character boundaries. Joining the lines with a
/// space where a space was consumed gives the text back. Always at least
/// one line.
fn wrap(text: &str, width: usize) -> Vec<&str> {
    let mut lines = Vec::new();
    // The line being built: byte range and character count.
    let (mut start, mut end, mut chars) = (0usize, 0usize, 0usize);
    let mut open = false;
    let mut at = 0usize;
    for word in text.split(' ') {
        let wc = word.chars().count();
        if open && chars + 1 + wc <= width {
            end = at + word.len();
            chars += 1 + wc;
        } else {
            if open {
                lines.push(&text[start..end]);
            }
            start = at;
            end = at + word.len();
            chars = wc;
            open = true;
            // A word too long for a line: whole lines of `width`
            // characters, the rest starts the next line.
            while chars > width {
                let cut = text[start..end]
                    .char_indices()
                    .nth(width)
                    .map_or(end, |(i, _)| start + i);
                lines.push(&text[start..cut]);
                start = cut;
                chars -= width;
            }
        }
        at += word.len() + 1;
    }
    if open {
        lines.push(&text[start..end]);
    }
    lines
}

struct Writer {
    sink: Sink,
    losses: BTreeSet<Loss>,
}

impl Writer {
    fn blocks(&mut self, blocks: &[Block], loose: bool, width: usize) -> String {
        let sep = if loose { "\n\n" } else { "\n" };
        let mut out = String::new();
        let mut any = false;
        for b in blocks {
            if self.sink.failed() {
                break;
            }
            if let Some(s) = self.block(b, width)
                && !s.is_empty()
            {
                if any {
                    self.sink.push_str(&mut out, sep);
                }
                self.sink.push_str(&mut out, &s);
                any = true;
            }
        }
        out
    }

    fn block(&mut self, b: &Block, width: usize) -> Option<String> {
        Some(match b {
            Block::Paragraph(c) => self.inlines(c),
            Block::Heading { level, content } => {
                if *level != 1 {
                    self.losses.insert(Loss::HeadingLevels);
                }
                self.inlines(content)
            }
            Block::CodeBlock { text, .. } => {
                let text = text.strip_suffix('\n').unwrap_or(text);
                self.prefix_lines(text, "    ")
            }
            Block::BlockQuote(inner) => {
                // The `> ` prefix takes two columns of the line.
                let body = self.blocks(inner, true, width.saturating_sub(QUOTE_PREFIX));
                if body.is_empty() {
                    return None;
                }
                let mut out = String::new();
                for (i, line) in body.split('\n').enumerate() {
                    if i > 0 {
                        self.sink.push(&mut out, '\n');
                    }
                    self.sink
                        .push_str(&mut out, if line.is_empty() { ">" } else { "> " });
                    self.sink.push_str(&mut out, line);
                }
                out
            }
            Block::List {
                start,
                tight,
                items,
            } => {
                let mut out = String::new();
                for (i, item) in items.iter().enumerate() {
                    if self.sink.failed() {
                        break;
                    }
                    if i > 0 {
                        self.sink
                            .push_str(&mut out, if *tight { "\n" } else { "\n\n" });
                    }
                    let marker = match start {
                        None => "- ".to_string(),
                        Some(n) => format!("{}. ", n.saturating_add(i as u64)),
                    };
                    // Continuation lines are indented by `pad`, so the
                    // item's blocks get that much less of the line.
                    let pad = if start.is_none() { 2 } else { marker.len() };
                    let mut body = self.blocks(&item.blocks, !*tight, width.saturating_sub(pad));
                    if let Some(done) = item.checked {
                        let mut marked = String::new();
                        self.sink
                            .push_str(&mut marked, if done { "[x]" } else { "[ ]" });
                        // No trailing space after the box of an empty item.
                        if !body.is_empty() {
                            self.sink.push(&mut marked, ' ');
                        }
                        self.sink.push_str(&mut marked, &body);
                        body = marked;
                    }
                    for (j, line) in body.split('\n').enumerate() {
                        if j == 0 {
                            self.sink.push_str(&mut out, &marker);
                        } else {
                            self.sink.push(&mut out, '\n');
                            if !line.is_empty() {
                                for _ in 0..pad {
                                    self.sink.push(&mut out, ' ');
                                }
                            }
                        }
                        self.sink.push_str(&mut out, line);
                    }
                    if body.is_empty() {
                        out.truncate(out.trim_end().len());
                    }
                }
                out
            }
            Block::Table { align, head, rows } => self.table(align, head, rows, width),
            Block::ThematicBreak => "----".to_string(),
            Block::HtmlBlock(_) => {
                self.losses.insert(Loss::Html);
                return None;
            }
        })
    }

    /// One cell as a single line of text: its inlines written plain, line
    /// breaks as spaces.
    fn cell_text(&mut self, cell: &[Inline]) -> String {
        let text = self.inlines(cell);
        if !text.contains('\n') {
            return text;
        }
        let mut one = String::new();
        for c in text.chars() {
            self.sink.push(&mut one, if c == '\n' { ' ' } else { c });
        }
        one
    }

    fn table(
        &mut self,
        align: &[Align],
        head: &[Vec<Inline>],
        rows: &[Vec<Vec<Inline>>],
        width: usize,
    ) -> String {
        // The cell grid holds one `String` header per cell besides the
        // text; empty cells make that header the whole cost. Charged to the
        // output budget before anything is allocated, so a table of
        // millions of empty cells fails like any other overrun instead of
        // taking memory the output never shows.
        let cells = head
            .len()
            .saturating_add(rows.iter().map(Vec::len).sum::<usize>());
        let grid_bytes = cells.saturating_mul(std::mem::size_of::<String>());
        if !self.sink.charge(grid_bytes) {
            return String::new();
        }
        let mut grid: Vec<Vec<String>> = Vec::new();
        for row in std::iter::once(head).chain(rows.iter().map(Vec::as_slice)) {
            let mut cells = Vec::with_capacity(row.len());
            for cell in row {
                cells.push(self.cell_text(cell));
            }
            grid.push(cells);
            if self.sink.failed() {
                return String::new();
            }
        }
        if grid
            .iter()
            .flatten()
            .any(|cell| cell.chars().any(is_wide_or_combining))
        {
            self.losses.insert(Loss::TableWidth);
        }
        let natural: Vec<usize> = (0..head.len())
            .map(|j| {
                grid.iter()
                    .filter_map(|row| row.get(j))
                    .map(|cell| cell.chars().count())
                    .fold(NARROWEST_NATURAL, usize::max)
            })
            .collect();
        match column_widths(&natural, width) {
            Some(widths) => self.aligned_table(align, &widths, &grid),
            None => {
                self.losses.insert(Loss::TableLayout);
                self.listed_table(&grid)
            }
        }
    }

    /// The table as aligned columns: the header, a row of dashes, the rows.
    fn aligned_table(&mut self, align: &[Align], widths: &[usize], grid: &[Vec<String>]) -> String {
        let mut out = String::new();
        let mut first = true;
        let line = |w: &mut Self, out: &mut String, pieces: &[(&str, usize, Align)]| {
            // Trailing empty columns are left out, and the last column
            // written has no right padding: no line ends in a space.
            let Some(last) = pieces.iter().rposition(|(t, ..)| !t.is_empty()) else {
                return;
            };
            for (j, (text, cells, how)) in pieces.iter().enumerate().take(last + 1) {
                if j > 0 {
                    w.fill(out, SPACES, COLUMN_GAP);
                }
                let slack = widths[j] - cells;
                let (left, right) = match how {
                    Align::Right => (slack, 0),
                    Align::Center => (slack / 2, slack - slack / 2),
                    Align::Left | Align::None => (0, slack),
                };
                w.fill(out, SPACES, left);
                w.sink.push_str(out, text);
                if j < last {
                    w.fill(out, SPACES, right);
                }
            }
        };
        for (r, row) in grid.iter().enumerate() {
            let wrapped: Vec<Vec<&str>> = row
                .iter()
                .enumerate()
                .map(|(j, cell)| wrap(cell, widths[j]))
                .collect();
            let height = wrapped.iter().map(Vec::len).max().unwrap_or(1);
            for k in 0..height {
                if self.sink.failed() {
                    return out;
                }
                if !first {
                    self.sink.push(&mut out, '\n');
                }
                first = false;
                let pieces: Vec<(&str, usize, Align)> = wrapped
                    .iter()
                    .enumerate()
                    .map(|(j, lines)| {
                        let t = lines.get(k).copied().unwrap_or("");
                        (
                            t,
                            t.chars().count(),
                            align.get(j).copied().unwrap_or(Align::None),
                        )
                    })
                    .collect();
                line(self, &mut out, &pieces);
            }
            if r == 0 {
                self.sink.push(&mut out, '\n');
                for (j, w) in widths.iter().enumerate() {
                    if j > 0 {
                        self.fill(&mut out, SPACES, COLUMN_GAP);
                    }
                    self.fill(&mut out, DASHES, *w);
                }
            }
        }
        out
    }

    /// The fallback for a table too wide to align: per row one
    /// `header: cell` line per column, a blank line between rows. An empty
    /// header is called `Column <n>`; a table without rows lists its header
    /// cells.
    fn listed_table(&mut self, grid: &[Vec<String>]) -> String {
        let mut out = String::new();
        let head = &grid[0];
        if grid.len() == 1 {
            for (j, cell) in head.iter().enumerate() {
                if j > 0 {
                    self.sink.push(&mut out, '\n');
                }
                self.sink.push_str(&mut out, cell);
            }
            return out;
        }
        for (r, row) in grid[1..].iter().enumerate() {
            if r > 0 {
                self.sink.push_str(&mut out, "\n\n");
            }
            for (j, cell) in row.iter().enumerate() {
                if self.sink.failed() {
                    return out;
                }
                if j > 0 {
                    self.sink.push(&mut out, '\n');
                }
                match head.get(j).filter(|h| !h.is_empty()) {
                    Some(h) => self.sink.push_str(&mut out, h),
                    None => {
                        self.sink.push_str(&mut out, "Column ");
                        self.sink.push_str(&mut out, &(j + 1).to_string());
                    }
                }
                self.sink.push(&mut out, ':');
                if !cell.is_empty() {
                    self.sink.push(&mut out, ' ');
                    self.sink.push_str(&mut out, cell);
                }
            }
        }
        out
    }

    /// `n` characters of `run` (a run of one repeated character no shorter
    /// than `n`, widths being at most the line width), in budgeted pieces.
    fn fill(&mut self, out: &mut String, run: &str, n: usize) {
        let mut left = n;
        while left > 0 {
            let step = left.min(run.len());
            self.sink.push_str(out, &run[..step]);
            left -= step;
        }
    }

    fn prefix_lines(&mut self, text: &str, prefix: &str) -> String {
        let mut out = String::new();
        for (i, line) in text.split('\n').enumerate() {
            if i > 0 {
                self.sink.push(&mut out, '\n');
            }
            if !line.is_empty() {
                self.sink.push_str(&mut out, prefix);
            }
            self.sink.push_str(&mut out, line);
        }
        out
    }

    fn inlines(&mut self, inlines: &[Inline]) -> String {
        let mut out = String::new();
        self.push_inlines(inlines, &mut out);
        out
    }

    fn push_inlines(&mut self, inlines: &[Inline], out: &mut String) {
        for i in inlines {
            if self.sink.failed() {
                return;
            }
            match i {
                Inline::Text(t) => self.sink.push_str(out, t),
                Inline::Emph(c) | Inline::Strong(c) => {
                    self.losses.insert(Loss::Emphasis);
                    self.push_inlines(c, out);
                }
                Inline::Strikethrough(c) => {
                    self.losses.insert(Loss::Strikethrough);
                    self.push_inlines(c, out);
                }
                Inline::Code(t) => {
                    self.losses.insert(Loss::InlineCode);
                    self.sink.push_str(out, t);
                }
                Inline::Link { dest, content, .. } => {
                    self.losses.insert(Loss::Links);
                    let text = self.inlines(content);
                    if dest.is_empty() || text == *dest {
                        self.sink
                            .push_str(out, if dest.is_empty() { &text } else { dest });
                    } else if text.is_empty() {
                        self.sink.push_str(out, dest);
                    } else {
                        self.sink.push_str(out, &text);
                        self.sink.push_str(out, " (");
                        self.sink.push_str(out, dest);
                        self.sink.push(out, ')');
                    }
                }
                Inline::Image { alt, .. } => {
                    self.losses.insert(Loss::Images);
                    self.push_inlines(alt, out);
                }
                Inline::SoftBreak => self.sink.push(out, ' '),
                Inline::HardBreak => self.sink.push(out, '\n'),
                Inline::Html(_) => {
                    self.losses.insert(Loss::Html);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plain_items;

    fn t(s: &str) -> Inline {
        Inline::Text(s.to_string())
    }
    fn para(s: &str) -> Block {
        Block::Paragraph(vec![t(s)])
    }

    #[test]
    fn crlf_equals_lf() {
        assert_eq!(
            read(b"a\r\nb\r\n\r\nc").unwrap(),
            read(b"a\nb\n\nc").unwrap()
        );
        assert_eq!(read(b"a\rb").unwrap(), read(b"a\nb").unwrap());
    }

    #[test]
    fn lines_in_paragraph_are_hard_breaks() {
        let (d, _) = read(b"a\nb\n\nc").unwrap();
        assert_eq!(
            d.blocks,
            vec![
                Block::Paragraph(vec![t("a"), Inline::HardBreak, t("b")]),
                para("c")
            ]
        );
    }

    #[test]
    fn bom_stripped() {
        assert_eq!(read(b"\xef\xbb\xbfa").unwrap(), read(b"a").unwrap());
    }

    #[test]
    fn invalid_utf8_is_error() {
        assert_eq!(
            ReadError::NotText.to_string(),
            "not text (invalid UTF-8 or a NUL byte)"
        );
        assert_eq!(read(b"\xff").unwrap_err(), ReadError::NotText);
    }

    #[test]
    fn nul_byte_is_error() {
        assert_eq!(read(b"a\0b").unwrap_err(), ReadError::NotText);
        assert_eq!(read(b"\0").unwrap_err(), ReadError::NotText);
    }

    #[test]
    fn empty_and_blank_input() {
        let (d, l) = read(b"").unwrap();
        assert!(d.blocks.is_empty() && l.is_empty());
        // Nothing but blank lines: they are dropped, and said so.
        let (d, l) = read(b"  \n\n").unwrap();
        assert!(d.blocks.is_empty());
        assert!(l.contains(&Loss::BlankLines));
    }

    #[test]
    fn edge_and_whitespace_only_lines_reported() {
        for input in [
            &b"\n\n\nalpha\n"[..],
            b"\nalpha\n",
            b"alpha\n\n\n\n",
            b"alpha\n\n",
            b"a\n   \nb\n",
            b"a\n\t\nb\n",
            b"a\r\n\r\n",
        ] {
            let (_, l) = read(input).unwrap();
            assert!(l.contains(&Loss::BlankLines), "{input:?}");
        }
        // Terminators and single blank separators lose nothing.
        for input in [&b"alpha\n"[..], b"alpha", b"a\n\nb\n", b"a\r\n\r\nb\r\n"] {
            let (_, l) = read(input).unwrap();
            assert!(l.is_empty(), "{input:?}");
        }
    }

    #[test]
    fn blank_lines_merged_reported() {
        let (d, l) = read(b"a\n\n\n\nb").unwrap();
        assert_eq!(d.blocks, vec![para("a"), para("b")]);
        assert!(l.contains(&Loss::BlankLines));
        let (_, l) = read(b"a\n\nb").unwrap();
        assert!(l.is_empty());
    }

    #[test]
    fn leading_spaces_kept() {
        let (d, _) = read(b"  x").unwrap();
        assert_eq!(d.blocks, vec![para("  x")]);
    }

    #[test]
    fn many_lines_linear_and_flat() {
        let input = "x\n".repeat(200_000);
        let (d, _) = read(input.as_bytes()).unwrap();
        assert_eq!(d.blocks.len(), 1);
    }

    #[test]
    fn writer_drops_emphasis_reports_loss() {
        let d = Document {
            blocks: vec![Block::Paragraph(vec![Inline::Strong(vec![t("a")])])],
        };
        let (s, l) = write(&d);
        assert_eq!(s, "a\n");
        assert_eq!(l, BTreeSet::from([Loss::Emphasis]));
    }

    #[test]
    fn writer_link_text_and_url() {
        let link = |text: &str| Inline::Link {
            dest: "https://x".into(),
            title: String::new(),
            content: vec![t(text)],
            auto: false,
        };
        let d = Document {
            blocks: vec![Block::Paragraph(vec![link("x")])],
        };
        let (s, l) = write(&d);
        assert_eq!(s, "x (https://x)\n");
        assert_eq!(l, BTreeSet::from([Loss::Links]));
        let d = Document {
            blocks: vec![Block::Paragraph(vec![link("https://x")])],
        };
        assert_eq!(write(&d).0, "https://x\n");
    }

    #[test]
    fn writer_image_code_html_breaks() {
        let d = Document {
            blocks: vec![Block::Paragraph(vec![
                Inline::Image {
                    dest: "u".into(),
                    title: String::new(),
                    alt: vec![t("alt")],
                },
                Inline::SoftBreak,
                Inline::Code("c".into()),
                Inline::HardBreak,
                Inline::Html("<b>".into()),
                t("z"),
            ])],
        };
        let (s, l) = write(&d);
        assert_eq!(s, "alt c\nz\n");
        assert_eq!(
            l,
            BTreeSet::from([Loss::Images, Loss::InlineCode, Loss::Html])
        );
    }

    #[test]
    fn writer_heading_levels() {
        let h = |level| Block::Heading {
            level,
            content: vec![t("H")],
        };
        let (s, l) = write(&Document {
            blocks: vec![h(1), para("p")],
        });
        assert_eq!(s, "H\n\np\n");
        assert!(l.is_empty());
        let (_, l) = write(&Document { blocks: vec![h(2)] });
        assert_eq!(l, BTreeSet::from([Loss::HeadingLevels]));
    }

    #[test]
    fn writer_nested_list_indent() {
        let inner = Block::List {
            start: None,
            tight: true,
            items: plain_items(vec![vec![para("b")]]),
        };
        let outer = Block::List {
            start: None,
            tight: true,
            items: plain_items(vec![vec![para("a"), inner], vec![para("c")]]),
        };
        assert_eq!(
            write(&Document {
                blocks: vec![outer]
            })
            .0,
            "- a\n  - b\n- c\n"
        );
    }

    #[test]
    fn writer_ordered_list_numbers() {
        let l = Block::List {
            start: Some(3),
            tight: true,
            items: plain_items(vec![vec![para("a")], vec![para("b")]]),
        };
        assert_eq!(write(&Document { blocks: vec![l] }).0, "3. a\n4. b\n");
    }

    #[test]
    fn writer_loose_list_separates_items() {
        let l = Block::List {
            start: None,
            tight: false,
            items: plain_items(vec![vec![para("a")], vec![para("b")]]),
        };
        assert_eq!(write(&Document { blocks: vec![l] }).0, "- a\n\n- b\n");
    }

    #[test]
    fn writer_code_quote_rule_html_block() {
        let d = Document {
            blocks: vec![
                Block::CodeBlock {
                    info: "rs".into(),
                    text: "x\n\ny\n".into(),
                },
                Block::BlockQuote(vec![para("q1"), para("q2")]),
                Block::ThematicBreak,
                Block::HtmlBlock("<p>".into()),
            ],
        };
        let (s, l) = write(&d);
        assert_eq!(s, "    x\n\n    y\n\n> q1\n>\n> q2\n\n----\n");
        assert_eq!(l, BTreeSet::from([Loss::Html]));
    }

    #[test]
    fn loss_messages_cover_every_variant() {
        use Loss::*;
        for l in [
            Emphasis,
            InlineCode,
            Links,
            Images,
            Html,
            HeadingLevels,
            BlankLines,
            Whitespace,
        ] {
            assert!(!l.message().is_empty());
        }
        assert_eq!(Emphasis.message(), "emphasis removed");
    }

    #[test]
    fn writer_trailing_newline_exactly_one() {
        assert_eq!(write(&Document { blocks: vec![] }).0, "");
        let html_only = Document {
            blocks: vec![
                Block::HtmlBlock("<p>".into()),
                Block::Paragraph(vec![Inline::Html("<b>".into())]),
            ],
        };
        assert_eq!(write(&html_only).0, "");
        let s = write(&Document {
            blocks: vec![para("a"), para("b")],
        })
        .0;
        assert!(s.ends_with('\n') && !s.ends_with("\n\n"));
    }

    #[test]
    fn writer_skips_empty_blocks() {
        let d = Document {
            blocks: vec![
                para("a"),
                Block::Paragraph(vec![Inline::Html("<b>".into())]),
                Block::BlockQuote(vec![]),
                Block::List {
                    start: None,
                    tight: true,
                    items: plain_items(vec![]),
                },
                para("b"),
            ],
        };
        assert_eq!(write(&d).0, "a\n\nb\n");
    }

    #[test]
    fn writer_ordered_list_nesting_uses_marker_width() {
        let inner = Block::List {
            start: None,
            tight: true,
            items: plain_items(vec![vec![para("b")]]),
        };
        let l = Block::List {
            start: Some(1),
            tight: true,
            items: plain_items(vec![vec![para("a"), inner]]),
        };
        assert_eq!(write(&Document { blocks: vec![l] }).0, "1. a\n   - b\n");
    }
    fn gfm(input: &str) -> Document {
        crate::md::read_with(input.as_bytes(), crate::Dialect::Gfm).unwrap()
    }
    fn gfm_txt(input: &str) -> (String, BTreeSet<Loss>) {
        write(&gfm(input))
    }

    #[test]
    fn column_widths_natural_fit() {
        assert_eq!(column_widths(&[5, 8, 12], 80), Some(vec![5, 8, 12]));
    }

    #[test]
    fn column_widths_shrink_widest_first() {
        let w = column_widths(&[40, 40, 3], 80).unwrap();
        assert_eq!(w.iter().sum::<usize>() + 2 * COLUMN_GAP, 80);
        assert_eq!(w[2], 3);
        assert!(w.iter().all(|&c| c >= 3 && (c >= MIN_COLUMN || c == 3)));
        assert_eq!(w, vec![36, 37, 3]);
        // A narrow column is kept while a wide one gives up its width.
        assert_eq!(column_widths(&[200, 12], 40), Some(vec![26, 12]));
    }

    #[test]
    fn column_widths_fallback() {
        assert_eq!(column_widths(&[30; 7], 80), None);
        // Six columns of ten fit exactly: 6 * 10 + 5 * 2 = 70.
        assert_eq!(column_widths(&[30; 6], 80).unwrap().len(), 6);
        // Narrow columns count with their own width.
        assert!(column_widths(&[3; 15], 80).is_some());
        assert_eq!(column_widths(&[5], 4), None);
    }

    #[test]
    fn table_aligned_columns() {
        let (s, l) = gfm_txt(
            "| Name | Qty | Mid |\n|:--|--:|:-:|\n| apple | 3 | ab |\n| fig | 12345 | abcd |\n",
        );
        assert_eq!(
            s,
            "Name     Qty  Mid\n\
             -----  -----  ----\n\
             apple      3   ab\n\
             fig    12345  abcd\n"
        );
        assert!(l.is_empty(), "{l:?}");
    }

    #[test]
    fn long_cell_wraps_on_spaces() {
        let cell =
            "alpha beta gamma delta epsilon zeta eta theta iota kappa lambda mu nu xi omicron";
        let input = format!("| a | b |\n|--|--|\n| {cell} | {cell} |\n");
        let (s, _) = gfm_txt(&input);
        assert!(s.lines().all(|l| l.chars().count() <= 80), "{s}");
        let mut col = [String::new(), String::new()];
        for line in s.lines().skip(2) {
            for (j, part) in [
                (0, &line[..39.min(line.len())]),
                (1, line.get(41..).unwrap_or("")),
            ] {
                if !part.trim().is_empty() {
                    if !col[j].is_empty() {
                        col[j].push(' ');
                    }
                    col[j].push_str(part.trim_end());
                }
            }
        }
        assert_eq!(col[0], cell);
        assert_eq!(col[1], cell);
    }

    #[test]
    fn long_word_breaks_hard() {
        let word = "x".repeat(150);
        let input = format!("| a |\n|--|\n| {word} |\n");
        let (s, _) = gfm_txt(&input);
        assert!(s.lines().all(|l| l.chars().count() <= 80), "{s}");
        let body: String = s.lines().skip(2).collect();
        assert_eq!(body, word);
        assert_eq!(wrap(&word, 80), vec![&word[..80], &word[80..]]);
        assert_eq!(wrap("ab cd ef", 5), vec!["ab cd", "ef"]);
        assert_eq!(wrap("", 5), vec![""]);
    }

    #[test]
    fn fallback_block_lists_rows() {
        let long = "c".repeat(30);
        let head = ["Name", "", "C", "D", "E", "F", "G", "H"];
        let row = |a: &str| {
            let mut v = vec![a.to_string(), long.clone()];
            v.extend((0..6).map(|_| long.clone()));
            v.join(" | ")
        };
        let input = format!(
            "| {} |\n|{}\n| {} |\n| {} |\n",
            head.join(" | "),
            "--|".repeat(8),
            row("Anna"),
            row("Bela")
        );
        let (s, l) = gfm_txt(&input);
        let block = |n: &str| {
            format!(
                "Name: {n}\nColumn 2: {long}\nC: {long}\nD: {long}\nE: {long}\nF: {long}\nG: {long}\nH: {long}"
            )
        };
        assert_eq!(s, format!("{}\n\n{}\n", block("Anna"), block("Bela")));
        assert_eq!(l, BTreeSet::from([Loss::TableLayout]));
    }

    #[test]
    fn nested_table_fits_the_remaining_width() {
        let wide = "w".repeat(40);
        let table = format!("| a | b |\n|--|--|\n| {wide} | {wide} |\n");
        // Three list levels (2 spaces each) and a quote: 8 columns gone.
        let nested = |t: &str, depth: usize| {
            let indent =
                |s: &str, p: &str| s.lines().map(|l| format!("{p}{l}\n")).collect::<String>();
            let mut s = t.to_string();
            for _ in 0..depth {
                s = format!("- x\n{}", indent(&s, "  "));
            }
            indent(&s, "> ")
        };
        let (s, l) = gfm_txt(&nested(&table, 3));
        assert!(s.lines().all(|l| l.chars().count() <= 80), "{s}");
        assert!(s.contains("w   "), "{s}");
        assert!(l.is_empty(), "{l:?}");
        // Six columns of ten fit 80 columns but not what is left in five
        // list levels and a quote (12 columns): the fallback block.
        let six = |c: &str| {
            format!(
                "| {} |\n|{}\n| {} |\n",
                ["h"; 6].join(" | "),
                "--|".repeat(6),
                [c; 6].join(" | ")
            )
        };
        let (s, l) = gfm_txt(&six(&"c".repeat(10)));
        assert!(s.contains("h  "), "{s}");
        assert!(l.is_empty());
        let (s, l) = gfm_txt(&nested(&six(&"c".repeat(10)), 5));
        assert!(s.contains("h: cccccccccc"), "{s}");
        assert!(s.lines().all(|l| l.chars().count() <= 80), "{s}");
        assert_eq!(l, BTreeSet::from([Loss::TableLayout]));
    }

    #[test]
    fn wide_characters_warn() {
        let (s, l) = gfm_txt("| a | b |\n|--|--|\n| \u{6f22}\u{5b57} | x |\n");
        assert_eq!(l, BTreeSet::from([Loss::TableWidth]));
        assert!(s.contains('\u{6f22}'));
        let (_, l) = gfm_txt("| a |\n|--|\n| \u{1F600} |\n");
        assert_eq!(l, BTreeSet::from([Loss::TableWidth]));
        let (_, l) = gfm_txt("| a |\n|--|\n| e\u{301} |\n");
        assert_eq!(l, BTreeSet::from([Loss::TableWidth]));
        let (_, l) = gfm_txt("| a |\n|--|\n| plain |\n");
        assert!(l.is_empty());
        assert!(!is_wide_or_combining('a') && !is_wide_or_combining('\u{e9}'));
    }

    #[test]
    fn common_emoji_symbols_warn() {
        let (_, l) = gfm_txt("| ok | n |\n|--|--|\n| \u{2705} | \u{2B50} |\n| \u{274C} | x |\n");
        assert_eq!(l, BTreeSet::from([Loss::TableWidth]));
        for c in ['\u{200D}', '\u{FE0F}', '\u{1F004}'] {
            assert!(is_wide_or_combining(c), "{c:?}");
        }
        let (_, l) = gfm_txt("| a | b |\n|---|---|\n| ok | 1 |\n| no | 2 |\n");
        assert!(l.is_empty());
    }

    #[test]
    fn many_empty_cells_are_charged_to_the_output_budget() {
        let mut input = String::from("| a | b | c | d |\n|--|--|--|--|\n");
        for _ in 0..2000 {
            input.push_str("|  |  |  |  |\n");
        }
        let d = gfm(&input);
        let full = write(&d).0.len();
        let lim = Limits {
            output_bytes: full,
            ..Limits::UNLIMITED
        };
        assert_eq!(
            write_limited(&d, lim).unwrap_err(),
            LimitError::Exceeded("output")
        );
    }

    #[test]
    fn strikethrough_drops_markup() {
        let (s, l) = gfm_txt("~~a~~ b\n");
        assert_eq!(s, "a b\n");
        assert_eq!(l, BTreeSet::from([Loss::Strikethrough]));
    }

    #[test]
    fn task_list_markers() {
        let (s, l) = gfm_txt("- [x] kész\n- [ ] nyitott\n- [ ]\n");
        assert_eq!(s, "- [x] kész\n- [ ] nyitott\n- [ ]\n");
        assert!(l.is_empty());
        // An empty task item has no trailing space after its box.
        let empty = |checked| crate::ListItem {
            checked: Some(checked),
            blocks: vec![],
        };
        let d = Document {
            blocks: vec![Block::List {
                start: None,
                tight: true,
                items: vec![empty(false), empty(true)],
            }],
        };
        assert_eq!(write(&d).0, "- [ ]\n- [x]\n");
    }

    #[test]
    fn huge_table_fails_on_the_output_budget() {
        let mut input = String::from("| a | b |\n|--|--|\n");
        for _ in 0..2000 {
            // Cells long enough that the output, not the cell grid's
            // per-cell headers, sets the budget edge.
            input.push_str(&format!("| {} | {} |\n", "1".repeat(30), "2".repeat(30)));
        }
        let d = gfm(&input);
        let full = write(&d).0.len();
        let lim = |n| Limits {
            output_bytes: n,
            ..Limits::UNLIMITED
        };
        assert!(write_limited(&d, lim(full)).is_ok());
        assert_eq!(
            write_limited(&d, lim(full - 1)).unwrap_err(),
            LimitError::Exceeded("output")
        );
        // The fallback layout is charged the same way.
        let mut wide = String::from("| a | b | c | d | e | f | g |\n|-|-|-|-|-|-|-|\n");
        for _ in 0..500 {
            wide.push_str(&format!("| {} |\n", vec!["x".repeat(30); 7].join(" | ")));
        }
        let d = gfm(&wide);
        let full = write(&d).0.len();
        assert!(write_limited(&d, lim(full)).is_ok());
        assert!(write_limited(&d, lim(full - 1)).is_err());
    }

    #[test]
    fn no_gfm_input_output_unchanged() {
        let d = crate::md::read(
            b"# T\n\n> q1\n>\n> q2\n\n- a\n  - b\n\n3. c\n   - d\n\n*e* [l](u)\n\n    code\n",
        )
        .unwrap();
        let (s, l) = write(&d);
        assert_eq!(
            s,
            "T\n\n> q1\n>\n> q2\n\n- a\n  - b\n\n3. c\n   - d\n\ne l (u)\n\n    code\n"
        );
        assert_eq!(l, BTreeSet::from([Loss::Emphasis, Loss::Links]));
    }
}
