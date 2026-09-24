//! Markdown style runs for the styled source editor: a thin mirror of
//! `krabink_core::markdown` so every platform styles the same source the
//! same way.

use krabink_core as pcore;

/// What a run of the source means. See `krabink_core::StyleKind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum StyleKind {
    /// `level` 1..=6; the run covers the whole heading line(s).
    Heading {
        level: u8,
    },
    Strong,
    Emphasis,
    Strikethrough,
    /// Inline code, backticks included.
    CodeSpan,
    /// A fenced or indented block, fences included.
    CodeBlock,
    /// A list item; `depth` 1 for a top-level list.
    ListItem {
        depth: u8,
        ordered: bool,
    },
    BlockQuote,
    /// A link or image, brackets and destination included.
    Link,
    /// Syntax that is not content: `#`, `*`, `-`, `1.`, `>`, backticks,
    /// fences, `[`, `](url)`, `[ ]`. Always inside some other run.
    Marker,
    ThematicBreak,
}

impl From<pcore::StyleKind> for StyleKind {
    fn from(k: pcore::StyleKind) -> Self {
        match k {
            pcore::StyleKind::Heading { level } => Self::Heading { level },
            pcore::StyleKind::Strong => Self::Strong,
            pcore::StyleKind::Emphasis => Self::Emphasis,
            pcore::StyleKind::Strikethrough => Self::Strikethrough,
            pcore::StyleKind::CodeSpan => Self::CodeSpan,
            pcore::StyleKind::CodeBlock => Self::CodeBlock,
            pcore::StyleKind::ListItem { depth, ordered } => Self::ListItem { depth, ordered },
            pcore::StyleKind::BlockQuote => Self::BlockQuote,
            pcore::StyleKind::Link => Self::Link,
            pcore::StyleKind::Marker => Self::Marker,
            pcore::StyleKind::ThematicBreak => Self::ThematicBreak,
        }
    }
}

/// A styled span `[start, end)` of the source, in unicode scalars (the
/// unit `NoteSession::apply_text_edit` and anchors use).
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Record)]
pub struct StyleRun {
    pub start: u64,
    pub end: u64,
    pub kind: StyleKind,
}

impl From<pcore::StyleRun> for StyleRun {
    fn from(r: pcore::StyleRun) -> Self {
        Self {
            start: r.start as u64,
            end: r.end as u64,
            kind: r.kind.into(),
        }
    }
}

/// Style runs of `text`, sorted by start, outer before inner. Runs nest;
/// block kinds cover whole source lines; `Marker` runs sit inside another
/// run. Empty or plain text yields nothing.
#[uniffi::export]
pub fn style_runs(text: String) -> Vec<StyleRun> {
    pcore::style_runs(&text)
        .into_iter()
        .map(Into::into)
        .collect()
}
