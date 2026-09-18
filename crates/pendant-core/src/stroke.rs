//! Vector stroke model and the compact point-chunk codec.
//!
//! Points are quantised to 1/8 canvas unit on entry. A chunk stores the first
//! point absolutely and every following point as a delta, so a typical pen
//! sample costs ~5 bytes after postcard's varint encoding. Chunks are split
//! whenever a delta would overflow its field, so arbitrarily large jumps are
//! representable.

use std::borrow::Cow;

use serde::{Deserialize, Serialize};

use crate::brush::{BrushSpec, CustomBrush};
use crate::{Error, Result, StrokeId};

/// Sub-unit resolution of stored coordinates (1/8 canvas unit).
const QUANT: f32 = 8.0;
/// Upper bound on points per chunk; keeps individual CRDT values small.
const MAX_CHUNK_POINTS: usize = 64;

/// Drawing tool a stroke was made with; names one of the built-in brush
/// presets ([`BrushSpec::preset`](crate::BrushSpec::preset)).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Tool {
    Pen,
    /// Graphite: tilt widens and lightens, pressure darkens.
    Pencil,
    /// Chisel highlighter: translucent, never darker where it crosses
    /// itself.
    Marker,
    Monoline,
    /// Flat calligraphy nib oriented by the pen's azimuth + barrel roll
    /// (Apple Pencil Pro), so the ink is broad across the nib and thin
    /// along it. Stored as `"brush"`, its name before pencils existed.
    Fountain,
}

impl Tool {
    pub const ALL: [Self; 5] = [
        Self::Pen,
        Self::Pencil,
        Self::Marker,
        Self::Monoline,
        Self::Fountain,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pen => "pen",
            Self::Pencil => "pencil",
            Self::Marker => "marker",
            Self::Monoline => "monoline",
            Self::Fountain => "brush",
        }
    }
}

impl core::str::FromStr for Tool {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self> {
        match s {
            "pen" => Ok(Self::Pen),
            "pencil" => Ok(Self::Pencil),
            "marker" => Ok(Self::Marker),
            "monoline" => Ok(Self::Monoline),
            "brush" | "fountain" => Ok(Self::Fountain),
            other => Err(Error::Schema(format!("unknown tool {other:?}"))),
        }
    }
}

/// What the stored points describe. Every stroke authored since the
/// canvas took over input from PencilKit is `PolylineSample` (the output
/// of `BrushModeler`); `BSplineControl` only exists in older data and is
/// still flattened and rendered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PointKind {
    /// Control points of PencilKit's uniform cubic B-spline (legacy).
    BSplineControl,
    /// Modelled polyline samples, each carrying its rendered width.
    PolylineSample,
}

impl PointKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::BSplineControl => "bspline",
            Self::PolylineSample => "polyline",
        }
    }
}

impl core::str::FromStr for PointKind {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self> {
        match s {
            "bspline" => Ok(Self::BSplineControl),
            "polyline" => Ok(Self::PolylineSample),
            other => Err(Error::Schema(format!("unknown point kind {other:?}"))),
        }
    }
}

/// Pen orientation, PencilKit-authored strokes only.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Tilt {
    /// Radians, 0..2π.
    pub azimuth: f32,
    /// Radians, 0 (flat) .. π/2 (perpendicular).
    pub altitude: f32,
    /// Barrel roll, radians -π..π; 0 for pens that cannot report it.
    pub roll: f32,
}

impl Tilt {
    /// Orientation of a flat nib on the canvas: where the barrel points,
    /// turned by how far it was rolled.
    pub fn nib_angle(self) -> f32 {
        self.azimuth + self.roll
    }
}

/// Rendered point size in canvas units, PencilKit-authored strokes only.
/// PencilKit derives it from more than force (speed, tool response curves),
/// so dropping it changes stroke appearance after a sync roundtrip.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct PointSize {
    pub w: f32,
    pub h: f32,
}

/// One pen sample / control point.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct StrokePoint {
    pub x: f32,
    pub y: f32,
    /// Normalised pressure, 0..=1.
    pub force: f32,
    /// Milliseconds since stroke start.
    pub t_ms: u32,
    pub tilt: Option<Tilt>,
    pub size: Option<PointSize>,
}

