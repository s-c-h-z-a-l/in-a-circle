//! Shared document model of the text family: one reader and one writer per
//! format, all going through this tree.

pub mod html;
pub mod md;
pub mod txt;

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
        items: Vec<Vec<Block>>,
    },
    ThematicBreak,
    HtmlBlock(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Inline {
    Text(String),
    Emph(Vec<Inline>),
    Strong(Vec<Inline>),
    Code(String),
    Link {
        dest: String,
        title: String,
        content: Vec<Inline>,
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
            Loss::BlankLines => "repeated blank lines merged",
            Loss::Whitespace => "whitespace normalized",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadError {
    NotUtf8,
}

impl std::fmt::Display for ReadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ReadError::NotUtf8 => f.write_str("input is not valid UTF-8"),
        }
    }
}

impl std::error::Error for ReadError {}
