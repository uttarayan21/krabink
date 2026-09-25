//! Markdown export: rewrites `krabink://sketch/<id>` image URIs to relative
//! SVG assets and renders each referenced sketch's elements to SVG.
//!
//! The markdown is rewritten by plain string substitution rather than a
//! parse-and-reserialize pass, so the exported source is byte-identical to
//! the note outside the rewritten URIs.
//!
//! The note's page ink layer ([`NoteDoc::page_elements`]) is not exported
//! yet: its elements have no layout-independent position, only a line
//! anchor. A follow-up can group them by resolved line and emit one SVG
//! after each paragraph.

use std::fmt::Write as _;

use crate::brush::{Blend, StrokeEnd, TipEvaluator};
use crate::element::Element;
use crate::geom::Ink;
use crate::inline::points_bounds;
use crate::note::NoteDoc;
use crate::stroke::StrokePoint;
use crate::{Result, SketchId};

pub const SKETCH_URI_PREFIX: &str = "krabink://sketch/";
const ULID_LEN: usize = 26;
const SVG_PAD: f32 = 8.0;

/// One exported sketch image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportAsset {
    /// File name relative to the assets directory, e.g. `01ABC....svg`.
    pub file_name: String,
    pub svg: String,
}

/// A note ready to write to disk: `markdown` next to an `assets/` directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportBundle {
    pub markdown: String,
    pub assets: Vec<ExportAsset>,
}

impl NoteDoc {
    /// Render this note for consumption outside krabink.
    pub fn export(&self) -> Result<ExportBundle> {
        let source = self.text();
        let mut markdown = String::with_capacity(source.len());
        let mut assets = Vec::new();
        let mut rest = source.as_str();

        while let Some(at) = rest.find(SKETCH_URI_PREFIX) {
            let (before, from_uri) = rest.split_at(at);
            markdown.push_str(before);
            let after_prefix = &from_uri[SKETCH_URI_PREFIX.len()..];
            let id: Option<SketchId> = after_prefix
                .get(..ULID_LEN)
                .and_then(|raw| raw.parse().ok());
            match id {
                Some(id) if self.sketch_ids().contains(&id) => {
                    let file_name = format!("{id}.svg");
                    write!(markdown, "assets/{file_name}").expect("string write");
                    if !assets
                        .iter()
                        .any(|a: &ExportAsset| a.file_name == file_name)
                    {
                        assets.push(ExportAsset {
                            file_name,
                            svg: elements_to_svg(&self.elements(id)?),
                        });
                    }
                    rest = &after_prefix[ULID_LEN..];
                }
                _ => {
                    // Unknown sketch: keep the URI untouched.
                    markdown.push_str(SKETCH_URI_PREFIX);
                    rest = after_prefix;
                }
            }
        }
        markdown.push_str(rest);

        Ok(ExportBundle { markdown, assets })
    }
}

/// Render elements to a standalone SVG document.
///
/// Each element's ink becomes one closed outline polygon filled with the
/// nonzero rule, so a translucent stroke never darkens where it overlaps
/// itself (the same look [`crate::Overlap::Discard`] gives on screen).
/// Per-point opacity is averaged into the fill; grain and soft edges are
/// not represented.
pub fn elements_to_svg(elements: &[Element]) -> String {
    let outlines: Vec<(&Element, Vec<[f32; 2]>, f32)> = elements
        .iter()
        .map(|el| {
            let points = el.outline();
            let ink = el.ink();
            let opacity = mean_opacity(&ink, &points);
            (el, ink.outline(&points), opacity)
        })
        .filter(|(_, pts, _)| !pts.is_empty())
        .collect();
    let (min_x, min_y, max_x, max_y) =
        match points_bounds(outlines.iter().flat_map(|(_, pts, _)| pts)) {
            Some((min, max)) => (
                min[0] - SVG_PAD,
                min[1] - SVG_PAD,
                max[0] + SVG_PAD,
                max[1] + SVG_PAD,
            ),
            None => (0.0, 0.0, 1.0, 1.0),
        };

    let mut svg = format!(
        r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="{min_x} {min_y} {w} {h}">"#,
        w = max_x - min_x,
        h = max_y - min_y,
    );
    svg.push('\n');

    for (el, pts, opacity) in &outlines {
        let style = el.ink().style();
        let [r, g, b, a] = style.color.0;
        let mut d = String::new();
        for (i, p) in pts.iter().enumerate() {
            let cmd = if i == 0 { 'M' } else { 'L' };
            write!(d, "{cmd}{x} {y} ", x = p[0], y = p[1]).expect("string write");
        }
        d.push('Z');
        let blend = match style.blend {
            Blend::Normal => "",
            Blend::Multiply => r#" style="mix-blend-mode: multiply""#,
        };
        writeln!(
            svg,
            r##"<path d="{d}" fill="rgb({r} {g} {b})" fill-opacity="{op}" fill-rule="nonzero"{blend}/>"##,
            op = f32::from(a) / 255.0 * style.opacity * opacity,
        )
        .expect("string write");
    }

    svg.push_str("</svg>\n");
    svg
}

