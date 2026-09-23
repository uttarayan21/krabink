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

/// Identifies an image a brush samples (a tip mask or a paper grain): the
/// content hash of its PNG bytes, so the same picture imported twice is
/// one asset and a stroke names exactly the pixels it was drawn with.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct AssetId(pub String);

impl AssetId {
    /// The id of these bytes: `a:` + 16 hex digits of FNV-1a 64.
    pub fn of(bytes: &[u8]) -> Self {
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for &b in bytes {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x0100_0000_01b3);
        }
        Self(format!("a:{h:016x}"))
    }
}

impl core::fmt::Display for AssetId {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(&self.0)
    }
}

/// What an image asset is for; renderers keep masks in a clamped array
/// and grains in a repeating one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AssetKind {
    Mask,
    Grain,
}

/// An image asset: greyscale PNG bytes and what they are for. Bundled
/// ones come from [`BrushSpec::builtin_assets`]; the workspace document
/// carries the rest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Asset {
    pub id: AssetId,
    pub name: String,
    pub kind: AssetKind,
    pub png: Vec<u8>,
}

/// Largest PNG the workspace accepts, bytes.
pub const MAX_ASSET_BYTES: usize = 64 * 1024;

impl Asset {
    /// An asset from PNG bytes, id derived from them; rejects anything
    /// that is not a PNG or is over [`MAX_ASSET_BYTES`].
    pub fn from_png(name: String, kind: AssetKind, png: Vec<u8>) -> Result<Self> {
        const SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
        if !png.starts_with(&SIGNATURE) {
            return Err(Error::Schema("asset is not a PNG".into()));
        }
        if png.len() > MAX_ASSET_BYTES {
            return Err(Error::Schema(format!(
                "asset is {} bytes, over the {MAX_ASSET_BYTES} limit",
                png.len()
            )));
        }
        Ok(Self {
            id: AssetId::of(&png),
            name,
            kind,
            png,
        })
    }
}

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
    pub emit: Emit,
    pub paint: Paint,
}

/// How the tip lays its ink along the path.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum Emit {
    /// One ribbon swept along the path: a stroker for round tips, hulls
    /// of the nib rectangle for oriented ones.
    Continuous,
    /// The tip's shape stamped at intervals along the path, each dab
    /// masked in the fragment shader; jitter roughens the edge the way a
    /// crayon or a grainy pencil does.
    Stamped(Stamped),
}

/// Dab placement for [`Emit::Stamped`]. Lengths are in sizes (multiples
/// of `base_width`), jitters are symmetric unless noted.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Stamped {
    /// Distance between dab centres along the path.
    pub spacing: f32,
    /// Random offset of each dab in any direction, up to this far.
    pub scatter: f32,
    /// Random rotation of each dab, up to ± this many radians.
    pub rotation_jitter: f32,
    /// Random size of each dab, up to ± this fraction.
    pub size_jitter: f32,
    /// Random opacity taken off each dab, up to this fraction.
    pub opacity_jitter: f32,
}

/// The handful of numbers a brush editor exposes, read from and written
/// back into a spec without the editor knowing its layout. `None` means
/// the spec has no such control (a continuous brush has no spacing);
/// writing `Some` where there was `None` adds the feature with defaults.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct BrushKnobs {
    /// [`Paint::opacity`], 0..=1.
    pub opacity: f32,
    /// [`Tip::hardness`], 0..=1.
    pub hardness: f32,
    /// [`Stamped::spacing`] in sizes.
    pub spacing: Option<f32>,
    /// [`Stamped::scatter`] in sizes.
    pub scatter: Option<f32>,
    /// [`Stamped::size_jitter`], 0..=1.
    pub size_jitter: Option<f32>,
    /// [`Stamped::opacity_jitter`], 0..=1.
    pub opacity_jitter: Option<f32>,
    /// [`Grain::strength`], 0..=1.
    pub grain_strength: Option<f32>,
    /// [`Grain::scale`] in canvas units.
    pub grain_scale: Option<f32>,
}

