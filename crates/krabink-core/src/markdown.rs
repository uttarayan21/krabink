//! Markdown style runs for the editors: which spans of the source are a
//! heading, emphasis, code, a list item, or syntax markers. Both editors
//! keep the source text as is (view text == CRDT text) and style it in
//! place, so this is the one parse they share.
//!
//! Offsets are unicode scalars, `[start, end)`, matching
//! [`crate::NoteDoc::splice_text`].

use std::ops::Range;

use pulldown_cmark::{Event, HeadingLevel, Options, Parser, Tag, TagEnd};

/// What a run of source text is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
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

/// A styled span of the source, in unicode scalars.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct StyleRun {
    pub start: usize,
    pub end: usize,
    pub kind: StyleKind,
}

/// Style runs of `text`, sorted by start, outer before inner; runs nest and
/// never partially overlap; `Marker` runs never overlap each other.
pub fn style_runs(text: &str) -> Vec<StyleRun> {
    let chars = CharTable::new(text);
    let options = Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS;
    let mut runs: Vec<(Range<usize>, StyleKind)> = Vec::new();
    // Byte ranges that are content (or markers already emitted); the gaps
    // between them are syntax.
    let mut covered: Vec<Range<usize>> = Vec::new();
    let mut lists: Vec<bool> = Vec::new();

    for (event, range) in Parser::new_ext(text, options).into_offset_iter() {
        match event {
            Event::Start(tag) => {
                let kind = match tag {
                    Tag::Heading { level, .. } => Some(StyleKind::Heading {
                        level: heading_level(level),
                    }),
                    Tag::Strong => Some(StyleKind::Strong),
                    Tag::Emphasis => Some(StyleKind::Emphasis),
                    Tag::Strikethrough => Some(StyleKind::Strikethrough),
                    Tag::CodeBlock(_) => Some(StyleKind::CodeBlock),
                    Tag::BlockQuote(_) => Some(StyleKind::BlockQuote),
                    Tag::Link { .. } | Tag::Image { .. } => Some(StyleKind::Link),
                    Tag::List(first) => {
                        lists.push(first.is_some());
                        None
                    }
                    Tag::Item => Some(StyleKind::ListItem {
                        depth: u8::try_from(lists.len()).unwrap_or(u8::MAX),
                        ordered: lists.last().copied().unwrap_or(false),
                    }),
                    _ => None,
                };
                if let Some(kind) = kind {
                    runs.push((range, kind));
                }
            }
            Event::End(TagEnd::List(_)) => {
                lists.pop();
            }
            Event::End(_) => {}
            Event::Code(_) => {
                let src = &text[range.clone()];
                let open = src.len() - src.trim_start_matches('`').len();
                let close = src.len() - src.trim_end_matches('`').len();
                runs.push((range.clone(), StyleKind::CodeSpan));
                runs.push((range.start..range.start + open, StyleKind::Marker));
                runs.push((range.end - close..range.end, StyleKind::Marker));
                covered.push(range);
            }
            Event::Rule => {
                runs.push((range.clone(), StyleKind::ThematicBreak));
                covered.push(range);
            }
            Event::TaskListMarker(_) => {
                runs.push((range.clone(), StyleKind::Marker));
                covered.push(range);
            }
            Event::Text(_)
            | Event::Html(_)
            | Event::InlineHtml(_)
            | Event::InlineMath(_)
            | Event::DisplayMath(_)
            | Event::FootnoteReference(_)
            | Event::SoftBreak
            | Event::HardBreak => covered.push(range),
        }
    }

    // Everything not covered is syntax: split it on whitespace so blank
    // lines and indentation stay unstyled.
    covered.sort_by_key(|r| r.start);
    let mut cursor = 0;
    for range in covered
        .iter()
        .chain(std::iter::once(&(text.len()..text.len())))
    {
        if range.start > cursor {
            push_markers(text, cursor..range.start, &mut runs);
        }
        cursor = cursor.max(range.end);
    }

    let mut runs: Vec<StyleRun> = runs
        .into_iter()
        .filter(|(range, _)| range.start < range.end)
        .map(|(range, kind)| StyleRun {
            start: chars.index(range.start),
            end: chars.index(range.end),
            kind,
        })
        .collect();
    runs.sort_by(|a, b| {
        a.start
            .cmp(&b.start)
            .then(b.end.cmp(&a.end))
            .then((a.kind == StyleKind::Marker).cmp(&(b.kind == StyleKind::Marker)))
    });
    runs.dedup();
    runs
}