/// Mean per-point opacity of the finished ink, 1 for an empty run.
fn mean_opacity(ink: &Ink<'_>, points: &[StrokePoint]) -> f32 {
    let states = TipEvaluator::evaluate(&ink.spec, ink.base_width, points, StrokeEnd::Complete);
    if states.is_empty() {
        return 1.0;
    }
    // usize -> f32 has no `From`; a point count is exact in f32.
    let count = states.len() as f32; // ast-grep-ignore: no-as-cast
    states.iter().map(|s| s.opacity).sum::<f32>() / count
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::element::{ShapeElement, Style};
    use crate::shape::Shape;
    use crate::stroke::{PointKind, Rgba, Stroke, StrokePoint, Tool};
    use crate::{ElementId, NoteId, StrokeId};

    #[test]
    fn export_rewrites_known_sketch_uris_only() {
        let note = NoteDoc::new(NoteId::new());
        let sketch = note.create_sketch(0).unwrap();
        note.add_stroke(
            sketch,
            &Stroke {
                id: StrokeId::new(),
                tool: Tool::Monoline,
                brush: None,
                color: Rgba::BLACK,
                base_width: 2.0,
                kind: PointKind::PolylineSample,
                points: vec![
                    StrokePoint {
                        x: 0.0,
                        y: 0.0,
                        force: 1.0,
                        t_ms: 0,
                        tilt: None,
                        size: None,
                    },
                    StrokePoint {
                        x: 10.0,
                        y: 5.0,
                        force: 1.0,
                        t_ms: 16,
                        tilt: None,
                        size: None,
                    },
                ],
                created_ms: 0,
            },
        )
        .unwrap();
        note.add_shape(
            sketch,
            &ShapeElement {
                id: ElementId::new(),
                shape: Shape::Rect {
                    center: [50.0, 50.0],
                    size: [40.0, 20.0],
                    angle: 0.0,
                },
                style: Style {
                    tool: Tool::Marker,
                    color: Rgba([255, 0, 0, 255]),
                    width: 3.0,
                },
                start: None,
                end: None,
                created_ms: 0,
            },
        )
        .unwrap();

        let missing = SketchId::new();
        note.splice_text(
            0,
            0,
            &format!(
                "# t\n\n![d]({SKETCH_URI_PREFIX}{sketch})\n\n![m]({SKETCH_URI_PREFIX}{missing})\n"
            ),
        )
        .unwrap();

        let bundle = note.export().unwrap();
        assert!(
            bundle
                .markdown
                .contains(&format!("![d](assets/{sketch}.svg)"))
        );
        assert!(
            bundle
                .markdown
                .contains(&format!("![m]({SKETCH_URI_PREFIX}{missing})"))
        );
        assert_eq!(bundle.assets.len(), 1);
        let svg = &bundle.assets[0].svg;
        assert_eq!(svg.matches("<path").count(), 2);
        assert_eq!(svg.matches(r#"fill-rule="nonzero""#).count(), 2);
        // The monoline stroke: opaque black, closed outline.
        assert!(
            svg.contains(r#"fill="rgb(0 0 0)" fill-opacity="1""#),
            "{svg}"
        );
        // The rect drawn with the marker preset: red, translucent, multiply.
        assert!(
            svg.contains(r#"fill="rgb(255 0 0)" fill-opacity="0.45""#),
            "{svg}"
        );
        assert!(svg.contains("mix-blend-mode: multiply"), "{svg}");
        assert!(svg.contains(" Z\""), "{svg}");
    }
}
