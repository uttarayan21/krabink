//! Markdown export: rewrites `pendant://sketch/<id>` image URIs to relative
//! SVG assets and renders each referenced sketch's elements to SVG.
//!
//! The markdown is rewritten by plain string substitution rather than a
//! parse-and-reserialize pass, so the exported source is byte-identical to
//! the note outside the rewritten URIs.

use std::fmt::Write as _;

use crate::element::Element;
use crate::note::NoteDoc;
use crate::{Result, SketchId};

pub const SKETCH_URI_PREFIX: &str = "pendant://sketch/";
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
    /// Render this note for consumption outside pendant.
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
/// Each element's outline becomes a fixed-width polyline path; a stroke's
/// width is scaled by its mean pressure, a shape's outline carries full
/// pressure so it gets the element's width. Pressure-varying outlines
/// would need the tessellator's mesh.
pub fn elements_to_svg(elements: &[Element]) -> String {
    let outlines: Vec<(&Element, Vec<crate::StrokePoint>)> = elements
        .iter()
        .map(|el| (el, el.outline()))
        .filter(|(_, pts)| !pts.is_empty())
        .collect();
    let points = outlines.iter().flat_map(|(_, pts)| pts);
    let (min_x, min_y, max_x, max_y) = points.fold(
        (f32::MAX, f32::MAX, f32::MIN, f32::MIN),
        |(min_x, min_y, max_x, max_y), p| {
            (
                min_x.min(p.x),
                min_y.min(p.y),
                max_x.max(p.x),
                max_y.max(p.y),
            )
        },
    );
    let (min_x, min_y, max_x, max_y) = if outlines.is_empty() {
        (0.0, 0.0, 1.0, 1.0)
    } else {
        (
            min_x - SVG_PAD,
            min_y - SVG_PAD,
            max_x + SVG_PAD,
            max_y + SVG_PAD,
        )
    };

    let mut svg = format!(
        r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="{min_x} {min_y} {w} {h}">"#,
        w = max_x - min_x,
        h = max_y - min_y,
    );
    svg.push('\n');

    for (el, pts) in &outlines {
        // usize -> f32 has no `From`; a point count is exact in f32.
        let count = pts.len() as f32; // ast-grep-ignore: no-as-cast
        let mean_force = pts.iter().map(|p| p.force).sum::<f32>() / count;
        let width = (el.base_width() * mean_force.clamp(0.3, 1.0)).max(0.1);
        let [r, g, b, a] = el.color().0;

        let mut d = String::new();
        for (i, p) in pts.iter().enumerate() {
            let cmd = if i == 0 { 'M' } else { 'L' };
            write!(d, "{cmd}{x} {y} ", x = p.x, y = p.y).expect("string write");
        }
        writeln!(
            svg,
            r##"<path d="{d}" fill="none" stroke="rgb({r} {g} {b})" stroke-opacity="{op}" stroke-width="{width}" stroke-linecap="round" stroke-linejoin="round"/>"##,
            d = d.trim_end(),
            op = f32::from(a) / 255.0,
        )
        .expect("string write");
    }

    svg.push_str("</svg>\n");
    svg
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
                tool: Tool::Pen,
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
                    tool: Tool::Pen,
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
        // The rect's five outline points at full width, in its colour.
        assert!(svg.contains(r#"stroke="rgb(255 0 0)""#), "{svg}");
        assert!(svg.contains(r#"stroke-width="3""#), "{svg}");
        assert!(svg.contains("M30 40 L70 40 L70 60 L30 60 L30 40"), "{svg}");
    }
}