/// Non-whitespace stretches of `range` become `Marker` runs.
fn push_markers(text: &str, range: Range<usize>, runs: &mut Vec<(Range<usize>, StyleKind)>) {
    let slice = &text[range.clone()];
    let mut start: Option<usize> = None;
    for (offset, ch) in slice.char_indices() {
        match (ch.is_whitespace(), start) {
            (true, Some(s)) => {
                runs.push((range.start + s..range.start + offset, StyleKind::Marker));
                start = None;
            }
            (false, None) => start = Some(offset),
            _ => {}
        }
    }
    if let Some(s) = start {
        runs.push((range.start + s..range.end, StyleKind::Marker));
    }
}

fn heading_level(level: HeadingLevel) -> u8 {
    match level {
        HeadingLevel::H1 => 1,
        HeadingLevel::H2 => 2,
        HeadingLevel::H3 => 3,
        HeadingLevel::H4 => 4,
        HeadingLevel::H5 => 5,
        HeadingLevel::H6 => 6,
    }
}

/// Byte offset -> unicode-scalar index, for offsets on char boundaries.
struct CharTable {
    starts: Vec<usize>,
}

impl CharTable {
    fn new(text: &str) -> Self {
        Self {
            starts: text.char_indices().map(|(b, _)| b).collect(),
        }
    }