impl StrokePoint {
    /// Snap to the codec's storage resolution. Encoding applies this
    /// implicitly; apply it manually when comparing round-tripped points.
    pub fn quantized(self) -> Self {
        let q8 = |v: f32| (v * QUANT).round() / QUANT;
        let q255 = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() / 255.0;
        Self {
            x: q8(self.x),
            y: q8(self.y),
            force: q255(self.force),
            t_ms: self.t_ms,
            tilt: self.tilt.map(|t| Tilt {
                azimuth: unpack_azimuth(pack_azimuth(t.azimuth)),
                altitude: unpack_altitude(pack_altitude(t.altitude)),
                roll: unpack_roll(pack_roll(t.roll)),
            }),
            size: self.size.map(|s| PointSize {
                w: unpack_size(pack_size(s.w)),
                h: unpack_size(pack_size(s.h)),
            }),
        }
    }
}

/// RGBA colour, 8 bits per channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rgba(pub [u8; 4]);

impl Rgba {
    pub const BLACK: Self = Self([0, 0, 0, 255]);

    pub fn packed(self) -> i64 {
        i64::from(u32::from_be_bytes(self.0))
    }

    pub fn from_packed(v: i64) -> Self {
        Self(u32::to_be_bytes(v as u32))
    }
}

/// A finished stroke as clients consume it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Stroke {
    pub id: StrokeId,
    /// The preset this stroke renders with when `brush` is `None`.
    pub tool: Tool,
    /// A custom brush, snapshotted inline so the stroke renders the same
    /// on a device without the brush library.
    pub brush: Option<CustomBrush>,
    pub color: Rgba,
    /// Full ink width in canvas units, the brush "size".
    pub base_width: f32,
    pub kind: PointKind,
    pub points: Vec<StrokePoint>,
    /// Unix millis at stroke creation.
    pub created_ms: u64,
}

impl Stroke {
    /// The brush this stroke renders with: its custom spec, or the tool's
    /// preset.
    pub fn spec(&self) -> Cow<'_, BrushSpec> {
        match &self.brush {
            Some(custom) => Cow::Borrowed(&custom.spec),
            None => Cow::Owned(BrushSpec::preset(self.tool)),
        }
    }
}

/// Storage form of a run of points (see module docs for layout).
#[derive(Debug, Serialize, Deserialize)]
struct ChunkRepr {
    qx0: i32,
    qy0: i32,
    t0_ms: u32,
    force0: u8,
    /// (dqx, dqy, force, dt_ms) per subsequent point.
    deltas: Vec<(i16, i16, u8, u16)>,
    /// Parallel to the full point run (len == deltas.len() + 1) when present.
    tilts: Option<Vec<(u8, u8)>>,
    /// Quantised (w, h) per point, parallel run like `tilts`.
    sizes: Option<Vec<(u16, u16)>>,
    /// Quantised barrel roll per point, parallel to `tilts` (present iff
    /// `tilts` is). Added after the first data shipped; see `ChunkReprV1`.
    rolls: Option<Vec<u8>>,
}

/// Chunk layout before `rolls` existed. Postcard is not self-describing,
/// so old bytes are decoded through this and read as roll 0.
#[derive(Debug, Deserialize)]
struct ChunkReprV1 {
    qx0: i32,
    qy0: i32,
    t0_ms: u32,
    force0: u8,
    deltas: Vec<(i16, i16, u8, u16)>,
    tilts: Option<Vec<(u8, u8)>>,
    sizes: Option<Vec<(u16, u16)>>,
}

impl From<ChunkReprV1> for ChunkRepr {
    fn from(v1: ChunkReprV1) -> Self {
        let rolls = v1.tilts.as_ref().map(|t| vec![pack_roll(0.0); t.len()]);
        Self {
            qx0: v1.qx0,
            qy0: v1.qy0,
            t0_ms: v1.t0_ms,
            force0: v1.force0,
            deltas: v1.deltas,
            tilts: v1.tilts,
            sizes: v1.sizes,
            rolls,
        }
    }
}

