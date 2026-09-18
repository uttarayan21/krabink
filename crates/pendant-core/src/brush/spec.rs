//! What a brush is: how raw input is smoothed ([`InputParams`]), the tip's
//! shape ([`Tip`]), how input properties drive the tip ([`Behavior`]), and
//! how the ink is painted ([`Paint`]). A spec is size-relative: every
//! length is a multiple of the stroke's `base_width`, so one spec serves
//! every picker width.
//!
//! Specs are plain data. The presets for the built-in tools live in
//! [`BrushSpec::preset`]; custom brushes carry their spec inline on each
//! stroke ([`crate::Stroke::brush`]) so a document renders without any
//! brush library. Serialised with a leading version byte
//! ([`BrushSpec::encode`]) so the layout can evolve behind it.

use serde::{Deserialize, Serialize};

use super::input::InputParams;
use crate::stroke::Tool;
use crate::{Error, Result};

/// Identifies a custom brush; `builtin:` ids are bundled with the app.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct BrushId(pub String);

impl core::fmt::Display for BrushId {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(&self.0)
    }
}

/// A brush a stroke was drawn with when it is not one of the presets.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CustomBrush {
    pub id: BrushId,
    pub spec: BrushSpec,
}

/// Everything that turns input points into ink.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BrushSpec {
    pub input: InputParams,
    pub tip: Tip,
    pub dynamics: Vec<Behavior>,
    pub paint: Paint,
}

/// The shape the tip leaves at one point, before dynamics.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Tip {
    /// Height over width of the footprint: 1 is round (or square), 0.25 a
    /// chisel, 0.15 a flat nib.
    pub aspect: f32,
    /// 0 square corners … 1 a full ellipse.
    pub corner: f32,
    pub orient: Orient,
    /// 1 is a hard edge; lower feathers the mask in the fragment shader.
    pub hardness: f32,
    /// Largest change of width or height per canvas unit travelled, as a
    /// fraction of `base_width`; keeps pressure spikes from stepping.
    pub max_size_rate: f32,
    /// The tip never gets smaller than this fraction of `base_width`,
    /// whatever the dynamics say.
    pub min_size: f32,
}

/// Which way the tip's long axis points.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum Orient {
    /// Across the direction of travel: a round or chisel tip whose width
    /// is always perpendicular to the stroke.
    Motion,
    /// Along the pen's nib angle (azimuth + barrel roll), or `fallback`
    /// radians when the pen reports no orientation. The ink is broad
    /// across the nib and thin along it.
    Nib { fallback: f32 },
    /// A fixed canvas angle in radians.
    Fixed(f32),
}

/// One input property driving one tip property.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Behavior {
    pub source: Source,
    pub curve: Curve,
    /// Output at source 0 and source 1: a multiplier for size and opacity
    /// targets, radians for rotation.
    pub range: [f32; 2],
    pub target: Target,
    /// Time constant of a first-order lowpass on the source, ms; 0 for
    /// none. Smooths speed and pressure jitter.
    pub damping_ms: f32,
}

/// A normalised 0..=1 input property.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum Source {
    /// Pen pressure, floored at [`InputParams::min_force`].
    Pressure,
    /// Speed in multiples of `base_width` per second, saturating at `max`.
    Speed { max: f32 },
    /// 0 with the pen upright, 1 flat on the glass; 0 without tilt data.
    Tilt,
    /// 0 at pen-down, 1 once `over` sizes have been travelled.
    DistanceFromStart { over: f32 },
    /// 1 until the last `over` sizes of a finished stroke, 0 at its very
    /// end; always 1 while the stroke is still live.
    DistanceToEnd { over: f32 },
}

/// How a source maps into its range.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum Curve {
    Linear,
    /// `t.powf(exponent)`.
    Pow(f32),
    Smoothstep,
}

impl Curve {
    fn apply(self, t: f32) -> f32 {
        let t = t.clamp(0.0, 1.0);
        match self {
            Self::Linear => t,
            Self::Pow(e) => t.powf(e),
            Self::Smoothstep => t * t * (3.0 - 2.0 * t),
        }
    }
}

impl Behavior {
    /// The behaviour's output for a damped source value in 0..=1.
    pub(super) fn output(&self, t: f32) -> f32 {
        let t = self.curve.apply(t);
        self.range[0] + (self.range[1] - self.range[0]) * t
    }
}

/// The tip property a behaviour changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Target {
    /// Multiplies the width (across the stroke).
    Width,
    /// Multiplies the height (along the stroke).
    Height,
    /// Multiplies both.
    Size,
    /// Multiplies the paint opacity.
    Opacity,
    /// Adds to the tip rotation, radians.
    Rotation,
}

/// How the ink is composited.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Paint {
    /// 0..=1, multiplied into the stroke colour's alpha.
    pub opacity: f32,
    pub overlap: Overlap,
    pub blend: Blend,
}

/// What happens where a stroke covers itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Overlap {
    /// Both layers show: a translucent stroke darkens where it crosses
    /// itself, like real ink.
    Accumulate,
    /// Each pixel is painted at most once per stroke: a highlighter that
    /// doubles back stays one shade. Renderers guarantee this; only hard
    /// tips may use it.
    Discard,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Blend {
    Normal,
    /// Ink multiplies what is under it: a highlighter over text keeps the
    /// text black.
    Multiply,
}

/// Bump when the serialised layout changes; older readers fall back to the
/// tool's preset.
const SPEC_VERSION: u8 = 1;

