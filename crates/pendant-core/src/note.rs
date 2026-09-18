//! CRDT-backed note document: markdown text plus embedded vector sketches.
//!
//! Wraps `loro` entirely - no Loro types cross this module's public API, so
//! the CRDT stays swappable and the FFI surface stays small.

use loro::{ExportMode, LoroDoc, LoroList, LoroMap, LoroMovableList, LoroValue, ValueOrContainer};

use crate::brush::{BrushId, BrushSpec, CustomBrush};
use crate::element::{Binding, Element, ShapeElement, Style};
use crate::shape::Shape;
use crate::stroke::{Rgba, Stroke, Tool, decode_chunks, encode_chunks};
use crate::{ElementId, Error, NoteId, Result, SketchId, StrokeId};

const META: &str = "meta";
const TEXT: &str = "text";
const SKETCHES: &str = "sketches";
/// Per-sketch z-ordered element list. Named for the days it held only
/// strokes; renaming would orphan every existing sketch.
const ELEMENTS: &str = "strokes";
const ELEM_STROKE: &str = "stroke";
const ELEM_SHAPE: &str = "shape";

/// A single note: CommonMark text + sketches, one Loro doc.
pub struct NoteDoc {
    doc: LoroDoc,
    id: NoteId,
}

impl NoteDoc {
    pub fn new(id: NoteId) -> Self {
        Self {
            doc: LoroDoc::new(),
            id,
        }
    }

