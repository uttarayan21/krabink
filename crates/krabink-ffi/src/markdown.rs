//! Markdown style runs for the styled source editor: a thin mirror of
//! `krabink_core::markdown` so every platform styles the same source the
//! same way.

use krabink_core as pcore;

/// What a run of the source means. See `krabink_core::StyleKind`.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Enum)]
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
    /// An inline sketch embed: the whole `![…](krabink://sketch/<id>)`
    /// span, alone on its line, first occurrence of `sketch`. Never hosts
    /// `Marker` runs; hide the text and lay out a box of
    /// `NoteSession::sketch_box_height` in its place.
    SketchEmbed {
        sketch: String,
    },
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
            pcore::StyleKind::SketchEmbed { sketch } => Self::SketchEmbed {
                sketch: sketch.to_string(),
            },
        }
    }
}

/// A styled span `[start, end)` of the source, in unicode scalars (the
/// unit `NoteSession::apply_text_edit` and anchors use).
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
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

/// The reading view of a source text. See `krabink_core::PreviewText`.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct PreviewText {
    /// The source with markers hidden, bullets substituted and fence lines
    /// dropped.
    pub text: String,
    /// For each display char (unicode scalar) the source char it came
    /// from, plus one entry for the end mapping to the source length.
    pub source_of: Vec<u64>,
    /// Style runs in display coordinates.
    pub runs: Vec<StyleRun>,
}

/// The reading view of `text`: markers hidden, every display char mapped
/// back to its source char so anchors keep resolving.
#[uniffi::export]
pub fn preview_text(text: String) -> PreviewText {
    let p = pcore::preview_text(&text);
    PreviewText {
        text: p.text,
        source_of: p.source_of.into_iter().map(|s| s as u64).collect(),
        runs: p.runs.into_iter().map(Into::into).collect(),
    }
}

/// Inset from an inline sketch box's top-left corner to the sketch's
/// local origin, in points. Shared by every platform so ink lines up.
#[uniffi::export]
pub fn inline_padding() -> f32 {
    pcore::INLINE_PADDING
}

/// Height of an inline sketch box with no (or only shallow) ink, in points.
#[uniffi::export]
pub fn inline_min_height() -> f32 {
    pcore::INLINE_MIN_HEIGHT
}
