//! Wet-ink payloads: the transient stroke stream carried inside the sync
//! protocol's ephemeral channel. Never persisted, never imported into a doc —
//! receivers render these provisionally and drop them once the authoritative
//! stroke lands in the CRDT at pen-up.
//!
//! Points travel as the same chunk-coded [`StrokePoint`]s the committed
//! stroke stores ([`encode_chunks`]), so a receiver folds them through the
//! same brush and draws exactly what the sender drew; the commit then
//! replaces the provisional ink without a visible change.

use serde::{Deserialize, Serialize};

use crate::stroke::{Rgba, StrokePoint, Tilt, Tool, decode_chunks, encode_chunks};
use crate::{Result, SketchId, StrokeId};

/// A wet-ink event. `Begin` → `Points`* → `End`, keyed by the stroke id the
/// authoritative CRDT stroke will use, so receivers can swap provisional ink
/// for the committed stroke. `Cancel` replaces `End` when no stroke will
/// follow (the pen manipulated a ruler or a tool, not the ink): receivers
/// drop the provisional ink at once instead of waiting for a commit.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum WetInk {
    Begin {
        sketch: SketchId,
        stroke: StrokeId,
        tool: Tool,
        color: Rgba,
        base_width: f32,
        /// A custom brush's encoded spec ([`crate::BrushSpec::encode`]);
        /// `None` for the tool's preset.
        spec: Option<Vec<u8>>,
    },
    Points {
        stroke: StrokeId,
        /// Monotonic per stroke; receivers drop stale/duplicate batches.
        seq: u32,
        /// Sender's unix-millis clock when the batch was sent. Latency
        /// telemetry only — meaningless across badly skewed clocks.
        sent_ms: u64,
        /// [`encode_chunks`] of the points emitted since the last batch;
        /// `t_ms` counts from the stroke's first point.
        chunks: Vec<Vec<u8>>,
    },
    End {
        stroke: StrokeId,
        sent_ms: u64,
        /// The points pen-up added after the last batch (the landing on
        /// the last raw sample), chunk-coded like `Points`.
        tail: Vec<Vec<u8>>,
    },
    Cancel {
        stroke: StrokeId,
    },
    /// Where the sender's pen is right now, hovering or drawing: peers
    /// draw a pointer there (the tip's footprint plus a ring). Sent at a
    /// throttled rate; receivers drop a pointer that goes quiet.
    /// Variants are append-only: postcard tags enums by declaration
    /// order, and older receivers log-and-drop what they cannot decode.
    Pointer {
        sketch: SketchId,
        x: f32,
        y: f32,
        tilt: Option<Tilt>,
        /// `None` while the eraser is selected: no footprint, ring only.
        tool: Option<Tool>,
        color: Rgba,
        /// The tool's width, or the eraser's diameter.
        base_width: f32,
        /// Touching the glass (a wet stroke is streaming) vs hovering.
        down: bool,
        sent_ms: u64,
    },
    /// The pen left `sketch` (lifted away or the view closed).
    PointerGone {
        sketch: SketchId,
    },
}

impl WetInk {
    pub fn encode(&self) -> Result<Vec<u8>> {
        Ok(postcard::to_stdvec(self)?)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        Ok(postcard::from_bytes(bytes)?)
    }

    /// A `Points` batch for `points`.
    pub fn points(
        stroke: StrokeId,
        seq: u32,
        sent_ms: u64,
        points: &[StrokePoint],
    ) -> Result<Self> {
        Ok(Self::Points {
            stroke,
            seq,
            sent_ms,
            chunks: encode_chunks(points)?,
        })
    }

    /// An `End` carrying `tail`.
    pub fn end(stroke: StrokeId, sent_ms: u64, tail: &[StrokePoint]) -> Result<Self> {
        Ok(Self::End {
            stroke,
            sent_ms,
            tail: encode_chunks(tail)?,
        })
    }

    /// The points a `Points` batch or an `End` tail carries; empty for
    /// other events.
    pub fn decode_points(&self) -> Result<Vec<StrokePoint>> {
        match self {
            Self::Points { chunks, .. } => decode_chunks(chunks.iter().map(Vec::as_slice)),
            Self::End { tail, .. } => decode_chunks(tail.iter().map(Vec::as_slice)),
            Self::Begin { .. }
            | Self::Cancel { .. }
            | Self::Pointer { .. }
            | Self::PointerGone { .. } => Ok(Vec::new()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stroke::Tilt;

    #[test]
    fn points_roundtrip_through_chunks() {
        let points: Vec<StrokePoint> = (0..5)
            .map(|i| StrokePoint {
                x: 1.5 * i as f32,
                y: -2.0,
                force: 0.5,
                t_ms: i * 8,
                tilt: Some(Tilt {
                    azimuth: 1.0,
                    altitude: 0.5,
                    roll: 0.1,
                }),
                size: None,
            })
            .map(StrokePoint::quantized)
            .collect();
        let msg = WetInk::points(StrokeId::new(), 7, 1_757_000_000_123, &points).unwrap();
        let decoded = WetInk::decode(&msg.encode().unwrap()).unwrap();
        assert_eq!(decoded, msg);
        assert_eq!(decoded.decode_points().unwrap(), points);

        let end = WetInk::end(StrokeId::new(), 1, &points[..1]).unwrap();
        assert_eq!(end.decode_points().unwrap(), points[..1]);
        assert!(
            WetInk::Cancel {
                stroke: StrokeId::new()
            }
            .decode_points()
            .unwrap()
            .is_empty()
        );
    }

    #[test]
    fn pointer_roundtrip() {
        let sketch = crate::SketchId::new();
        let pointer = WetInk::Pointer {
            sketch,
            x: 12.5,
            y: 40.0,
            tilt: Some(Tilt {
                azimuth: 2.0,
                altitude: 0.9,
                roll: -0.2,
            }),
            tool: Some(Tool::Marker),
            color: Rgba([10, 20, 30, 255]),
            base_width: 6.0,
            down: false,
            sent_ms: 1_757_000_000_123,
        };
        let decoded = WetInk::decode(&pointer.encode().unwrap()).unwrap();
        assert_eq!(decoded, pointer);
        assert!(decoded.decode_points().unwrap().is_empty());

        let eraser = WetInk::Pointer {
            sketch,
            x: 0.0,
            y: 0.0,
            tilt: None,
            tool: None,
            color: Rgba([0; 4]),
            base_width: 24.0,
            down: true,
            sent_ms: 0,
        };
        assert_eq!(WetInk::decode(&eraser.encode().unwrap()).unwrap(), eraser);

        let gone = WetInk::PointerGone { sketch };
        assert_eq!(WetInk::decode(&gone.encode().unwrap()).unwrap(), gone);
    }

    #[test]
    fn garbage_rejected() {
        assert!(WetInk::decode(&[0xff, 0xff, 0xff]).is_err());
    }
}