    /// Rebuild from a snapshot (and optionally further updates) as stored by
    /// [`crate::store::Store`].
    pub fn from_bytes<'a>(
        id: NoteId,
        snapshot: Option<&[u8]>,
        updates: impl IntoIterator<Item = &'a [u8]>,
    ) -> Result<Self> {
        let note = Self::new(id);
        if let Some(snapshot) = snapshot {
            note.doc.import(snapshot)?;
        }
        for update in updates {
            note.doc.import(update)?;
        }
        Ok(note)
    }

    pub fn id(&self) -> NoteId {
        self.id
    }

    // ---- text ----

    pub fn text(&self) -> String {
        self.doc.get_text(TEXT).to_string()
    }

    pub fn text_len(&self) -> usize {
        self.doc.get_text(TEXT).len_unicode()
    }

    /// Replace `del` unicode chars at `at` with `insert`.
    pub fn splice_text(&self, at: usize, del: usize, insert: &str) -> Result<()> {
        let text = self.doc.get_text(TEXT);
        if del > 0 {
            text.delete(at, del)?;
        }
        if !insert.is_empty() {
            text.insert(at, insert)?;
        }
        self.doc.commit();
        Ok(())
    }

    // ---- meta ----

    pub fn title(&self) -> Option<String> {
        as_string(self.doc.get_map(META).get("title")?)
    }

    pub fn set_title(&self, title: &str) -> Result<()> {
        self.doc.get_map(META).insert("title", title)?;
        self.doc.commit();
        Ok(())
    }

    // ---- sketches ----

    pub fn create_sketch(&self, created_ms: u64) -> Result<SketchId> {
        let id = SketchId::new();
        let sketch = self
            .doc
            .get_map(SKETCHES)
            .insert_container(&id.to_string(), LoroMap::new())?;
        sketch.insert("created_at", to_i64(created_ms))?;
        sketch.insert_container(ELEMENTS, LoroMovableList::new())?;
        self.doc.commit();
        Ok(id)
    }

    pub fn sketch_ids(&self) -> Vec<SketchId> {
        self.doc
            .get_map(SKETCHES)
            .keys()
            .filter_map(|k| k.parse().ok())
            .collect()
    }

    /// Append a stroke on top of the sketch's z-order.
    pub fn add_stroke(&self, sketch: SketchId, stroke: &Stroke) -> Result<()> {
        let elements = self.elements_list(sketch)?;
        let map = elements.push_container(LoroMap::new())?;
        map.insert("id", stroke.id.to_string())?;
        map.insert("elem", ELEM_STROKE)?;
        map.insert("tool", stroke.tool.as_str())?;
        map.insert("color", stroke.color.packed())?;
        map.insert("width", f64::from(stroke.base_width))?;
        map.insert("kind", stroke.kind.as_str())?;
        map.insert("created", to_i64(stroke.created_ms))?;
        if let Some(custom) = &stroke.brush {
            map.insert("brush", custom.id.to_string())?;
            map.insert("spec", LoroValue::Binary(custom.spec.encode()?.into()))?;
        }
        let points = map.insert_container("points", LoroList::new())?;
        for chunk in encode_chunks(&stroke.points)? {
            points.push(LoroValue::Binary(chunk.into()))?;
        }
        self.doc.commit();
        Ok(())
    }

    /// Append a shape on top of the sketch's z-order. Geometry is stored
    /// as flat scalar keys so a later edit merges per field.
    pub fn add_shape(&self, sketch: SketchId, shape: &ShapeElement) -> Result<()> {
        let elements = self.elements_list(sketch)?;
        let map = elements.push_container(LoroMap::new())?;
        map.insert("id", shape.id.to_string())?;
        map.insert("elem", ELEM_SHAPE)?;
        map.insert("tool", shape.style.tool.as_str())?;
        map.insert("color", shape.style.color.packed())?;
        map.insert("width", f64::from(shape.style.width))?;
        map.insert("created", to_i64(shape.created_ms))?;
        let (kind, fields): (&str, Vec<(&str, f32)>) = match shape.shape {
            Shape::Line { a, b } => (
                "line",
                vec![("ax", a[0]), ("ay", a[1]), ("bx", b[0]), ("by", b[1])],
            ),
            Shape::Arrow { a, b } => (
                "arrow",
                vec![("ax", a[0]), ("ay", a[1]), ("bx", b[0]), ("by", b[1])],
            ),
            Shape::Rect {
                center,
                size,
                angle,
            } => (
                "rect",
                vec![
                    ("cx", center[0]),
                    ("cy", center[1]),
                    ("w", size[0]),
                    ("h", size[1]),
                    ("angle", angle),
                ],
            ),
            Shape::Ellipse {
                center,
                radii,
                angle,
            } => (
                "ellipse",
                vec![
                    ("cx", center[0]),
                    ("cy", center[1]),
                    ("rx", radii[0]),
                    ("ry", radii[1]),
                    ("angle", angle),
                ],
            ),
        };
        map.insert("shape", kind)?;
        for (key, value) in fields {
            map.insert(key, f64::from(value))?;
        }
        for (key, binding) in [("start", shape.start), ("end", shape.end)] {
            if let Some(b) = binding {
                let m = map.insert_container(key, LoroMap::new())?;
                m.insert("element", b.element.to_string())?;
                m.insert("fx", f64::from(b.fixed_point[0]))?;
                m.insert("fy", f64::from(b.fixed_point[1]))?;
                m.insert("gap", f64::from(b.gap))?;
            }
        }
        self.doc.commit();
        Ok(())
    }

    /// Remove the element with `id` (stroke or shape).
    pub fn remove_element(&self, sketch: SketchId, id: ElementId) -> Result<()> {
        let elements = self.elements_list(sketch)?;
        let target = id.to_string();
        let index = (0..elements.len()).find(|&i| {
            elements
                .get(i)
                .and_then(as_map)
                .and_then(|m| m.get("id"))
                .and_then(as_string)
                .is_some_and(|id| id == target)
        });
        let Some(index) = index else {
            return Err(Error::Schema(format!("element {target} not found")));
        };
        elements.delete(index, 1)?;
        self.doc.commit();
        Ok(())
    }

    /// Alias of [`Self::remove_element`].
    pub fn remove_stroke(&self, sketch: SketchId, stroke: StrokeId) -> Result<()> {
        self.remove_element(sketch, stroke)
    }

    /// All elements of a sketch in z-order. Entries this version cannot
    /// read (a newer element kind, a malformed map) are logged and skipped
    /// rather than hiding the whole sketch; a binding whose target is not
    /// in the sketch is dropped.
    pub fn elements(&self, sketch: SketchId) -> Result<Vec<Element>> {
        let list = self.elements_list(sketch)?;
        let mut elements: Vec<Element> = (0..list.len())
            .filter_map(|i| {
                let Some(map) = list.get(i).and_then(as_map) else {
                    tracing::warn!(%sketch, index = i, "element entry is not a map; skipped");
                    return None;
                };
                match Self::read_element(&map) {
                    Ok(element) => element,
                    Err(err) => {
                        tracing::warn!(%sketch, index = i, %err, "unreadable element skipped");
                        None
                    }
                }
            })
            .collect();
        let ids: std::collections::HashSet<ElementId> = elements.iter().map(Element::id).collect();
        for el in &mut elements {
            if let Element::Shape(shape) = el {
                for binding in [&mut shape.start, &mut shape.end] {
                    if binding.is_some_and(|b| !ids.contains(&b.element)) {
                        *binding = None;
                    }
                }
            }
        }
        Ok(elements)
    }

    /// The strokes of a sketch in z-order (shapes filtered out).
    pub fn strokes(&self, sketch: SketchId) -> Result<Vec<Stroke>> {
        Ok(self
            .elements(sketch)?
            .into_iter()
            .filter_map(|el| match el {
                Element::Stroke(s) => Some(s),
                Element::Shape(_) => None,
            })
            .collect())
    }

    /// `Ok(None)` for an element kind this version does not know.
    fn read_element(map: &LoroMap) -> Result<Option<Element>> {
        // Entries written before shapes existed carry no discriminator.
        let elem = map
            .get("elem")
            .and_then(as_string)
            .unwrap_or_else(|| ELEM_STROKE.to_owned());
        match elem.as_str() {
            ELEM_STROKE => Self::read_stroke(map).map(|s| Some(Element::Stroke(s))),
            ELEM_SHAPE => Self::read_shape(map).map(|s| Some(Element::Shape(s))),
            other => {
                tracing::warn!(elem = other, "unknown element kind skipped");
                Ok(None)
            }
        }
    }

    fn read_stroke(map: &LoroMap) -> Result<Stroke> {
        let points_list = map
            .get("points")
            .and_then(as_list)
            .ok_or_else(|| Error::Schema("stroke missing points".into()))?;
        let chunks: Vec<Vec<u8>> = (0..points_list.len())
            .filter_map(|i| points_list.get(i))
            .filter_map(as_binary)
            .collect();

        // A tool this build does not know still renders, as a pen, rather
        // than hiding the stroke.
        let tool_name = get_str(map, "tool")?;
        let tool = tool_name.parse().unwrap_or_else(|err| {
            tracing::warn!(%err, "unknown tool; rendering as a pen");
            Tool::Pen
        });
        let brush = match (
            map.get("brush").and_then(as_string),
            map.get("spec").and_then(as_binary),
        ) {
            (Some(id), Some(bytes)) => match BrushSpec::decode(&bytes) {
                Ok(spec) => Some(CustomBrush {
                    id: BrushId(id),
                    spec,
                }),
                Err(err) => {
                    tracing::warn!(%err, brush = id, tool = tool_name, "unreadable brush spec; using the tool preset");
                    None
                }
            },
            _ => None,
        };

        Ok(Stroke {
            id: get_str(map, "id")?.parse()?,
            tool,
            brush,
            color: Rgba::from_packed(get_i64(map, "color")?),
            base_width: get_f32(map, "width")?,
            kind: get_str(map, "kind")?.parse()?,
            points: decode_chunks(chunks.iter().map(Vec::as_slice))?,
            created_ms: u64::try_from(get_i64(map, "created")?).unwrap_or(0),
        })
    }

    fn read_shape(map: &LoroMap) -> Result<ShapeElement> {
        let kind = get_str(map, "shape")?;
        let f = |key: &str| get_f32(map, key);
        let shape = match kind.as_str() {
            "line" => Shape::Line {
                a: [f("ax")?, f("ay")?],
                b: [f("bx")?, f("by")?],
            },
            "arrow" => Shape::Arrow {
                a: [f("ax")?, f("ay")?],
                b: [f("bx")?, f("by")?],
            },
            "rect" => Shape::Rect {
                center: [f("cx")?, f("cy")?],
                size: [f("w")?, f("h")?],
                angle: f("angle")?,
            },
            "ellipse" => Shape::Ellipse {
                center: [f("cx")?, f("cy")?],
                radii: [f("rx")?, f("ry")?],
                angle: f("angle")?,
            },
            other => return Err(Error::Schema(format!("unknown shape {other:?}"))),
        };
        let binding = |key: &str| -> Result<Option<Binding>> {
            let Some(m) = map.get(key).and_then(as_map) else {
                return Ok(None);
            };
            Ok(Some(Binding {
                element: get_str(&m, "element")?.parse()?,
                fixed_point: [get_f32(&m, "fx")?, get_f32(&m, "fy")?],
                gap: get_f32(&m, "gap")?,
            }))
        };
        Ok(ShapeElement {
            id: get_str(map, "id")?.parse()?,
            shape,
            style: Style {
                tool: get_str(map, "tool")?.parse()?,
                color: Rgba::from_packed(get_i64(map, "color")?),
                width: get_f32(map, "width")?,
            },
            start: binding("start")?,
            end: binding("end")?,
            created_ms: u64::try_from(get_i64(map, "created")?).unwrap_or(0),
        })
    }

    /// The sketch's z-ordered element list (historically named `strokes`).
    fn elements_list(&self, sketch: SketchId) -> Result<LoroMovableList> {
        self.doc
            .get_map(SKETCHES)
            .get(&sketch.to_string())
            .and_then(as_map)
            .and_then(|m| m.get(ELEMENTS))
            .and_then(as_movable_list)
            .ok_or(Error::UnknownSketch(sketch))
    }

    /// Append a raw entry, for tests of the tolerant reader. `chunks`
    /// become the `points` list when non-empty.
    #[cfg(test)]
    fn push_raw(
        &self,
        sketch: SketchId,
        fields: &[(&str, LoroValue)],
        chunks: &[Vec<u8>],
    ) -> Result<()> {
        let map = self.elements_list(sketch)?.push_container(LoroMap::new())?;
        for (k, v) in fields {
            map.insert(k, v.clone())?;
        }
        if !chunks.is_empty() {
            let points = map.insert_container("points", LoroList::new())?;
            for chunk in chunks {
                points.push(LoroValue::Binary(chunk.clone().into()))?;
            }
        }
        self.doc.commit();
        Ok(())
    }

    // ---- sync ----

    /// Encoded version vector of everything this doc has seen.
    pub fn version(&self) -> Vec<u8> {
        self.doc.oplog_vv().encode()
    }

    /// Incremental update containing everything the peer at `since` lacks.
    /// `since = &[]` (or garbage) falls back to the full history.
    pub fn export_updates_since(&self, since: &[u8]) -> Result<Vec<u8>> {
        let from = loro::VersionVector::decode(since).unwrap_or_default();
        Ok(self.doc.export(ExportMode::updates(&from))?)
    }

    pub fn export_snapshot(&self) -> Result<Vec<u8>> {
        Ok(self.doc.export(ExportMode::Snapshot)?)
    }

    pub fn import_update(&self, bytes: &[u8]) -> Result<()> {
        self.doc.import(bytes)?;
        Ok(())
    }
}

