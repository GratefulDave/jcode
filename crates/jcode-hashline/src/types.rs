//! Pure data types for the parser, applier, and patcher.

use std::collections::HashMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Anchor {
    pub line: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cursor {
    Bof,
    Eof,
    BeforeAnchor { anchor: Anchor },
    AfterAnchor { anchor: Anchor },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedRange {
    pub start: Anchor,
    pub end: Anchor,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PasteTarget {
    Gap { cursor: Cursor },
    Span { range: ParsedRange },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileOp {
    Rem,
    Move { dest: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockMode {
    Replace,
    InsertAfter,
    Cut,
    PasteAfter,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Edit {
    Insert {
        cursor: Cursor,
        text: String,
        line_num: usize,
        index: usize,
        replacement: bool,
        block_start: Option<usize>,
    },
    Delete {
        anchor: Anchor,
        line_num: usize,
        index: usize,
    },
    Cut {
        range: ParsedRange,
        register: Option<String>,
        line_num: usize,
        index: usize,
    },
    Paste {
        at: PasteTarget,
        register: Option<String>,
        line_num: usize,
        index: usize,
        block_start: Option<usize>,
    },
    Block {
        anchor: Anchor,
        payloads: Vec<String>,
        mode: BlockMode,
        register: Option<String>,
        line_num: usize,
        index: usize,
    },
}

impl Edit {
    pub fn line_num(&self) -> usize {
        match self {
            Self::Insert { line_num, .. }
            | Self::Delete { line_num, .. }
            | Self::Cut { line_num, .. }
            | Self::Paste { line_num, .. }
            | Self::Block { line_num, .. } => *line_num,
        }
    }

    pub fn anchors(&self) -> Vec<Anchor> {
        match self {
            Self::Insert { cursor, .. } => match cursor {
                Cursor::BeforeAnchor { anchor } | Cursor::AfterAnchor { anchor } => vec![*anchor],
                Cursor::Bof | Cursor::Eof => vec![],
            },
            Self::Delete { anchor, .. } => vec![*anchor],
            Self::Cut { range, .. } => vec![range.start, range.end],
            Self::Paste { at, .. } => match at {
                PasteTarget::Gap { cursor } => match cursor {
                    Cursor::BeforeAnchor { anchor } | Cursor::AfterAnchor { anchor } => {
                        vec![*anchor]
                    }
                    Cursor::Bof | Cursor::Eof => vec![],
                },
                PasteTarget::Span { range } => vec![range.start, range.end],
            },
            Self::Block { anchor, .. } => vec![*anchor],
        }
    }

    pub fn is_anchor_scoped(&self) -> bool {
        !self.anchors().is_empty()
    }
}

#[derive(Debug, Clone, Default)]
pub struct Clipboard {
    pub lines: Option<Vec<String>>,
    pub named: HashMap<String, Vec<String>>,
    pub pending_anon_cuts: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlockSpan {
    pub start: usize,
    pub end: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockResolution {
    pub line: usize,
    pub start: usize,
    pub end: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ApplyResult {
    pub text: String,
    pub first_changed_line: Option<usize>,
    pub warnings: Vec<String>,
    pub block_resolutions: Vec<BlockResolution>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    pub path: String,
    pub text: String,
    pub hash: String,
    pub seen_lines: Option<std::collections::BTreeSet<usize>>,
}

#[derive(Debug, Clone)]
pub struct PatchSection {
    pub path: String,
    pub file_hash: Option<String>,
    pub body: String,
}

#[derive(Debug, Clone)]
pub struct Patch {
    pub sections: Vec<PatchSection>,
}

pub const MAX_RANGE_LINES: usize = 100_000;
pub const SNAPSHOT_MAX_BYTES: usize = 4 * 1024 * 1024;
pub const DEFAULT_MAX_PATHS: usize = 256;
pub const DEFAULT_MAX_VERSIONS_PER_PATH: usize = 4;
pub const SEEN_LINE_REVEAL_CAP: usize = 40;
pub const SEEN_LINE_REVEAL_MAX_COLUMNS: usize = 512;
