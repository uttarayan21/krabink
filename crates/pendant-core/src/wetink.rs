//! Wet-ink payloads: the transient stroke stream carried inside the sync
//! protocol's ephemeral channel. Never persisted, never imported into a doc —
//! receivers render these provisionally and drop them once the authoritative
//! stroke lands in the CRDT at pen-up.

use serde::{Deserialize, Serialize};

use crate::stroke::{Rgba, Tool};
use crate::{Result, SketchId, StrokeId};

/// One live pen sample. No tilt — provisional rendering doesn't need it.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct WetPoint {
    pub x: f32,
    pub y: f32,
    /// Normalised pressure, 0..=1.
    pub force: f32,
    /// Rendered line width at this sample, canvas units. `None` → receivers
    /// fall back to `base_width * force`.
    pub width: Option<f32>,
}

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
    },
    Points {
        stroke: StrokeId,
        /// Monotonic per stroke; receivers drop stale/duplicate batches.
        seq: u32,
        /// Sender's unix-millis clock when the batch was sent. Latency
        /// telemetry only — meaningless across badly skewed clocks.
        sent_ms: u64,
        points: Vec<WetPoint>,
    },
    End {
        stroke: StrokeId,
        sent_ms: u64,
    },
    Cancel {
        stroke: StrokeId,
    },
}

impl WetInk {
    pub fn encode(&self) -> Result<Vec<u8>> {
        Ok(postcard::to_stdvec(self)?)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        Ok(postcard::from_bytes(bytes)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let msg = WetInk::Points {
            stroke: StrokeId::new(),
            seq: 7,
            sent_ms: 1_757_000_000_123,
            points: vec![
                WetPoint {
                    x: 1.5,
                    y: -2.0,
                    force: 0.5,
                    width: Some(3.25),
                },
                WetPoint {
                    x: 2.5,
                    y: -1.0,
                    force: 0.75,
                    width: None,
                },
            ],
        };
        assert_eq!(WetInk::decode(&msg.encode().unwrap()).unwrap(), msg);
    }

    #[test]
    fn garbage_rejected() {
        assert!(WetInk::decode(&[0xff, 0xff, 0xff]).is_err());
    }
}