// ---- LoroValue plumbing, kept private to this module ----

fn get_str(map: &LoroMap, key: &str) -> Result<String> {
    map.get(key)
        .and_then(as_string)
        .ok_or_else(|| Error::Schema(format!("element missing {key}")))
}

fn get_i64(map: &LoroMap, key: &str) -> Result<i64> {
    map.get(key)
        .and_then(as_i64)
        .ok_or_else(|| Error::Schema(format!("element missing {key}")))
}

fn get_f32(map: &LoroMap, key: &str) -> Result<f32> {
    let v = map
        .get(key)
        .and_then(as_f64)
        .ok_or_else(|| Error::Schema(format!("element missing {key}")))?;
    // f64 -> f32 has no trait conversion; canvas coordinates fit easily.
    Ok(v as f32) // ast-grep-ignore: no-as-cast
}

/// Unix millis into Loro's integer; saturating (a clock past 2^63 ms is
/// not a real concern).
fn to_i64(ms: u64) -> i64 {
    i64::try_from(ms).unwrap_or(i64::MAX)
}

fn as_string(v: ValueOrContainer) -> Option<String> {
    match v {
        ValueOrContainer::Value(LoroValue::String(s)) => Some(s.to_string()),
        _ => None,
    }
}

fn as_i64(v: ValueOrContainer) -> Option<i64> {
    match v {
        ValueOrContainer::Value(LoroValue::I64(i)) => Some(i),
        _ => None,
    }
}