/// Roll in 1/256 turns as a two's-complement byte, so 0 is exact and
/// -π..π covers -128..127 (about 1.4° per step).
fn pack_roll(v: f32) -> u8 {
    let steps = (v / core::f32::consts::TAU * 256.0).round();
    (steps.clamp(-128.0, 127.0) as i8) as u8
}

fn unpack_roll(v: u8) -> f32 {
    f32::from(v as i8) / 256.0 * core::f32::consts::TAU
}

fn pack_azimuth(rad: f32) -> u8 {
    let turn = rad.rem_euclid(core::f32::consts::TAU) / core::f32::consts::TAU;
    (turn * 255.0).round() as u8
}

fn unpack_azimuth(v: u8) -> f32 {
    f32::from(v) / 255.0 * core::f32::consts::TAU
}

fn pack_altitude(rad: f32) -> u8 {
    let frac = (rad / core::f32::consts::FRAC_PI_2).clamp(0.0, 1.0);
    (frac * 255.0).round() as u8
}

fn unpack_altitude(v: u8) -> f32 {
    f32::from(v) / 255.0 * core::f32::consts::FRAC_PI_2
}

fn quant(v: f32) -> i32 {
    (v * QUANT).round() as i32
}

fn pack_size(v: f32) -> u16 {
    ((v * QUANT).round()).clamp(0.0, f32::from(u16::MAX)) as u16
}

fn unpack_size(v: u16) -> f32 {
    f32::from(v) / QUANT
}

/// Encode a run of points into one or more postcard chunks.
pub fn encode_chunks(points: &[StrokePoint]) -> Result<Vec<Vec<u8>>> {
    let mut chunks = Vec::new();
    let mut iter = points.iter().peekable();

    while let Some(first) = iter.next() {
        let with_tilt = first.tilt.is_some();
        let with_size = first.size.is_some();
        let mut repr = ChunkRepr {
            qx0: quant(first.x),
            qy0: quant(first.y),
            t0_ms: first.t_ms,
            force0: (first.force.clamp(0.0, 1.0) * 255.0).round() as u8,
            deltas: Vec::new(),
            tilts: first
                .tilt
                .map(|t| vec![(pack_azimuth(t.azimuth), pack_altitude(t.altitude))]),
            sizes: first.size.map(|s| vec![(pack_size(s.w), pack_size(s.h))]),
            rolls: first.tilt.map(|t| vec![pack_roll(t.roll)]),
        };
        let (mut prev_qx, mut prev_qy, mut prev_t) = (repr.qx0, repr.qy0, repr.t0_ms);

        while repr.deltas.len() + 1 < MAX_CHUNK_POINTS {
            let Some(next) = iter.peek() else { break };
            let (qx, qy) = (quant(next.x), quant(next.y));
            let (dx, dy) = (qx - prev_qx, qy - prev_qy);
            let dt = next.t_ms.saturating_sub(prev_t);
            let fits = i16::try_from(dx).is_ok()
                && i16::try_from(dy).is_ok()
                && u16::try_from(dt).is_ok()
                && next.tilt.is_some() == with_tilt
                && next.size.is_some() == with_size;
            if !fits {
                break; // start a fresh chunk with an absolute first point
            }
            let next = iter.next().expect("peeked");
            repr.deltas.push((
                dx as i16,
                dy as i16,
                (next.force.clamp(0.0, 1.0) * 255.0).round() as u8,
                dt as u16,
            ));
            if let (Some(tilts), Some(tilt)) = (repr.tilts.as_mut(), next.tilt) {
                tilts.push((pack_azimuth(tilt.azimuth), pack_altitude(tilt.altitude)));
            }
            if let (Some(rolls), Some(tilt)) = (repr.rolls.as_mut(), next.tilt) {
                rolls.push(pack_roll(tilt.roll));
            }
            if let (Some(sizes), Some(size)) = (repr.sizes.as_mut(), next.size) {
                sizes.push((pack_size(size.w), pack_size(size.h)));
            }
            (prev_qx, prev_qy, prev_t) = (qx, qy, next.t_ms);
        }

        chunks.push(postcard::to_stdvec(&repr)?);
    }

    Ok(chunks)
}