/// A brush bundled with the app under a `builtin:` id, offered next to
/// the tool presets. Strokes snapshot the spec inline like any custom
/// brush, so a bundled brush can be retuned without restyling old ink.
#[derive(Debug, Clone, PartialEq)]
pub struct BuiltinBrush {
    pub id: BrushId,
    pub name: &'static str,
    pub spec: BrushSpec,
}

/// The shape the tip leaves at one point, before dynamics.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
    /// The footprint's shape: the rounded superellipse from `aspect` and
    /// `corner`, or a greyscale image. Images only apply to stamped
    /// brushes; a continuous ribbon ignores them.
    pub mask: Mask,
}

/// What cuts a dab's shape.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Mask {
    Shape,
    /// A greyscale image over the tip rectangle, white where the ink is.
    Image(AssetId),
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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Paint {
    /// 0..=1, multiplied into the stroke colour's alpha.
    pub opacity: f32,
    pub overlap: Overlap,
    pub blend: Blend,
    /// Paper texture modulating the ink's alpha; `None` for flat ink.
    pub grain: Option<Grain>,
}

/// A texture that thins the ink where the paper's tooth would hold it
/// off: pencil, crayon. Evaluated in the fragment shader, so it costs no
/// geometry and does not change with zoom buckets.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Grain {
    pub source: GrainSource,
    pub mapping: GrainMapping,
    /// Size of one texture cell in canvas units.
    pub scale: f32,
    /// 0 leaves the ink flat; 1 lets the texture cut alpha all the way to
    /// zero in its dark cells.
    pub strength: f32,
}

/// Where the grain texture comes from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum GrainSource {
    /// Smooth value noise from an integer hash: identical on every
    /// platform, needs no asset.
    Noise,
    /// A tileable greyscale image, white where the paper takes ink.
    Image(AssetId),
}

/// What the grain texture is anchored to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum GrainMapping {
    /// The page: overlapping strokes reveal the same paper and the texture
    /// zooms with the ink.
    Canvas,
    /// The stroke's own arc length and side, seeded per stroke: a ribbon
    /// texture that travels with the stroke.
    Stroke,
}

/// What happens where a stroke covers itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Overlap {
    /// Both layers show: a translucent stroke darkens where it crosses
    /// itself, like real ink.
    Accumulate,
    /// Each pixel is painted at most once per stroke: a highlighter that
    /// doubles back stays one shade, and a wide translucent tip does not
    /// light up where the stroker's joins overlap themselves. Renderers
    /// guarantee this. A soft tip under it gets a stippled edge (fragments
    /// dropped by the paper noise) rather than a feathered one, since the
    /// first fragment at a pixel wins and must carry the full alpha.
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
const SPEC_VERSION: u8 = 4;

/// A soft chalk: the bundled chalk mask stamped over the bundled paper.
pub const BUILTIN_CHALK: &str = "builtin:chalk";
/// Bundled greyscale PNGs, embedded in the core so every platform has
/// them without a workspace.
const PAPER_PNG: &[u8] = include_bytes!("../../assets/paper.png");
const CHALK_PNG: &[u8] = include_bytes!("../../assets/chalk.png");

/// A wide, soft, stamped tip with paper grain, [`Tool::Pencil`]'s cousin.
pub const BUILTIN_CRAYON: &str = "builtin:crayon";
/// The pencil preset laid as scattered dabs: a rougher edge than the
/// continuous pencil.
pub const BUILTIN_PENCIL_GRAINY: &str = "builtin:pencil-grainy";

impl BrushSpec {
    /// The image assets bundled with the app: a paper grain and a chalk
    /// tip mask.
    pub fn builtin_assets() -> Vec<Asset> {
        vec![
            Asset {
                id: AssetId::of(PAPER_PNG),
                name: "Paper".into(),
                kind: AssetKind::Grain,
                png: PAPER_PNG.to_vec(),
            },
            Asset {
                id: AssetId::of(CHALK_PNG),
                name: "Chalk".into(),
                kind: AssetKind::Mask,
                png: CHALK_PNG.to_vec(),
            },
        ]
    }