fn as_f64(v: ValueOrContainer) -> Option<f64> {
    match v {
        ValueOrContainer::Value(LoroValue::Double(d)) => Some(d),
        _ => None,
    }
}

fn as_binary(v: ValueOrContainer) -> Option<Vec<u8>> {
    match v {
        ValueOrContainer::Value(LoroValue::Binary(b)) => Some(b.to_vec()),
        _ => None,
    }
}

fn as_map(v: ValueOrContainer) -> Option<LoroMap> {
    v.into_container().ok()?.into_map().ok()
}

fn as_list(v: ValueOrContainer) -> Option<LoroList> {
    v.into_container().ok()?.into_list().ok()
}

fn as_movable_list(v: ValueOrContainer) -> Option<LoroMovableList> {
    v.into_container().ok()?.into_movable_list().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::brush::{Blend, Paint};
    use crate::stroke::{PointKind, StrokePoint};

    fn sample_stroke() -> Stroke {
        Stroke {
            id: StrokeId::new(),
            tool: Tool::Pen,
            brush: None,
            color: Rgba::BLACK,
            base_width: 2.0,
            kind: PointKind::PolylineSample,
            points: (0..40)
                .map(|i| StrokePoint {
                    x: i as f32,
                    y: (i * 2) as f32,
                    force: 0.7,
                    t_ms: i * 8,
                    tilt: None,
                    size: None,
                })
                .map(StrokePoint::quantized)
                .collect(),
            created_ms: 1_000,
        }
    }

    fn sample_shape(shape: Shape) -> ShapeElement {
        ShapeElement {
            id: ElementId::new(),
            shape,
            style: Style {
                tool: Tool::Marker,
                color: Rgba([10, 20, 30, 255]),
                width: 3.0,
            },
            start: None,
            end: None,
            created_ms: 2_000,
        }
    }

    #[test]
    fn shapes_roundtrip_and_keep_z_order_with_strokes() {
        let id = NoteId::new();
        let a = NoteDoc::new(id);
        let b = NoteDoc::new(id);
        let sketch = a.create_sketch(0).unwrap();

        let stroke = sample_stroke();
        let rect = sample_shape(Shape::Rect {
            center: [10.0, 20.0],
            size: [100.0, 50.0],
            angle: 0.25,
        });
        let arrow = ShapeElement {
            start: Some(Binding {
                element: rect.id,
                fixed_point: [1.0, 0.5],
                gap: 4.0,
            }),
            end: Some(Binding {
                element: ElementId::new(),
                fixed_point: [0.0, 0.5],
                gap: 0.0,
            }),
            ..sample_shape(Shape::Arrow {
                a: [0.0, 0.0],
                b: [80.0, 5.0],
            })
        };
        let ellipse = sample_shape(Shape::Ellipse {
            center: [1.0, 2.0],
            radii: [30.0, 30.0],
            angle: 0.0,
        });
        let line = sample_shape(Shape::Line {
            a: [1.0, 1.0],
            b: [2.0, 9.0],
        });
        a.add_shape(sketch, &rect).unwrap();
        a.add_stroke(sketch, &stroke).unwrap();
        a.add_shape(sketch, &arrow).unwrap();
        a.add_shape(sketch, &ellipse).unwrap();
        a.add_shape(sketch, &line).unwrap();

        b.import_update(&a.export_updates_since(&[]).unwrap())
            .unwrap();

        // The dangling end binding is dropped; the one to the rect stays.
        let expect_arrow = ShapeElement { end: None, ..arrow };
        let expected = vec![
            Element::Shape(rect),
            Element::Stroke(stroke),
            Element::Shape(expect_arrow),
            Element::Shape(ellipse),
            Element::Shape(line),
        ];
        assert_eq!(b.elements(sketch).unwrap(), expected);
        assert_eq!(b.strokes(sketch).unwrap().len(), 1);

        b.remove_element(sketch, rect.id).unwrap();
        let after = b.elements(sketch).unwrap();
        assert_eq!(after.len(), 4);
        // Its binding target gone, the arrow's start binding drops too.
        assert!(matches!(&after[1], Element::Shape(s) if s.start.is_none()));
    }

    #[test]
    fn unknown_and_malformed_entries_are_skipped() {
        let note = NoteDoc::new(NoteId::new());
        let sketch = note.create_sketch(0).unwrap();
        note.add_stroke(sketch, &sample_stroke()).unwrap();
        note.push_raw(
            sketch,
            &[
                ("id", ElementId::new().to_string().into()),
                ("elem", "hologram".into()),
            ],
            &[],
        )
        .unwrap();
        note.push_raw(
            sketch,
            &[
                ("id", ElementId::new().to_string().into()),
                ("elem", "shape".into()),
                ("shape", "rect".into()),
            ],
            &[],
        )
        .unwrap();
        note.add_shape(
            sketch,
            &sample_shape(Shape::Line {
                a: [0.0, 0.0],
                b: [5.0, 5.0],
            }),
        )
        .unwrap();
        let elements = note.elements(sketch).unwrap();
        assert_eq!(elements.len(), 2);
        assert!(matches!(elements[0], Element::Stroke(_)));
        assert!(matches!(elements[1], Element::Shape(_)));
    }

    #[test]
    fn custom_brushes_roundtrip_and_bad_specs_fall_back() {
        let id = NoteId::new();
        let a = NoteDoc::new(id);
        let b = NoteDoc::new(id);
        let sketch = a.create_sketch(0).unwrap();
        let mut spec = BrushSpec::preset(Tool::Marker);
        spec.paint = Paint {
            opacity: 0.3,
            blend: Blend::Normal,
            grain: None,
            ..spec.paint
        };
        let custom = Stroke {
            brush: Some(CustomBrush {
                id: BrushId("user:soft-marker".into()),
                spec: spec.clone(),
            }),
            tool: Tool::Marker,
            ..sample_stroke()
        };
        a.add_stroke(sketch, &custom).unwrap();
        b.import_update(&a.export_updates_since(&[]).unwrap())
            .unwrap();
        let read = b.strokes(sketch).unwrap();
        assert_eq!(read, vec![custom.clone()]);
        assert_eq!(read[0].spec().as_ref(), &spec);

        // A spec from the future, and a tool this build never heard of,
        // both still render: as the preset, and as a pen.
        let chunks = encode_chunks(&custom.points).unwrap();
        let mut future = spec.encode().unwrap();
        future[0] = 200;
        for (tool, spec_bytes) in [("marker", Some(future)), ("laser", None)] {
            let note = NoteDoc::new(NoteId::new());
            let sketch = note.create_sketch(0).unwrap();
            let mut fields: Vec<(&str, LoroValue)> = vec![
                ("id", StrokeId::new().to_string().into()),
                ("elem", "stroke".into()),
                ("tool", tool.into()),
                ("color", 255_i64.into()),
                ("width", 2.0_f64.into()),
                ("kind", "polyline".into()),
                ("created", 0_i64.into()),
            ];
            if let Some(bytes) = spec_bytes {
                fields.push(("brush", "user:x".into()));
                fields.push(("spec", LoroValue::Binary(bytes.into())));
            }
            note.push_raw(sketch, &fields, &chunks).unwrap();
            let read = note.strokes(sketch).unwrap();
            assert_eq!(read.len(), 1, "{tool}");
            assert!(read[0].brush.is_none(), "{tool}");
            assert_eq!(
                read[0].tool,
                if tool == "laser" {
                    Tool::Pen
                } else {
                    Tool::Marker
                }
            );
        }
    }

    #[test]
    fn text_and_stroke_roundtrip_through_sync() {
        let id = NoteId::new();
        let a = NoteDoc::new(id);
        let b = NoteDoc::new(id);

        a.splice_text(0, 0, "# hello\n").unwrap();
        let sketch = a.create_sketch(0).unwrap();
        let stroke = sample_stroke();
        a.add_stroke(sketch, &stroke).unwrap();

        b.import_update(&a.export_updates_since(&[]).unwrap())
            .unwrap();

        assert_eq!(b.text(), "# hello\n");
        assert_eq!(b.strokes(sketch).unwrap(), vec![stroke.clone()]);

        // and the reverse direction, incremental
        let before = a.version();
        b.splice_text(0, 1, "").unwrap();
        b.remove_stroke(sketch, stroke.id).unwrap();
        a.import_update(&b.export_updates_since(&before).unwrap())
            .unwrap();

        assert_eq!(a.text(), " hello\n");
        assert!(a.strokes(sketch).unwrap().is_empty());
    }
}