impl BrushSpec {
    /// The tuned brush for a built-in tool.
    pub fn preset(tool: Tool) -> Self {
        let input = InputParams {
            streamline: 0.3,
            min_distance: 0.25,
            min_force: 0.15,
        };
        let round = Tip {
            aspect: 1.0,
            corner: 1.0,
            orient: Orient::Motion,
            hardness: 1.0,
            max_size_rate: 2.0,
            min_size: 0.25,
        };
        let opaque = Paint {
            opacity: 1.0,
            overlap: Overlap::Accumulate,
            blend: Blend::Normal,
        };
        let behavior = |source, curve, range, target, damping_ms| Behavior {
            source,
            curve,
            range,
            target,
            damping_ms,
        };
        match tool {
            Tool::Pen => Self {
                input: InputParams {
                    streamline: 0.5,
                    ..input
                },
                tip: round,
                dynamics: vec![
                    behavior(
                        Source::Pressure,
                        Curve::Linear,
                        [0.5, 1.0],
                        Target::Size,
                        0.0,
                    ),
                    // 1.5 canvas units per ms at size 4 was the old
                    // saturation point: 375 sizes per second.
                    behavior(
                        Source::Speed { max: 375.0 },
                        Curve::Linear,
                        [1.0, 0.75],
                        Target::Size,
                        40.0,
                    ),
                    behavior(
                        Source::DistanceToEnd { over: 1.5 },
                        Curve::Linear,
                        [0.25, 1.0],
                        Target::Size,
                        0.0,
                    ),
                ],
                paint: opaque,
            },
            Tool::Pencil => Self {
                input,
                tip: Tip {
                    hardness: 0.7,
                    ..round
                },
                dynamics: vec![
                    behavior(Source::Tilt, Curve::Linear, [1.0, 2.2], Target::Size, 0.0),
                    behavior(
                        Source::Tilt,
                        Curve::Linear,
                        [1.0, 0.45],
                        Target::Opacity,
                        0.0,
                    ),
                    behavior(
                        Source::Pressure,
                        Curve::Pow(0.8),
                        [0.3, 1.0],
                        Target::Opacity,
                        0.0,
                    ),
                    behavior(
                        Source::Pressure,
                        Curve::Linear,
                        [0.8, 1.0],
                        Target::Size,
                        0.0,
                    ),
                ],
                paint: Paint {
                    opacity: 0.9,
                    ..opaque
                },
            },
            Tool::Marker => Self {
                input: InputParams {
                    streamline: 0.35,
                    min_distance: 0.5,
                    ..input
                },
                tip: Tip {
                    aspect: 0.25,
                    corner: 0.1,
                    orient: Orient::Nib {
                        fallback: core::f32::consts::FRAC_PI_4,
                    },
                    ..round
                },
                dynamics: Vec::new(),
                paint: Paint {
                    opacity: 0.45,
                    overlap: Overlap::Discard,
                    blend: Blend::Multiply,
                },
            },
            Tool::Monoline => Self {
                input: InputParams {
                    streamline: 0.2,
                    ..input
                },
                tip: round,
                dynamics: Vec::new(),
                paint: opaque,
            },
            Tool::Fountain => Self {
                input,
                tip: Tip {
                    aspect: 0.15,
                    corner: 0.0,
                    orient: Orient::Nib {
                        fallback: -core::f32::consts::FRAC_PI_4,
                    },
                    ..round
                },
                dynamics: vec![behavior(
                    Source::Pressure,
                    Curve::Linear,
                    [0.85, 1.15],
                    Target::Width,
                    0.0,
                )],
                paint: opaque,
            },
        }
    }

    /// Does the tip follow the pen's orientation rather than the motion?
    pub fn has_nib(&self) -> bool {
        !matches!(self.tip.orient, Orient::Motion)
    }

    /// Versioned bytes for storage and the wire.
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut bytes = vec![SPEC_VERSION];
        bytes.extend(postcard::to_stdvec(self)?);
        Ok(bytes)
    }

    /// Reads bytes from [`Self::encode`]; a version this build does not
    /// know is a schema error, which readers treat as "use the preset".
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        match bytes.split_first() {
            Some((&SPEC_VERSION, rest)) => Ok(postcard::from_bytes(rest)?),
            Some((version, _)) => Err(Error::Schema(format!("brush spec version {version}"))),
            None => Err(Error::Schema("empty brush spec".into())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn specs_roundtrip_and_reject_unknown_versions() {
        for tool in [
            Tool::Pen,
            Tool::Pencil,
            Tool::Marker,
            Tool::Monoline,
            Tool::Fountain,
        ] {
            let spec = BrushSpec::preset(tool);
            let bytes = spec.encode().unwrap();
            assert_eq!(BrushSpec::decode(&bytes).unwrap(), spec, "{tool:?}");
            let mut future = bytes.clone();
            future[0] = SPEC_VERSION + 1;
            assert!(BrushSpec::decode(&future).is_err());
        }
        assert!(BrushSpec::decode(&[]).is_err());
        assert!(BrushSpec::decode(&[SPEC_VERSION, 0xff, 0xff]).is_err());
    }

    #[test]
    fn curves_map_the_unit_interval() {
        assert_eq!(Curve::Linear.apply(0.25), 0.25);
        assert_eq!(Curve::Pow(2.0).apply(0.5), 0.25);
        assert_eq!(Curve::Smoothstep.apply(0.0), 0.0);
        assert_eq!(Curve::Smoothstep.apply(1.0), 1.0);
        assert_eq!(Curve::Linear.apply(7.0), 1.0);
    }
}