    fn index(&self, byte: usize) -> usize {
        self.starts.partition_point(|&b| b < byte)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn runs(text: &str) -> Vec<(usize, usize, StyleKind)> {
        style_runs(text)
            .into_iter()
            .map(|r| (r.start, r.end, r.kind))
            .collect()
    }

    fn markers(text: &str) -> Vec<&str> {
        let chars: Vec<char> = text.chars().collect();
        style_runs(text)
            .into_iter()
            .filter(|r| r.kind == StyleKind::Marker)
            .map(|r| chars[r.start..r.end].iter().collect::<String>())
            .map(|s| Box::leak(s.into_boxed_str()) as &str)
            .collect()
    }

    #[test]
    fn heading_levels_and_markers() {
        let text = "# One\n\n### Three\nbody\n";
        assert_eq!(
            runs(text),
            vec![
                (0, 6, StyleKind::Heading { level: 1 }),
                (0, 1, StyleKind::Marker),
                (7, 17, StyleKind::Heading { level: 3 }),
                (7, 10, StyleKind::Marker),
            ]
        );
    }

    #[test]
    fn strong_inside_heading_nests() {
        let text = "## a **b** c\n";
        assert_eq!(
            runs(text),
            vec![
                (0, 13, StyleKind::Heading { level: 2 }),
                (0, 2, StyleKind::Marker),
                (5, 10, StyleKind::Strong),
                (5, 7, StyleKind::Marker),
                (8, 10, StyleKind::Marker),
            ]
        );
    }

    #[test]
    fn emphasis_and_strikethrough() {
        let text = "*a* ~~b~~";
        assert_eq!(
            runs(text),
            vec![
                (0, 3, StyleKind::Emphasis),
                (0, 1, StyleKind::Marker),
                (2, 3, StyleKind::Marker),
                (4, 9, StyleKind::Strikethrough),
                (4, 6, StyleKind::Marker),
                (7, 9, StyleKind::Marker),
            ]
        );
    }

    #[test]
    fn code_span_backticks_are_markers() {
        assert_eq!(
            runs("x `a` y"),
            vec![
                (2, 5, StyleKind::CodeSpan),
                (2, 3, StyleKind::Marker),
                (4, 5, StyleKind::Marker),
            ]
        );
        assert_eq!(
            runs("`` a`b ``"),
            vec![
                (0, 9, StyleKind::CodeSpan),
                (0, 2, StyleKind::Marker),
                (7, 9, StyleKind::Marker),
            ]
        );
    }

    #[test]
    fn fenced_block_fences_are_markers_content_is_not() {
        let text = "```rust\nlet x = *y;\n```\n";
        assert_eq!(
            runs(text),
            vec![
                (0, 23, StyleKind::CodeBlock),
                (0, 7, StyleKind::Marker),
                (20, 23, StyleKind::Marker),
            ]
        );
        // Indented blocks: indentation is whitespace, so no markers.
        assert_eq!(runs("    code\n"), vec![(4, 9, StyleKind::CodeBlock)]);
    }

    #[test]
    fn nested_lists_report_depth_and_ordered() {
        let text = "- a\n  1. b\n- c\n";
        let items: Vec<_> = runs(text)
            .into_iter()
            .filter(|(_, _, k)| matches!(k, StyleKind::ListItem { .. }))
            .collect();
        assert_eq!(
            items,
            vec![
                (
                    0,
                    11,
                    StyleKind::ListItem {
                        depth: 1,
                        ordered: false
                    }
                ),
                (
                    6,
                    11,
                    StyleKind::ListItem {
                        depth: 2,
                        ordered: true
                    }
                ),
                (
                    11,
                    15,
                    StyleKind::ListItem {
                        depth: 1,
                        ordered: false
                    }
                ),
            ]
        );
        assert_eq!(markers(text), vec!["-", "1.", "-"]);
    }

    #[test]
    fn task_list_box_is_one_marker() {
        assert_eq!(
            markers("- [ ] todo\n- [x] done\n"),
            vec!["-", "[ ]", "-", "[x]"]
        );
    }

    #[test]
    fn blockquote_marks_every_line() {
        let text = "> a\n> b\n";
        let quote: Vec<_> = runs(text)
            .into_iter()
            .filter(|(_, _, k)| *k == StyleKind::BlockQuote)
            .collect();
        assert_eq!(quote, vec![(0, 8, StyleKind::BlockQuote)]);
        assert_eq!(markers(text), vec![">", ">"]);
    }

    #[test]
    fn link_brackets_and_url_are_markers() {
        let text = "see [it](http://x) now";
        assert_eq!(
            runs(text),
            vec![
                (4, 18, StyleKind::Link),
                (4, 5, StyleKind::Marker),
                (7, 18, StyleKind::Marker),
            ]
        );
        assert_eq!(markers("![alt](u)"), vec!["![", "](u)"]);
    }

    #[test]
    fn thematic_break() {
        assert_eq!(runs("a\n\n---\n"), vec![(3, 7, StyleKind::ThematicBreak)]);
    }

    #[test]
    fn offsets_are_unicode_scalars() {
        let text = "héllo 🦀\n# T\n";
        assert_eq!(
            runs(text),
            vec![
                (8, 12, StyleKind::Heading { level: 1 }),
                (8, 9, StyleKind::Marker),
            ]
        );
    }

    #[test]
    fn empty_and_plain_text_yield_nothing() {
        assert!(runs("").is_empty());
        assert!(runs("just words\n\nand more\n").is_empty());
    }

    #[test]
    fn runs_are_sorted_and_nested() {
        let corpus = [
            "# H *e* **s** `c`\n\n> q **b**\n\n- [ ] a `b`\n  - [x] [l](u)\n\n```\nx\n```\n\n---\n",
            "1. one\n2. two **b** *i*\n\n***\n\n![i](x) ~~s~~",
        ];
        for text in corpus {
            let runs = style_runs(text);
            for pair in runs.windows(2) {
                let (a, b) = (pair[0], pair[1]);
                assert!(a.start <= b.start, "{text:?}: {a:?} before {b:?}");
                // Either disjoint or nested, never partially overlapping.
                assert!(
                    b.start >= a.end || b.end <= a.end,
                    "{text:?}: {a:?} overlaps {b:?}"
                );
            }
            let markers: Vec<_> = runs
                .iter()
                .filter(|r| r.kind == StyleKind::Marker)
                .collect();
            for pair in markers.windows(2) {
                assert!(pair[1].start >= pair[0].end, "{text:?}: markers overlap");
            }
            for m in &markers {
                assert!(
                    runs.iter().any(|r| r.kind != StyleKind::Marker
                        && r.start <= m.start
                        && m.end <= r.end),
                    "{text:?}: marker {m:?} outside any run"
                );
            }
        }
    }
}
