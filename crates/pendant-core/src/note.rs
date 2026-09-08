//! CRDT-backed note document: markdown text plus embedded vector sketches.
//!
//! Wraps `loro` entirely - no Loro types cross this module's public API, so
//! the CRDT stays swappable and the FFI surface stays small.

use loro::{ExportMode, LoroDoc, LoroList, LoroMap, LoroMovableList, LoroValue, ValueOrContainer};

use crate::stroke::{Rgba, Stroke, decode_chunks, encode_chunks};
use crate::{Error, NoteId, Result, SketchId, StrokeId};

const META: &str = "meta";
const TEXT: &str = "text";
const SKETCHES: &str = "sketches";

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
        sketch.insert("created_at", created_ms as i64)?;
        sketch.insert_container("strokes", LoroMovableList::new())?;
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

    pub fn add_stroke(&self, sketch: SketchId, stroke: &Stroke) -> Result<()> {
        let strokes = self.strokes_list(sketch)?;
        let map = strokes.push_container(LoroMap::new())?;
        map.insert("id", stroke.id.to_string())?;
        map.insert("tool", stroke.tool.as_str())?;
        map.insert("color", stroke.color.packed())?;
        map.insert("width", f64::from(stroke.base_width))?;
        map.insert("kind", stroke.kind.as_str())?;
        map.insert("created", stroke.created_ms as i64)?;
        let points = map.insert_container("points", LoroList::new())?;
        for chunk in encode_chunks(&stroke.points)? {
            points.push(LoroValue::Binary(chunk.into()))?;
        }
        self.doc.commit();
        Ok(())
    }

    pub fn remove_stroke(&self, sketch: SketchId, stroke: StrokeId) -> Result<()> {
        let strokes = self.strokes_list(sketch)?;
        let target = stroke.to_string();
        let index = (0..strokes.len()).find(|&i| {
            strokes
                .get(i)
                .and_then(as_map)
                .and_then(|m| m.get("id"))
                .and_then(as_string)
                .is_some_and(|id| id == target)
        });
        let Some(index) = index else {
            return Err(Error::Schema(format!("stroke {target} not found")));
        };
        strokes.delete(index, 1)?;
        self.doc.commit();
        Ok(())
    }

    /// All strokes of a sketch in z-order.
    pub fn strokes(&self, sketch: SketchId) -> Result<Vec<Stroke>> {
        let strokes = self.strokes_list(sketch)?;
        (0..strokes.len())
            .map(|i| {
                let map = strokes
                    .get(i)
                    .and_then(as_map)
                    .ok_or_else(|| Error::Schema("stroke entry is not a map".into()))?;
                Self::read_stroke(&map)
            })
            .collect()
    }

    fn read_stroke(map: &LoroMap) -> Result<Stroke> {
        let get_str = |key: &str| {
            map.get(key)
                .and_then(as_string)
                .ok_or_else(|| Error::Schema(format!("stroke missing {key}")))
        };
        let get_i64 = |key: &str| {
            map.get(key)
                .and_then(as_i64)
                .ok_or_else(|| Error::Schema(format!("stroke missing {key}")))
        };

        let points_list = map
            .get("points")
            .and_then(as_list)
            .ok_or_else(|| Error::Schema("stroke missing points".into()))?;
        let chunks: Vec<Vec<u8>> = (0..points_list.len())
            .filter_map(|i| points_list.get(i))
            .filter_map(as_binary)
            .collect();

        Ok(Stroke {
            id: get_str("id")?.parse()?,
            tool: get_str("tool")?.parse()?,
            color: Rgba::from_packed(get_i64("color")?),
            base_width: map
                .get("width")
                .and_then(as_f64)
                .ok_or_else(|| Error::Schema("stroke missing width".into()))?
                as f32,
            kind: get_str("kind")?.parse()?,
            points: decode_chunks(chunks.iter().map(Vec::as_slice))?,
            created_ms: get_i64("created")? as u64,
        })
    }

    fn strokes_list(&self, sketch: SketchId) -> Result<LoroMovableList> {
        self.doc
            .get_map(SKETCHES)
            .get(&sketch.to_string())
            .and_then(as_map)
            .and_then(|m| m.get("strokes"))
            .and_then(as_movable_list)
            .ok_or(Error::UnknownSketch(sketch))
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
    use crate::stroke::{PointKind, StrokePoint, Tool};

    fn sample_stroke() -> Stroke {
        Stroke {
            id: StrokeId::new(),
            tool: Tool::Pen,
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