    /// Every brush bundled with the app.
    pub fn builtins() -> Vec<BuiltinBrush> {
        let pencil = Self::preset(Tool::Pencil);
        let paper = AssetId::of(PAPER_PNG);
        let chalk = AssetId::of(CHALK_PNG);
        vec![
            BuiltinBrush {
                id: BrushId(BUILTIN_CRAYON.into()),
                name: "Crayon",
                spec: Self {
                    tip: Tip {
                        hardness: 0.6,
                        ..pencil.tip.clone()
                    },
                    dynamics: vec![
                        Behavior {
                            source: Source::Pressure,
                            curve: Curve::Linear,
                            range: [0.85, 1.0],
                            target: Target::Size,
                            damping_ms: 0.0,
                        },
                        Behavior {
                            source: Source::Pressure,
                            curve: Curve::Pow(0.8),
                            range: [0.5, 1.0],
                            target: Target::Opacity,
                            damping_ms: 0.0,
                        },
                        Behavior {
                            source: Source::Tilt,
                            curve: Curve::Linear,
                            range: [1.0, 1.6],
                            target: Target::Size,
                            damping_ms: 0.0,
                        },
                    ],
                    emit: Emit::Stamped(Stamped {
                        spacing: 0.15,
                        scatter: 0.06,
                        rotation_jitter: 0.0,
                        size_jitter: 0.1,
                        opacity_jitter: 0.3,
                    }),
                    paint: Paint {
                        opacity: 0.9,
                        grain: Some(Grain {
                            source: GrainSource::Noise,
                            mapping: GrainMapping::Canvas,
                            scale: 2.0,
                            strength: 0.6,
                        }),
                        ..pencil.paint.clone()
                    },
                    ..pencil.clone()
                },
            },
            BuiltinBrush {
                id: BrushId(BUILTIN_PENCIL_GRAINY.into()),
                name: "Pencil (grainy)",
                spec: Self {
                    emit: Emit::Stamped(Stamped {
                        spacing: 0.15,
                        scatter: 0.1,
                        rotation_jitter: 0.0,
                        size_jitter: 0.05,
                        opacity_jitter: 0.1,
                    }),
                    ..pencil.clone()
                },
            },
            BuiltinBrush {
                id: BrushId(BUILTIN_CHALK.into()),
                name: "Chalk",
                spec: Self {
                    tip: Tip {
                        hardness: 1.0,
                        mask: Mask::Image(chalk),
                        ..pencil.tip.clone()
                    },
                    dynamics: vec![
                        Behavior {
                            source: Source::Pressure,
                            curve: Curve::Linear,
                            range: [0.8, 1.0],
                            target: Target::Size,
                            damping_ms: 0.0,
                        },
                        Behavior {
                            source: Source::Pressure,
                            curve: Curve::Pow(0.7),
                            range: [0.4, 1.0],
                            target: Target::Opacity,
                            damping_ms: 0.0,
                        },
                    ],
                    emit: Emit::Stamped(Stamped {
                        spacing: 0.12,
                        scatter: 0.03,
                        rotation_jitter: core::f32::consts::PI,
                        size_jitter: 0.1,
                        opacity_jitter: 0.2,
                    }),
                    paint: Paint {
                        opacity: 0.85,
                        grain: Some(Grain {
                            source: GrainSource::Image(paper),
                            mapping: GrainMapping::Canvas,
                            scale: 48.0,
                            strength: 0.7,
                        }),
                        ..pencil.paint.clone()
                    },
                    ..pencil
                },
            },
        ]
    }

    /// The image assets this spec samples, for a renderer to resolve.
    pub fn assets(&self) -> Vec<AssetId> {
        let mut out = Vec::new();
        if let Mask::Image(id) = &self.tip.mask {
            out.push(id.clone());
        }
        if let Some(Grain {
            source: GrainSource::Image(id),
            ..
        }) = &self.paint.grain
        {
            out.push(id.clone());
        }
        out
    }

