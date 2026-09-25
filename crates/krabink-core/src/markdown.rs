//! Markdown style runs for the editors: which spans of the source are a
//! heading, emphasis, code, a list item, or syntax markers. Both editors
//! keep the source text as is (view text == CRDT text) and style it in
//! place, so this is the one parse they share. [`preview_text`] derives
//! the reading view from the same runs: the source with its syntax hidden
//! (bullets substituted), plus the map from display chars back to source
//! chars, so ink anchored to source lines lands on the same lines there.
//!
//! Offsets are unicode scalars, `[start, end)`, matching
//! [`crate::NoteDoc::splice_text`].

use std::collections::HashSet;
use std::ops::Range;

use pulldown_cmark::{Event, HeadingLevel, Options, Parser, Tag, TagEnd};

use crate::SketchId;
use crate::export::SKETCH_URI_PREFIX;

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
    /// An inline sketch's embed line, `![…](krabink://sketch/<id>)`: the
    /// image alone on its line, first occurrence of that id. The run
    /// covers the whole image syntax and never hosts markers; the editors
    /// hide the text and lay the line out as the sketch's box.
    SketchEmbed {
        sketch: SketchId,
    },
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
    let mut embedded: HashSet<SketchId> = HashSet::new();

    for (event, range) in Parser::new_ext(text, options).into_offset_iter() {
        match event {
            Event::Start(tag) => {
                let embed = match &tag {
                    Tag::Image { dest_url, .. } => embed_id(dest_url, text, &range, &mut embedded),
                    _ => None,
                };
                let kind = match tag {
                    Tag::Image { .. } if embed.is_some() => {
                        // The whole syntax is the box: no markers inside.
                        covered.push(range.clone());
                        embed.map(|sketch| StyleKind::SketchEmbed { sketch })
                    }
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

/// The sketch an image at byte `range` embeds: its destination is a
/// `krabink://sketch/` URI with a ULID, the image is the only non-blank
/// content of its source line, and the id was not embedded before.
fn embed_id(
    dest: &str,
    text: &str,
    range: &Range<usize>,
    embedded: &mut HashSet<SketchId>,
) -> Option<SketchId> {
    let id: SketchId = dest.strip_prefix(SKETCH_URI_PREFIX)?.parse().ok()?;
    let line_start = text[..range.start].rfind('\n').map_or(0, |i| i + 1);
    let line_end = text[range.end..]
        .find('\n')
        .map_or(text.len(), |i| range.end + i);
    let alone = text[line_start..range.start].trim().is_empty()
        && text[range.end..line_end].trim().is_empty();
    (alone && embedded.insert(id)).then_some(id)
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

/// The reading view of a source text: markers hidden, list bullets and
/// task boxes substituted, lines that were only syntax (code fences)
/// dropped. Every display char knows the source char it came from, so
/// anchors resolved against the source map onto display lines.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PreviewText {
    pub text: String,
    /// For each display char, the source char (unicode scalar index) it
    /// came from; one more entry for the end of the text, mapping to the
    /// source length. Non-decreasing.
    pub source_of: Vec<usize>,
    /// [`style_runs`] of the source, remapped to display coordinates
    /// (still sorted, outer before inner; runs that vanished are gone).
    pub runs: Vec<StyleRun>,
}

impl PreviewText {
    /// Display index of the first display char at or after source char
    /// `source` (the display length when nothing follows). A char in a
    /// dropped line maps to the start of the next displayed line.
    pub fn display_of(&self, source: usize) -> usize {
        self.source_of.partition_point(|&s| s < source)
    }
}

/// What the preview does with one source char.
#[derive(Clone, Copy)]
enum Show {
    Keep,
    Drop,
    /// Replace the marker this char starts with.
    Subst(&'static str),
}

/// See [`PreviewText`].
pub fn preview_text(text: &str) -> PreviewText {
    let runs = style_runs(text);
    let chars: Vec<char> = text.chars().collect();
    let mut show = vec![Show::Keep; chars.len()];
    // The last substituted bullet: its start, and the index its item text
    // starts at (past the space).
    let mut last_bullet: Option<(usize, usize)> = None;

    for (i, run) in runs.iter().enumerate() {
        if run.kind != StyleKind::Marker {
            continue;
        }
        // The innermost run this marker belongs to: the last earlier
        // non-marker run that encloses it (sorted outer before inner).
        let host = runs[..i]
            .iter()
            .rev()
            .find(|r| r.kind != StyleKind::Marker && r.start <= run.start && r.end >= run.end)
            .map(|r| r.kind);
        let src: String = chars[run.start..run.end].iter().collect();
        let action = match host {
            Some(StyleKind::ListItem { depth, ordered }) => match src.as_str() {
                "-" | "*" | "+" if !ordered => Show::Subst(if depth <= 1 { "•" } else { "◦" }),
                "[ ]" => Show::Subst("☐"),
                "[x]" | "[X]" => Show::Subst("☑"),
                _ if ordered && is_ordinal(&src) => Show::Keep,
                _ => Show::Drop,
            },
            Some(StyleKind::ThematicBreak) => Show::Keep,
            _ => Show::Drop,
        };
        if matches!(action, Show::Keep) {
            continue;
        }
        // A task box replaces the bullet before it.
        if matches!(action, Show::Subst("☐" | "☑"))
            && let Some((bullet, gap)) = last_bullet.take()
            && gap == run.start
        {
            for slot in &mut show[bullet..run.start] {
                *slot = Show::Drop;
            }
        }
        show[run.start] = action;
        for slot in &mut show[run.start + 1..run.end] {
            *slot = Show::Drop;
        }
        // Block syntax (`#`, `>`, a dropped bullet) takes the space after
        // it along, so lines do not start with a stray blank.
        let at_line_start = chars[..run.start]
            .iter()
            .rev()
            .take_while(|&&c| c != '\n')
            .enumerate()
            .all(|(back, c)| {
                c.is_whitespace() || !matches!(show[run.start - 1 - back], Show::Keep)
            });
        if at_line_start && matches!(action, Show::Drop) {
            let mut j = run.end;
            while j < chars.len() && chars[j] == ' ' {
                show[j] = Show::Drop;
                j += 1;
            }
        }
        if let Show::Subst("•" | "◦") = action {
            // Remember the bullet and where the text after its space starts.
            let mut gap = run.end;
            while gap < chars.len() && chars[gap] == ' ' {
                gap += 1;
            }
            last_bullet = Some((run.start, gap));
        }
    }

    let mut out = String::new();
    let mut source_of: Vec<usize> = Vec::new();
    // Display length (chars) just before each source char is handled.
    let mut display_start = vec![0usize; chars.len() + 1];
    // The current source line: where its display started, whether the
    // source had any content.
    let mut line_start = 0usize;
    let mut line_content = false;
    let push = |out: &mut String, source_of: &mut Vec<usize>, ch: char, from: usize| {
        out.push(ch);
        source_of.push(from);
    };
    for (i, &ch) in chars.iter().enumerate() {
        display_start[i] = source_of.len();
        if ch == '\n' {
            if line_content && out[byte_at(&out, line_start)..].trim().is_empty() {
                // Only syntax on this line (a code fence): drop the line.
                out.truncate(byte_at(&out, line_start));
                source_of.truncate(line_start);
            } else {
                push(&mut out, &mut source_of, '\n', i);
            }
            line_start = source_of.len();
            line_content = false;
            continue;
        }
        line_content |= !ch.is_whitespace();
        match show[i] {
            Show::Keep => push(&mut out, &mut source_of, ch, i),
            Show::Drop => {}
            Show::Subst(s) => {
                for sub in s.chars() {
                    push(&mut out, &mut source_of, sub, i);
                }
            }
        }
    }
    if line_content && out[byte_at(&out, line_start)..].trim().is_empty() {
        out.truncate(byte_at(&out, line_start));
        source_of.truncate(line_start);
    }
    display_start[chars.len()] = source_of.len();
    // Chars of a dropped line were counted before the drop: clamp so the
    // table is non-decreasing and within the display.
    for i in (0..chars.len()).rev() {
        display_start[i] = display_start[i].min(display_start[i + 1]);
    }
    source_of.push(chars.len());

    let runs = runs
        .into_iter()
        .map(|r| StyleRun {
            start: display_start[r.start],
            end: display_start[r.end],
            kind: r.kind,
        })
        .filter(|r| r.start < r.end)
        .collect();
    PreviewText {
        text: out,
        source_of,
        runs,
    }
}

/// `1.` / `12)`: an ordered-list marker.
fn is_ordinal(marker: &str) -> bool {
    let digits = marker.trim_end_matches(['.', ')']);
    digits.len() + 1 == marker.len()
        && !digits.is_empty()
        && digits.chars().all(|c| c.is_ascii_digit())
}

/// Byte offset of char `index` in `s` (`s.len()` past the end).
fn byte_at(s: &str, index: usize) -> usize {
    s.char_indices().nth(index).map_or(s.len(), |(b, _)| b)
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
            "a\n\n![sketch](krabink://sketch/01ARZ3NDEKTSV4RRFFQ69G5FAV)\n\n- b",
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

    fn preview(text: &str) -> PreviewText {
        preview_text(text)
    }

    const EMBED: &str = "![sketch](krabink://sketch/01ARZ3NDEKTSV4RRFFQ69G5FAV)";

    fn embed_kind() -> StyleKind {
        StyleKind::SketchEmbed {
            sketch: "01ARZ3NDEKTSV4RRFFQ69G5FAV".parse().unwrap(),
        }
    }

    #[test]
    fn sketch_embed_is_one_run_without_markers() {
        let text = format!("a\n\n{EMBED}\n\nb");
        let embed = (3, 3 + EMBED.chars().count(), embed_kind());
        assert_eq!(runs(&text), vec![embed]);
        assert!(markers(&text).is_empty());
        // Indented or trailing blanks still count as alone on the line.
        let text = format!("  {EMBED}  \n");
        assert!(
            runs(&text).iter().any(|r| r.2 == embed_kind()),
            "{:?}",
            runs(&text)
        );
    }

    #[test]
    fn sketch_embed_needs_own_line_and_first_occurrence() {
        let text = format!("see {EMBED}");
        assert!(runs(&text).iter().all(|r| r.2 != embed_kind()));
        assert_eq!(
            markers(&text),
            vec!["![", "](krabink://sketch/01ARZ3NDEKTSV4RRFFQ69G5FAV)"]
        );
        let text = format!("{EMBED}\n\n{EMBED}\n");
        let embeds: Vec<_> = runs(&text)
            .into_iter()
            .filter(|r| r.2 == embed_kind())
            .collect();
        assert_eq!(embeds.len(), 1);
        assert_eq!(embeds[0].0, 0);
        let links: Vec<_> = runs(&text)
            .into_iter()
            .filter(|r| r.2 == StyleKind::Link)
            .collect();
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].0, EMBED.chars().count() + 2);
    }

    #[test]
    fn bad_sketch_uri_is_a_link() {
        for text in [
            "![s](krabink://sketch/short)",
            "![s](krabink://sketch/01ARZ3NDEKTSV4RRFFQ69G5FAVx)",
            "![s](krabink://note/01ARZ3NDEKTSV4RRFFQ69G5FAV)",
            "[s](krabink://sketch/01ARZ3NDEKTSV4RRFFQ69G5FAV)",
        ] {
            let kinds: Vec<_> = runs(text).into_iter().map(|r| r.2).collect();
            assert!(kinds.contains(&StyleKind::Link), "{text}: {kinds:?}");
            assert!(
                !kinds
                    .iter()
                    .any(|k| matches!(k, StyleKind::SketchEmbed { .. })),
                "{text}: {kinds:?}"
            );
        }
    }

    #[test]
    fn preview_keeps_embed_line_and_run() {
        let text = format!("# T\n{EMBED}\nafter\n");
        let p = preview(&text);
        assert_eq!(p.text, format!("T\n{EMBED}\nafter\n"));
        let embed = p.runs.iter().find(|r| r.kind == embed_kind()).unwrap();
        assert_eq!(embed.start, 2);
        assert_eq!(embed.end, 2 + EMBED.chars().count());
        // The line start maps back to the source `!`.
        assert_eq!(p.source_of[embed.start], 4);
        assert_eq!(p.display_of(4), 2);
    }

    #[test]
    fn preview_hides_markers_and_substitutes_bullets() {
        let p = preview(
            "# Title\n\nSome *em* and **st** `c` [l](u).\n\n- a\n  - b\n\n1. one\n\n- [ ] t\n- [x] d\n",
        );
        assert_eq!(
            p.text,
            "Title\n\nSome em and st c l.\n\n• a\n  ◦ b\n\n1. one\n\n☐ t\n☑ d\n"
        );
    }

    #[test]
    fn preview_drops_fence_lines_and_keeps_rules() {
        let p = preview("x\n```rust\nlet a = 1;\n```\n\n---\n");
        assert_eq!(p.text, "x\nlet a = 1;\n\n---\n");
        // Source char → display: the fence line maps to the code line.
        let fence = 2;
        let code = "x\n```rust\n".chars().count();
        assert_eq!(p.display_of(fence), 2);
        assert_eq!(p.display_of(code), 2);
        assert_eq!(p.source_of[2], code);
        assert_eq!(
            *p.source_of.last().unwrap(),
            "x\n```rust\nlet a = 1;\n```\n\n---\n".chars().count()
        );
        let code_run = p
            .runs
            .iter()
            .find(|r| r.kind == StyleKind::CodeBlock)
            .unwrap();
        assert_eq!(
            p.text
                .chars()
                .skip(code_run.start)
                .take(code_run.end - code_run.start)
                .collect::<String>(),
            "let a = 1;\n"
        );
    }

    #[test]
    fn preview_runs_are_remapped_and_sorted() {
        let p = preview("## H *e*\n\n> q **b**\n");
        let display_runs: Vec<(String, StyleKind)> = p
            .runs
            .iter()
            .map(|r| {
                (
                    p.text.chars().skip(r.start).take(r.end - r.start).collect(),
                    r.kind,
                )
            })
            .collect();
        assert!(
            display_runs
                .iter()
                .any(|(t, k)| t.trim_end() == "H e" && *k == StyleKind::Heading { level: 2 }),
            "{display_runs:?}"
        );
        assert!(display_runs.contains(&("e".into(), StyleKind::Emphasis)));
        assert!(display_runs.contains(&("b".into(), StyleKind::Strong)));
        assert!(
            display_runs
                .iter()
                .any(|(t, k)| t.trim_end() == "q b" && *k == StyleKind::BlockQuote),
            "{display_runs:?}"
        );
        // Markers only survive as substitutions (bullets, boxes), never as
        // syntax text.
        assert!(
            display_runs.iter().all(|(_, k)| *k != StyleKind::Marker),
            "{display_runs:?}"
        );
        assert!(p.runs.windows(2).all(|w| w[0].start <= w[1].start));
        assert_eq!(p.source_of.len(), p.text.chars().count() + 1);
    }

    #[test]
    fn preview_of_plain_text_is_identity() {
        let p = preview("plain\nlines\n");
        assert_eq!(p.text, "plain\nlines\n");
        assert_eq!(p.source_of, (0..=12).collect::<Vec<_>>());
        assert!(p.runs.is_empty());
    }
}
