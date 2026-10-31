//! Shared document model of the text family: one reader and one writer per
//! format, all going through this tree.

mod budget;
pub mod html;
pub mod md;
pub mod txt;

/// The Markdown syntax a reader accepts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Dialect {
    /// CommonMark 0.31.2 only; what `md::read` has always read.
    #[default]
    CommonMark,
    /// CommonMark plus the GitHub extensions: tables, strikethrough, task
    /// list items and extended autolinks.
    Gfm,
}

/// Column alignment of a table, from its delimiter row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Align {
    None,
    Left,
    Center,
    Right,
}

/// A list item: its blocks, and the task list state when the item starts
/// with `[ ]` or `[x]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListItem {
    /// `None`: not a task item; `Some(done)` otherwise.
    pub checked: Option<bool>,
    pub blocks: Vec<Block>,
}

/// Plain (non-task) items from block lists, for test fixtures.
#[cfg(test)]
pub(crate) fn plain_items(items: Vec<Vec<Block>>) -> Vec<ListItem> {
    items
        .into_iter()
        .map(|blocks| ListItem {
            checked: None,
            blocks,
        })
        .collect()
}

/// A parsed document: a sequence of blocks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Document {
    pub blocks: Vec<Block>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Block {
    Paragraph(Vec<Inline>),
    /// `level` is 1..=6.
    Heading {
        level: u8,
        content: Vec<Inline>,
    },
    CodeBlock {
        info: String,
        text: String,
    },
    BlockQuote(Vec<Block>),
    /// `start == None` is a bullet list, `Some(n)` an ordered list from `n`.
    List {
        start: Option<u64>,
        tight: bool,
        items: Vec<ListItem>,
    },
    /// A GFM table. `align` has one entry per header cell; every row of
    /// `rows` has exactly that many cells too.
    Table {
        align: Vec<Align>,
        head: Vec<Vec<Inline>>,
        rows: Vec<Vec<Vec<Inline>>>,
    },
    ThematicBreak,
    HtmlBlock(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Inline {
    Text(String),
    Emph(Vec<Inline>),
    Strong(Vec<Inline>),
    /// `~~text~~` (GFM).
    Strikethrough(Vec<Inline>),
    Code(String),
    Link {
        dest: String,
        title: String,
        content: Vec<Inline>,
        /// `true` only for a GFM extended autolink (`www.x.y`, `http://x.y`,
        /// a bare email), whose text is its destination as written.
        auto: bool,
    },
    Image {
        dest: String,
        title: String,
        alt: Vec<Inline>,
    },
    SoftBreak,
    HardBreak,
    Html(String),
}

/// What a conversion could not carry over.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, PartialOrd, Ord)]
pub enum Loss {
    Emphasis,
    InlineCode,
    Links,
    Images,
    Html,
    HeadingLevels,
    BlankLines,
    Whitespace,
    Strikethrough,
    TableLayout,
    TableWidth,
}

impl Loss {
    /// One-sentence English description, suitable for a notification.
    pub fn message(self) -> &'static str {
        match self {
            Loss::Emphasis => "emphasis removed",
            Loss::InlineCode => "inline code formatting removed",
            Loss::Links => "links flattened to text",
            Loss::Images => "images replaced by their alt text",
            Loss::Html => "HTML dropped",
            Loss::HeadingLevels => "heading levels lost",
            Loss::BlankLines => "blank lines merged or dropped",
            Loss::Whitespace => "runs of spaces or tabs may collapse when rendered",
            Loss::Strikethrough => "strikethrough removed",
            Loss::TableLayout => "table too wide; rows listed one by one",
            Loss::TableWidth => "table columns may be misaligned by wide or combining characters",
        }
    }
}

/// Structural budgets of one conversion. The readers and writers charge
/// them WHILE building, before the allocation they pay for, so a hostile
/// input stops at the budget instead of after the memory is spent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// Input bytes accepted (`document_input_bytes`). Checked before any
    /// work on the input.
    pub input_bytes: usize,
    /// Block and inline nodes a document may be built from
    /// (`document_nodes`), counted as they are created, including link
    /// reference definition paragraphs and inline arena nodes that a later
    /// flattening drops.
    pub nodes: usize,
    /// Text bytes the reader owns (`document_owned_text_bytes`): line and
    /// block content, inline strings, link destinations and titles copied
    /// per use of a reference, reference definitions, and the copy that
    /// replaces NUL by U+FFFD (3 bytes per NUL).
    pub text_bytes: usize,
    /// Bytes a writer may produce (`document_output_bytes`), counted for
    /// every piece (including a nested block's intermediate text) before
    /// it is appended.
    pub output_bytes: usize,
}

impl Limits {
    /// The shipped budgets (`document_input_bytes`,
    /// `document_nodes`, `document_owned_text_bytes`,
    /// `document_output_bytes`).
    pub const DEFAULT: Limits = Limits {
        // 8 MiB: 12.9x the largest real text measured (pcre2.txt, 652 KB).
        input_bytes: 8 * 1024 * 1024,
        // 2^19 (raised from 2^17 after measuring real inputs): the densest real
        // text measured is 15.9 B/node (oui.txt), so 2^17 nodes ended at
        // ~2 MiB of input; 2^19 reaches ~8 MiB at 16 B/node for ~100-200 MB
        // peak, a fifth of the sandbox's >= 1 GiB RLIMIT_DATA net.
        nodes: 1 << 19,
        // 32 MiB: 2.8x the worst real owned/input ratio times the input
        // limit; link reference expansion stops here.
        text_bytes: 32 * 1024 * 1024,
        // 32 MiB: 4x the input limit. Measured worst case of escape
        // expansion is 4.81x (every leading space of a line becomes
        // `&#32;`, 1 -> 5 bytes): such an input of 8 MiB fails cleanly on
        // this budget (`Exceeded("output")`, from about 6.6 MiB of input);
        // realistic text expands far less.
        output_bytes: 32 * 1024 * 1024,
    };

    /// No budget: the compatibility entry points `read` and `write`. Only
    /// an allocation failure can stop a conversion under it.
    pub const UNLIMITED: Limits = Limits {
        input_bytes: usize::MAX,
        nodes: usize::MAX,
        text_bytes: usize::MAX,
        output_bytes: usize::MAX,
    };
}

/// A budget of [`Limits`] ran out, or memory for the next piece could not
/// be reserved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LimitError {
    /// Names the budget: `"input"`, `"nodes"`, `"owned text"` or `"output"`.
    Exceeded(&'static str),
    /// The allocator refused a (small, budgeted) reservation. Best effort:
    /// only the reservations that go through the budget can report it
    /// (the text and output buffers). The smaller ones (headers, delimiter
    /// and bracket stacks, unescaping, code-span copies) are bounded by the
    /// same budgets but are plain allocations; the sandbox's `RLIMIT_DATA`
    /// is the net under them.
    Allocation,
}

impl std::fmt::Display for LimitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LimitError::Exceeded(what) => write!(f, "document {what} limit exceeded"),
            LimitError::Allocation => f.write_str("out of memory while converting"),
        }
    }
}

impl std::error::Error for LimitError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadError {
    NotText,
    Limit(LimitError),
}

impl From<LimitError> for ReadError {
    fn from(e: LimitError) -> Self {
        ReadError::Limit(e)
    }
}

impl std::fmt::Display for ReadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ReadError::NotText => f.write_str("not text (invalid UTF-8 or a NUL byte)"),
            ReadError::Limit(e) => e.fmt(f),
        }
    }
}

impl std::error::Error for ReadError {}