/// Decode chunks produced by [`encode_chunks`] back into points.
pub fn decode_chunks<'a>(chunks: impl IntoIterator<Item = &'a [u8]>) -> Result<Vec<StrokePoint>> {
    let mut points = Vec::new();

    for bytes in chunks {
        let repr: ChunkRepr = match postcard::from_bytes::<ChunkRepr>(bytes) {
            Ok(repr) => repr,
            Err(_) => postcard::from_bytes::<ChunkReprV1>(bytes)?.into(),
        };
        if let Some(tilts) = &repr.tilts
            && tilts.len() != repr.deltas.len() + 1
        {
            return Err(Error::Schema("tilt run length mismatch".into()));
        }
        if let Some(rolls) = &repr.rolls
            && rolls.len() != repr.deltas.len() + 1
        {
            return Err(Error::Schema("roll run length mismatch".into()));
        }
        if let Some(sizes) = &repr.sizes
            && sizes.len() != repr.deltas.len() + 1
        {
            return Err(Error::Schema("size run length mismatch".into()));
        }

        let tilt_at = |i: usize| {
            repr.tilts.as_ref().map(|t| Tilt {
                azimuth: unpack_azimuth(t[i].0),
                altitude: unpack_altitude(t[i].1),
                roll: repr.rolls.as_ref().map_or(0.0, |r| unpack_roll(r[i])),
            })
        };
        let size_at = |i: usize| {
            repr.sizes.as_ref().map(|s| PointSize {
                w: unpack_size(s[i].0),
                h: unpack_size(s[i].1),
            })
        };

        let (mut qx, mut qy, mut t) = (repr.qx0, repr.qy0, repr.t0_ms);
        points.push(StrokePoint {
            x: qx as f32 / QUANT,
            y: qy as f32 / QUANT,
            force: f32::from(repr.force0) / 255.0,
            t_ms: t,
            tilt: tilt_at(0),
            size: size_at(0),
        });

        for (i, (dx, dy, force, dt)) in repr.deltas.iter().enumerate() {
            qx += i32::from(*dx);
            qy += i32::from(*dy);
            t += u32::from(*dt);
            points.push(StrokePoint {
                x: qx as f32 / QUANT,
                y: qy as f32 / QUANT,
                force: f32::from(*force) / 255.0,
                t_ms: t,
                tilt: tilt_at(i + 1),
                size: size_at(i + 1),
            });
        }
    }

    Ok(points)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pt(x: f32, y: f32, force: f32, t_ms: u32) -> StrokePoint {
        StrokePoint {
            x,
            y,
            force,
            t_ms,
            tilt: None,
            size: None,
        }
    }

    #[test]
    fn roundtrip_is_lossless_after_quantisation() {
        let points: Vec<_> = (0..200)
            .map(|i| {
                pt(
                    i as f32 * 1.37,
                    (i as f32).sin() * 40.0,
                    0.5 + (i % 3) as f32 * 0.1,
                    i * 8,
                )
            })
            .map(StrokePoint::quantized)
            .collect();
        let chunks = encode_chunks(&points).unwrap();
        assert!(chunks.len() >= points.len() / MAX_CHUNK_POINTS);
        let decoded = decode_chunks(chunks.iter().map(Vec::as_slice)).unwrap();
        assert_eq!(points, decoded);
    }

    #[test]
    fn large_jumps_split_chunks() {
        let points = vec![
            pt(0.0, 0.0, 1.0, 0),
            pt(10_000.0, -10_000.0, 1.0, 10), // delta overflows i16 at 1/8 quantisation
            pt(10_001.0, -10_001.0, 1.0, 20),
        ];
        let chunks = encode_chunks(&points).unwrap();
        assert_eq!(chunks.len(), 2);
        let decoded = decode_chunks(chunks.iter().map(Vec::as_slice)).unwrap();
        assert_eq!(
            points.iter().map(|p| p.quantized()).collect::<Vec<_>>(),
            decoded
        );
    }

    #[test]
    fn size_survives_roundtrip() {
        let points: Vec<_> = (0..80)
            .map(|i| StrokePoint {
                size: Some(PointSize {
                    w: 2.0 + (i as f32 * 0.37) % 9.0,
                    h: 2.0 + (i as f32 * 0.21) % 9.0,
                }),
                ..pt(i as f32, -(i as f32), 0.6, i * 8)
            })
            .map(StrokePoint::quantized)
            .collect();
        let chunks = encode_chunks(&points).unwrap();
        let decoded = decode_chunks(chunks.iter().map(Vec::as_slice)).unwrap();
        assert_eq!(points, decoded);
    }

    #[test]
    fn size_presence_flip_splits_chunk() {
        let mut points = vec![pt(0.0, 0.0, 1.0, 0), pt(1.0, 1.0, 1.0, 10)];
        points.push(StrokePoint {
            size: Some(PointSize { w: 3.0, h: 3.0 }),
            ..pt(2.0, 2.0, 1.0, 20)
        });
        let chunks = encode_chunks(&points).unwrap();
        assert_eq!(chunks.len(), 2);
        let decoded = decode_chunks(chunks.iter().map(Vec::as_slice)).unwrap();
        assert_eq!(
            points.iter().map(|p| p.quantized()).collect::<Vec<_>>(),
            decoded
        );
    }

    #[test]
    fn tilt_survives_roundtrip() {
        let points: Vec<_> = (0..3)
            .map(|i| StrokePoint {
                tilt: Some(Tilt {
                    azimuth: 1.0 + i as f32,
                    altitude: 0.3,
                    roll: -1.25,
                }),
                ..pt(i as f32, i as f32, 0.8, i * 16)
            })
            .map(StrokePoint::quantized)
            .collect();
        let chunks = encode_chunks(&points).unwrap();
        let decoded = decode_chunks(chunks.iter().map(Vec::as_slice)).unwrap();
        assert_eq!(points, decoded);
    }

    #[test]
    fn roll_roundtrips_and_legacy_chunks_decode_as_roll_zero() {
        let tilt = Tilt {
            azimuth: 1.0,
            altitude: 0.7,
            roll: 2.0,
        };
        let points = vec![
            StrokePoint {
                x: 1.0,
                y: 2.0,
                force: 0.5,
                t_ms: 0,
                tilt: Some(tilt),
                size: None,
            },
            StrokePoint {
                x: 3.0,
                y: 2.5,
                force: 0.6,
                t_ms: 8,
                tilt: Some(Tilt { roll: -3.0, ..tilt }),
                size: None,
            },
        ];
        let chunks = encode_chunks(&points).unwrap();
        let back = decode_chunks(chunks.iter().map(Vec::as_slice)).unwrap();
        for (a, b) in points.iter().zip(&back) {
            let (ra, rb) = (a.tilt.unwrap().roll, b.tilt.unwrap().roll);
            assert!((ra - rb).abs() < 0.03, "roll {ra} vs {rb}");
        }

        // Bytes written before the roll channel existed.
        let v1 = ChunkReprV1 {
            qx0: 8,
            qy0: 16,
            t0_ms: 0,
            force0: 128,
            deltas: vec![(8, 0, 128, 4)],
            tilts: Some(vec![(10, 20), (10, 20)]),
            sizes: None,
        };
        #[derive(Serialize)]
        struct V1Out {
            qx0: i32,
            qy0: i32,
            t0_ms: u32,
            force0: u8,
            deltas: Vec<(i16, i16, u8, u16)>,
            tilts: Option<Vec<(u8, u8)>>,
            sizes: Option<Vec<(u16, u16)>>,
        }
        let bytes = postcard::to_stdvec(&V1Out {
            qx0: v1.qx0,
            qy0: v1.qy0,
            t0_ms: v1.t0_ms,
            force0: v1.force0,
            deltas: v1.deltas.clone(),
            tilts: v1.tilts.clone(),
            sizes: v1.sizes.clone(),
        })
        .unwrap();
        let back = decode_chunks([bytes.as_slice()]).unwrap();
        assert_eq!(back.len(), 2);
        assert!(back.iter().all(|p| p.tilt.unwrap().roll == 0.0));
    }
}