    /// The bundled brush with this id, if there is one.
    pub fn builtin(id: &str) -> Option<BuiltinBrush> {
        Self::builtins().into_iter().find(|b| b.id.0 == id)
    }
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
            mask: Mask::Shape,
        };
        let opaque = Paint {
            opacity: 1.0,
            overlap: Overlap::Accumulate,
            blend: Blend::Normal,
            grain: None,
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
                tip: round.clone(),
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
                emit: Emit::Continuous,
                paint: opaque.clone(),
            },
            Tool::Pencil => Self {
                input,
                tip: Tip {
                    hardness: 0.7,
                    ..round.clone()
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
                emit: Emit::Continuous,
                paint: Paint {
                    opacity: 0.9,
                    overlap: Overlap::Discard,
                    grain: Some(Grain {
                        source: GrainSource::Noise,
                        mapping: GrainMapping::Canvas,
                        scale: 1.5,
                        strength: 0.55,
                    }),
                    ..opaque.clone()
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
                    ..round.clone()
                },
                dynamics: Vec::new(),
                emit: Emit::Continuous,
                paint: Paint {
                    opacity: 0.45,
                    overlap: Overlap::Discard,
                    blend: Blend::Multiply,
                    grain: None,
                },
            },
            Tool::Monoline => Self {
                input: InputParams {
                    streamline: 0.2,
                    ..input
                },
                tip: round.clone(),
                dynamics: Vec::new(),
                emit: Emit::Continuous,
                paint: opaque.clone(),
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
                emit: Emit::Continuous,
                paint: opaque,
            },
        }
    }

    /// Is the ink laid as dabs rather than one ribbon?
    pub fn is_stamped(&self) -> bool {
        matches!(self.emit, Emit::Stamped(_))
    }

    /// The editable numbers of this spec.
    pub fn knobs(&self) -> BrushKnobs {
        let stamped = match &self.emit {
            Emit::Stamped(s) => Some(*s),
            Emit::Continuous => None,
        };
        BrushKnobs {
            opacity: self.paint.opacity,
            hardness: self.tip.hardness,
            spacing: stamped.map(|s| s.spacing),
            scatter: stamped.map(|s| s.scatter),
            size_jitter: stamped.map(|s| s.size_jitter),
            opacity_jitter: stamped.map(|s| s.opacity_jitter),
            grain_strength: self.paint.grain.as_ref().map(|g| g.strength),
            grain_scale: self.paint.grain.as_ref().map(|g| g.scale),
        }
    }

    /// This spec with `knobs` written into it. Stamping is added (at the
    /// pencil's dab rhythm) when any stamp knob is set on a continuous
    /// brush; grain is added (noise on the canvas) when a grain knob is
    /// set on a flat one.
    pub fn with_knobs(&self, knobs: &BrushKnobs) -> Self {
        let mut spec = self.clone();
        spec.paint.opacity = knobs.opacity.clamp(0.0, 1.0);
        spec.tip.hardness = knobs.hardness.clamp(0.0, 1.0);
        let stamp_knobs = [
            knobs.spacing,
            knobs.scatter,
            knobs.size_jitter,
            knobs.opacity_jitter,
        ];
        if stamp_knobs.iter().any(Option::is_some) {
            let mut s = match spec.emit {
                Emit::Stamped(s) => s,
                Emit::Continuous => Stamped {
                    spacing: 0.15,
                    scatter: 0.0,
                    rotation_jitter: 0.0,
                    size_jitter: 0.0,
                    opacity_jitter: 0.0,
                },
            };
            if let Some(v) = knobs.spacing {
                s.spacing = v.max(0.0);
            }
            if let Some(v) = knobs.scatter {
                s.scatter = v.max(0.0);
            }
            if let Some(v) = knobs.size_jitter {
                s.size_jitter = v.clamp(0.0, 1.0);
            }
            if let Some(v) = knobs.opacity_jitter {
                s.opacity_jitter = v.clamp(0.0, 1.0);
            }
            spec.emit = Emit::Stamped(s);
        }
        if knobs.grain_strength.is_some() || knobs.grain_scale.is_some() {
            let mut g = spec.paint.grain.clone().unwrap_or(Grain {
                source: GrainSource::Noise,
                mapping: GrainMapping::Canvas,
                scale: 1.5,
                strength: 0.5,
            });
            if let Some(v) = knobs.grain_strength {
                g.strength = v.clamp(0.0, 1.0);
            }
            if let Some(v) = knobs.grain_scale {
                g.scale = v.max(1e-3);
            }
            spec.paint.grain = Some(g);
        }
        spec
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
    fn knobs_roundtrip_and_add_features() {
        let crayon = BrushSpec::builtin(BUILTIN_CRAYON).unwrap().spec;
        let knobs = crayon.knobs();
        assert_eq!(knobs.spacing, Some(0.15));
        assert_eq!(
            crayon.with_knobs(&knobs),
            crayon,
            "writing back what was read is a no-op"
        );
        let pen = BrushSpec::preset(Tool::Pen);
        let pen_knobs = pen.knobs();
        assert_eq!(pen_knobs.spacing, None);
        assert_eq!(pen_knobs.grain_strength, None);
        let edited = pen.with_knobs(&BrushKnobs {
            opacity: 0.5,
            spacing: Some(0.3),
            grain_strength: Some(0.4),
            ..pen_knobs
        });
        assert!(edited.is_stamped());
        assert_eq!(edited.knobs().spacing, Some(0.3));
        assert_eq!(edited.knobs().grain_scale, Some(1.5));
        assert_eq!(edited.paint.opacity, 0.5);
        let clamped = pen.with_knobs(&BrushKnobs {
            opacity: 7.0,
            hardness: -1.0,
            ..pen_knobs
        });
        assert_eq!((clamped.paint.opacity, clamped.tip.hardness), (1.0, 0.0));
    }

    #[test]
    fn assets_hash_their_bytes_and_are_validated() {
        let assets = BrushSpec::builtin_assets();
        assert_eq!(assets.len(), 2);
        for a in &assets {
            assert_eq!(a.id, AssetId::of(&a.png));
            assert!(a.png.len() <= MAX_ASSET_BYTES, "{}", a.name);
            let again = Asset::from_png(a.name.clone(), a.kind, a.png.clone()).unwrap();
            assert_eq!(&again, a);
        }
        assert!(Asset::from_png("x".into(), AssetKind::Mask, vec![1, 2, 3]).is_err());
        let mut big = assets[0].png.clone();
        big.resize(MAX_ASSET_BYTES + 1, 0);
        assert!(Asset::from_png("x".into(), AssetKind::Grain, big).is_err());
        let chalk = BrushSpec::builtin(BUILTIN_CHALK).unwrap().spec;
        let ids = chalk.assets();
        assert_eq!(ids.len(), 2);
        assert!(ids.iter().all(|id| assets.iter().any(|a| &a.id == id)));
        assert!(BrushSpec::preset(Tool::Pen).assets().is_empty());
    }

    #[test]
    fn builtins_are_stamped_and_findable_by_id() {
        let all = BrushSpec::builtins();
        assert_eq!(all.len(), 3);
        for b in &all {
            assert!(b.id.0.starts_with("builtin:"), "{}", b.id);
            assert!(b.spec.is_stamped(), "{}", b.id);
            let bytes = b.spec.encode().unwrap();
            assert_eq!(BrushSpec::decode(&bytes).unwrap(), b.spec);
        }
        assert_eq!(BrushSpec::builtin(BUILTIN_CRAYON).unwrap().name, "Crayon");
        assert!(BrushSpec::builtin("builtin:nope").is_none());
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
