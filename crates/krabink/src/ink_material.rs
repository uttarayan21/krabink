//! The ink material: one [`Material2d`] for every brush, the desktop twin
//! of the Metal pipeline in `ios/Krabink/Sources/InkRenderer.swift`.
//!
//! Every stroke entity carries a [`MeshTag`] indexing a per-sketch table
//! of [`StrokeStyle`]s on the GPU ([`InkPalette`]); `ink.wgsl` reads the
//! stroke's colour, edge mask and grain from it. The table is a data
//! texture rather than a storage buffer so the same pipeline runs on
//! WebGL2: `Rgba32Uint`, [`STYLES_PER_ROW`] styles per row, four texels
//! per style (colour, mask, grain, then depth/flags/mask_layer/
//! grain_layer), floats stored as their bit patterns (see
//! [`pack_styles`]). The only pipeline splits are the blend function
//! (Normal/Multiply) and the depth compare that implements write-once ink
//! (Overlap::Discard), keyed by [`InkCombo`], so a sketch needs four
//! materials sharing one style texture.
//!
//! Depth: bevy's 2D pass uses reversed-Z cleared to 0, so a later stroke
//! (higher z) passes `GreaterEqual` over an earlier one. A Discard stroke
//! uses `Greater` and writes depth: where it covers itself the second
//! fragment sits at the same z and fails.
//!
//! Image masks (kind 2) and grains (kind 2) sample the two texture arrays
//! of [`crate::ink_assets::InkAssets`]; a style names its layer through
//! `mask_layer` / `grain_layer`.

use bevy::asset::{AssetPath, RenderAssetUsages, embedded_asset, embedded_path};
use bevy::mesh::{MeshTag, MeshVertexAttribute, MeshVertexBufferLayoutRef, VertexFormat};
use bevy::prelude::*;
use bevy::render::render_resource::{
    AsBindGroup, BlendComponent, BlendFactor, BlendOperation, BlendState, CompareFunction,
    Extent3d, RenderPipelineDescriptor, ShaderType, SpecializedMeshPipelineError, TextureDimension,
    TextureFormat,
};
use bevy::shader::ShaderRef;
use bevy::sprite_render::{AlphaMode2d, Material2d, Material2dKey, Material2dPlugin};
use krabink_core::{Blend, GrainMapping, InkStyle, MaskStyle, Overlap, Rgba};

use crate::ink_assets::InkAssets;
use crate::theme::PaperTone;

/// Stroke-space uv: `u` arc length in canvas units, `v` the side in -1..1.
pub const ATTRIBUTE_INK_UV: MeshVertexAttribute =
    MeshVertexAttribute::new("Ink_Uv", 0x50_454e_4401, VertexFormat::Float32x2);
/// Per-vertex opacity, 0..=1.
pub const ATTRIBUTE_INK_OPACITY: MeshVertexAttribute =
    MeshVertexAttribute::new("Ink_Opacity", 0x50_454e_4402, VertexFormat::Float32);

/// Registers the shader and the material.
pub struct InkMaterialPlugin;

impl Plugin for InkMaterialPlugin {
    fn build(&self, app: &mut App) {
        embedded_asset!(app, "ink.wgsl");
        app.add_plugins(Material2dPlugin::<InkMaterial>::default());
    }
}

/// Mirrors `StrokeStyle` in ink.wgsl and Shaders.metal: 64 bytes, the
/// same on both platforms. On the desktop it reaches the GPU as the four
/// texels of [`StrokeStyle::texels`].
#[derive(ShaderType, Debug, Clone, Copy, PartialEq, Default)]
pub struct StrokeStyle {
    /// Linear RGBA, straight alpha; the brush opacity is folded in.
    pub color: Vec4,
    /// aspect, corner, hardness, 0
    pub mask: Vec4,
    /// scale, strength, 0, 0
    pub grain: Vec4,
    /// NDC z slot on Metal; unused here, z comes from the transform.
    pub depth: f32,
    pub flags: u32,
    pub mask_layer: u32,
    pub grain_layer: u32,
}

impl StrokeStyle {
    /// Bits 0-1, mask kind 3: feather the ribbon edge by `mask.z`.
    /// Mask kind 1: rounded superellipse in tip space (stamped dabs).
    pub const MASK_SHAPE: u32 = 1;
    /// Mask kind 2: the `masks` array layer `mask_layer` over tip space.
    pub const MASK_IMAGE: u32 = 2;
    pub const MASK_EDGE: u32 = 3;
    /// Bits 2-3, grain kind 1: procedural value noise seeded by
    /// `grain_layer`.
    pub const GRAIN_NOISE: u32 = 4;
    /// Grain kind 2: the `grains` array layer `grain_layer`, tiled.
    pub const GRAIN_IMAGE: u32 = 8;
    /// Bit 4: grain follows the stroke's own uv rather than the canvas.
    pub const GRAIN_STROKE: u32 = 16;
    /// Bit 5: ink multiplies what is under it.
    pub const MULTIPLY: u32 = 32;
    /// Bit 6: each pixel painted at most once per stroke.
    pub const DISCARD: u32 = 64;

    /// The pipeline this style draws with.
    pub fn combo(&self) -> InkCombo {
        InkCombo {
            multiply: self.flags & Self::MULTIPLY != 0,
            discard: self.flags & Self::DISCARD != 0,
        }
    }

    /// The style's four `Rgba32Uint` texels, in the order `style()` in
    /// ink.wgsl loads them: colour, mask, grain, then depth, flags,
    /// mask_layer, grain_layer. Floats travel as their bit patterns.
    fn texels(&self) -> [[u32; 4]; TEXELS_PER_STYLE] {
        [
            self.color.to_array().map(f32::to_bits),
            self.mask.to_array().map(f32::to_bits),
            self.grain.to_array().map(f32::to_bits),
            [
                self.depth.to_bits(),
                self.flags,
                self.mask_layer,
                self.grain_layer,
            ],
        ]
    }
}

/// Styles per row of the style texture; `STYLES_PER_ROW` in ink.wgsl.
pub const STYLES_PER_ROW: u32 = 256;
/// Texels one style takes: colour, mask, grain, then the scalars.
const TEXELS_PER_STYLE: usize = 4;
/// Width of the style texture in texels.
const STYLE_TEXTURE_WIDTH: u32 = STYLES_PER_ROW * 4;

/// Rows of the style texture that hold `capacity` styles (at least one).
fn style_rows(capacity: usize) -> u32 {
    let per_row = usize::try_from(STYLES_PER_ROW).unwrap_or(usize::MAX);
    u32::try_from(capacity.div_ceil(per_row))
        .unwrap_or(u32::MAX)
        .max(1)
}

/// Lay `styles` out as `Rgba32Uint` texel data for a
/// `STYLE_TEXTURE_WIDTH` x height texture that holds at least `capacity`
/// styles; returns the height and the bytes. Style `i` starts at texel
/// `((i % STYLES_PER_ROW) * 4, i / STYLES_PER_ROW)`, which row-major is
/// simply texel `4 i`. Unused slots are zero ([`StrokeStyle::default`]).
fn pack_styles(styles: &[StrokeStyle], capacity: usize) -> (u32, Vec<u8>) {
    let height = style_rows(capacity.max(styles.len()));
    let slots = usize::try_from(height)
        .unwrap_or(usize::MAX)
        .saturating_mul(usize::try_from(STYLES_PER_ROW).unwrap_or(usize::MAX));
    let data = styles
        .iter()
        .copied()
        .chain(std::iter::repeat(StrokeStyle::default()))
        .take(slots)
        .flat_map(|style| style.texels())
        .flatten()
        .flat_map(u32::to_le_bytes)
        .collect();
    (height, data)
}

/// The style texture for `styles`, sized for `capacity` slots.
fn style_image(styles: &[StrokeStyle], capacity: usize) -> Image {
    let (height, data) = pack_styles(styles, capacity);
    Image::new(
        Extent3d {
            width: STYLE_TEXTURE_WIDTH,
            height,
            depth_or_array_layers: 1,
        },
        TextureDimension::D2,
        data,
        TextureFormat::Rgba32Uint,
        // The main world keeps the data so a flush can rewrite it in place.
        RenderAssetUsages::MAIN_WORLD | RenderAssetUsages::RENDER_WORLD,
    )
}

impl StrokeStyle {
    /// The GPU style for `style`, image layers resolved through `assets`
    /// (an asset the arrays lack falls back to the shape mask or noise).
    pub fn new(style: &InkStyle, assets: &InkAssets) -> Self {
        let LinearRgba {
            red,
            green,
            blue,
            alpha,
        } = linear(style.color);
        let (mask, edge, mask_layer) = match &style.mask {
            MaskStyle::Ribbon => (
                Vec4::new(1.0, 0.0, style.hardness, 0.0),
                if style.hardness < 1.0 {
                    Self::MASK_EDGE
                } else {
                    0
                },
                0,
            ),
            MaskStyle::Shape { corner } => (
                Vec4::new(1.0, *corner, style.hardness, 0.0),
                Self::MASK_SHAPE,
                0,
            ),
            MaskStyle::Image { asset, corner } => match assets.mask_layer(asset) {
                Some(layer) => (
                    Vec4::new(1.0, *corner, style.hardness, 0.0),
                    Self::MASK_IMAGE,
                    layer,
                ),
                None => (
                    Vec4::new(1.0, *corner, style.hardness, 0.0),
                    Self::MASK_SHAPE,
                    0,
                ),
            },
        };
        let multiply = match style.blend {
            Blend::Multiply => Self::MULTIPLY,
            Blend::Normal => 0,
        };
        let discard = match style.overlap {
            Overlap::Discard => Self::DISCARD,
            Overlap::Accumulate => 0,
        };
        let (grain, grain_flags, grain_layer) = match &style.grain {
            Some(g) => {
                let mapping = match g.mapping {
                    GrainMapping::Canvas => 0,
                    GrainMapping::Stroke => Self::GRAIN_STROKE,
                };
                let (kind, layer) = match g.image.as_ref().and_then(|id| assets.grain_layer(id)) {
                    Some(layer) => (Self::GRAIN_IMAGE, layer),
                    None => (Self::GRAIN_NOISE, g.seed),
                };
                (
                    Vec4::new(g.scale, g.strength, 0.0, 0.0),
                    kind | mapping,
                    layer,
                )
            }
            None => (Vec4::ZERO, 0, 0),
        };
        Self {
            color: Vec4::new(red, green, blue, alpha * style.opacity),
            mask,
            grain,
            depth: 0.0,
            flags: edge | multiply | discard | grain_flags,
            mask_layer,
            grain_layer,
        }
    }
}

/// sRGB bytes → linear light, the space the shader blends in.
fn linear(Rgba([r, g, b, a]): Rgba) -> LinearRgba {
    Srgba::rgba_u8(r, g, b, a).into()
}

/// The pipeline state a run of ink shares; the material's specialisation
/// key. Mirrors `InkCombo` in InkRenderer.swift.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct InkCombo {
    pub multiply: bool,
    pub discard: bool,
}

impl InkCombo {
    /// Every combination, indexed by [`Self::index`].
    pub const ALL: [Self; 4] = [
        Self {
            multiply: false,
            discard: false,
        },
        Self {
            multiply: true,
            discard: false,
        },
        Self {
            multiply: false,
            discard: true,
        },
        Self {
            multiply: true,
            discard: true,
        },
    ];

    /// Position in [`Self::ALL`].
    pub fn index(self) -> usize {
        usize::from(self.multiply) | (usize::from(self.discard) << 1)
    }

    fn blend_state(self, paper: PaperTone) -> BlendState {
        if self.multiply && paper == PaperTone::Dark {
            // Screen: out = src * (1 - dst) + dst, the highlighter on dark
            // paper (mirrors `psoScreen` in InkRenderer.swift). Multiply
            // would only darken, and darkening dark paper shows nothing.
            BlendState {
                color: BlendComponent {
                    src_factor: BlendFactor::OneMinusDst,
                    dst_factor: BlendFactor::One,
                    operation: BlendOperation::Add,
                },
                alpha: BlendComponent {
                    src_factor: BlendFactor::One,
                    dst_factor: BlendFactor::OneMinusSrcAlpha,
                    operation: BlendOperation::Add,
                },
            }
        } else if self.multiply {
            // out = dst * src + dst * (1 - a): multiply, straight through
            // where the ink is transparent. Colour is premultiplied.
            BlendState {
                color: BlendComponent {
                    src_factor: BlendFactor::Dst,
                    dst_factor: BlendFactor::OneMinusSrcAlpha,
                    operation: BlendOperation::Add,
                },
                alpha: BlendComponent {
                    src_factor: BlendFactor::One,
                    dst_factor: BlendFactor::OneMinusSrcAlpha,
                    operation: BlendOperation::Add,
                },
            }
        } else {
            BlendState::PREMULTIPLIED_ALPHA_BLENDING
        }
    }

    fn depth_compare(self) -> CompareFunction {
        if self.discard {
            CompareFunction::Greater
        } else {
            CompareFunction::GreaterEqual
        }
    }
}

/// The material's specialisation key: the run's [`InkCombo`] and the
/// paper it lands on (the highlighter's blend depends on both).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct InkKey {
    pub combo: InkCombo,
    pub paper: PaperTone,
}

impl From<&InkMaterial> for InkKey {
    fn from(material: &InkMaterial) -> Self {
        Self {
            combo: material.combo,
            paper: material.paper,
        }
    }
}

/// Mirrors `InkParams` in ink.wgsl (padded to 16 bytes).
#[derive(ShaderType, Debug, Clone, Copy, PartialEq, Default)]
pub struct InkParams {
    /// Canvas units per pixel; fades grain out as the view zooms out.
    pub zoom: f32,
    pad0: f32,
    pad1: f32,
    pad2: f32,
}

impl InkParams {
    pub fn new(zoom: f32) -> Self {
        Self {
            zoom,
            ..Self::default()
        }
    }
}

#[derive(Asset, AsBindGroup, TypePath, Debug, Clone)]
#[bind_group_data(InkKey)]
pub struct InkMaterial {
    /// The style texture (see [`pack_styles`]), indexed by each entity's
    /// [`MeshTag`]. Read with `textureLoad`, so no sampler.
    #[texture(0, sample_type = "u_int")]
    pub styles: Handle<Image>,
    #[uniform(1)]
    pub params: InkParams,
    /// Tip masks (mask kind 2), clamped.
    #[texture(2, dimension = "2d_array")]
    #[sampler(3)]
    pub masks: Handle<Image>,
    /// Paper grains (grain kind 2), repeating.
    #[texture(4, dimension = "2d_array")]
    #[sampler(5)]
    pub grains: Handle<Image>,
    /// Blend and depth state; half the specialisation key.
    pub combo: InkCombo,
    /// Light or dark paper under the ink; the other half of the key.
    pub paper: PaperTone,
}

fn shader() -> ShaderRef {
    ShaderRef::Path(AssetPath::from_path_buf(embedded_path!("ink.wgsl")).with_source("embedded"))
}

impl Material2d for InkMaterial {
    fn vertex_shader() -> ShaderRef {
        shader()
    }

    fn fragment_shader() -> ShaderRef {
        shader()
    }

    fn alpha_mode(&self) -> AlphaMode2d {
        AlphaMode2d::Blend
    }

    fn specialize(
        descriptor: &mut RenderPipelineDescriptor,
        layout: &MeshVertexBufferLayoutRef,
        key: Material2dKey<Self>,
    ) -> Result<(), SpecializedMeshPipelineError> {
        descriptor.vertex.buffers = vec![layout.0.get_layout(&[
            Mesh::ATTRIBUTE_POSITION.at_shader_location(0),
            ATTRIBUTE_INK_UV.at_shader_location(1),
            ATTRIBUTE_INK_OPACITY.at_shader_location(2),
        ])?];
        let InkKey { combo, paper } = key.bind_group_data;
        if let Some(fragment) = descriptor.fragment.as_mut() {
            fragment
                .targets
                .iter_mut()
                .flatten()
                .for_each(|target| target.blend = Some(combo.blend_state(paper)));
        }
        // The transparent 2D pipeline disables depth writes; ink relies on
        // them for draw order and write-once overlap.
        if let Some(depth) = descriptor.depth_stencil.as_mut() {
            depth.depth_write_enabled = Some(true);
            depth.depth_compare = Some(combo.depth_compare());
        }
        descriptor.label = Some(
            format!(
                "ink_pipeline_{}_{}_{}",
                if combo.multiply { "multiply" } else { "normal" },
                if combo.discard {
                    "discard"
                } else {
                    "accumulate"
                },
                match paper {
                    PaperTone::Light => "light",
                    PaperTone::Dark => "dark",
                }
            )
            .into(),
        );
        Ok(())
    }
}

/// A style's place in an [`InkPalette`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InkSlot {
    index: u32,
    combo: InkCombo,
}

/// A sketch's stroke styles: the GPU style texture every ink entity
/// indexes with its [`MeshTag`], and the four materials (one per
/// [`InkCombo`]) sharing it. Slots are reused through a free list so an
/// entity's tag stays put for its lifetime.
pub struct InkPalette {
    texture: Handle<Image>,
    materials: [Handle<InkMaterial>; 4],
    styles: Vec<StrokeStyle>,
    free: Vec<u32>,
    /// Styles uploaded per flush; grows geometrically so the GPU texture
    /// is rewritten in place rather than reallocated on most edits.
    capacity: usize,
    dirty: bool,
}

impl InkPalette {
    /// Slots a fresh palette holds before its first reallocation.
    const INITIAL_CAPACITY: usize = 64;

    pub fn new(
        images: &mut Assets<Image>,
        materials: &mut Assets<InkMaterial>,
        params: InkParams,
        assets: &InkAssets,
        paper: PaperTone,
    ) -> Self {
        let capacity = Self::INITIAL_CAPACITY;
        let texture = images.add(style_image(&[], capacity));
        let materials = InkCombo::ALL.map(|combo| {
            materials.add(InkMaterial {
                styles: texture.clone(),
                params,
                masks: assets.masks.clone(),
                grains: assets.grains.clone(),
                combo,
                paper,
            })
        });
        Self {
            texture,
            materials,
            styles: Vec::new(),
            free: Vec::new(),
            capacity,
            dirty: false,
        }
    }

    /// Point the materials at rebuilt texture arrays.
    pub fn set_assets(&self, materials: &mut Assets<InkMaterial>, assets: &InkAssets) {
        for handle in &self.materials {
            if let Some(mut material) = materials.get_mut(handle) {
                material.masks = assets.masks.clone();
                material.grains = assets.grains.clone();
            }
        }
    }

    /// The paper changed tone (theme switch): re-specialise the blends.
    pub fn set_paper(&self, materials: &mut Assets<InkMaterial>, paper: PaperTone) {
        for handle in &self.materials {
            if let Some(mut material) = materials.get_mut(handle) {
                material.paper = paper;
            }
        }
    }

    /// Reserve a slot for `style`; the upload happens at [`Self::flush`].
    pub fn insert(&mut self, style: &InkStyle, assets: &InkAssets) -> InkSlot {
        let style = StrokeStyle::new(style, assets);
        let combo = style.combo();
        let index = match self.free.pop() {
            Some(index) => {
                if let Some(slot) = usize::try_from(index)
                    .ok()
                    .and_then(|i| self.styles.get_mut(i))
                {
                    *slot = style;
                }
                index
            }
            None => {
                self.styles.push(style);
                u32::try_from(self.styles.len() - 1).unwrap_or(u32::MAX)
            }
        };
        self.dirty = true;
        InkSlot { index, combo }
    }

    /// Release a slot for reuse.
    pub fn remove(&mut self, slot: InkSlot) {
        self.free.push(slot.index);
    }

    /// The components an ink entity needs to draw with `slot`'s style.
    pub fn components(&self, slot: InkSlot) -> (MeshTag, MeshMaterial2d<InkMaterial>) {
        (
            MeshTag(slot.index),
            MeshMaterial2d(self.materials[slot.combo.index()].clone()),
        )
    }

    /// Upload pending style edits; call once per frame after the diff.
    pub fn flush(&mut self, images: &mut Assets<Image>, materials: &mut Assets<InkMaterial>) {
        if !self.dirty {
            return;
        }
        self.dirty = false;
        let needed = self.styles.len().max(1);
        if needed > self.capacity {
            self.capacity = needed.next_power_of_two();
        }
        let (height, data) = pack_styles(&self.styles, self.capacity);
        let same_size = images
            .get(&self.texture)
            .is_some_and(|image| image.height() == height);
        if same_size {
            // Same size: bevy writes the existing GPU texture in place.
            if let Some(mut image) = images.get_mut(&self.texture) {
                image.data = Some(data);
            }
        } else {
            // A taller texture is a new GPU resource, and the materials'
            // bind groups keep the old one: swap in a fresh asset so they
            // are rebuilt against it.
            self.texture = images.add(style_image(&self.styles, self.capacity));
            for handle in &self.materials {
                if let Some(mut material) = materials.get_mut(handle) {
                    material.styles = self.texture.clone();
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Bytes per `Rgba32Uint` texel.
    const TEXEL_BYTES: usize = 16;

    /// The four `u32` channels of texel `(x, y)` in packed style data.
    fn texel(data: &[u8], x: u32, y: u32) -> [u32; 4] {
        let index = usize::try_from(y * STYLE_TEXTURE_WIDTH + x).unwrap_or(usize::MAX);
        let start = index * TEXEL_BYTES;
        let bytes = &data[start..start + TEXEL_BYTES];
        std::array::from_fn(|c| {
            u32::from_le_bytes([
                bytes[c * 4],
                bytes[c * 4 + 1],
                bytes[c * 4 + 2],
                bytes[c * 4 + 3],
            ])
        })
    }

    fn sample(i: u32) -> StrokeStyle {
        let f = f32::from(u16::try_from(i).unwrap_or(u16::MAX));
        StrokeStyle {
            color: Vec4::new(f, 0.25, 0.5, 1.0),
            mask: Vec4::new(1.0, 0.125, 0.75, 0.0),
            grain: Vec4::new(3.5, 0.4, 0.0, 0.0),
            depth: -0.5,
            // Small integers are denormal as f32 bit patterns and all-ones
            // is a NaN: both must survive untouched.
            flags: StrokeStyle::MASK_EDGE | StrokeStyle::DISCARD | (i << 7),
            mask_layer: i,
            grain_layer: u32::MAX - i,
        }
    }

    #[test]
    fn style_texel_layout() {
        let styles: Vec<StrokeStyle> = (0..300).map(sample).collect();
        let (height, data) = pack_styles(&styles, 512);
        assert_eq!(height, 2);
        let width = usize::try_from(STYLE_TEXTURE_WIDTH).unwrap_or(usize::MAX);
        assert_eq!(data.len(), width * 2 * TEXEL_BYTES);

        for i in [0, 1, 255, 256, 257, 299] {
            let s = sample(i);
            let (x, y) = ((i % STYLES_PER_ROW) * 4, i / STYLES_PER_ROW);
            assert_eq!(texel(&data, x, y), s.color.to_array().map(f32::to_bits));
            assert_eq!(texel(&data, x + 1, y), s.mask.to_array().map(f32::to_bits));
            assert_eq!(texel(&data, x + 2, y), s.grain.to_array().map(f32::to_bits));
            let [depth, flags, mask_layer, grain_layer] = texel(&data, x + 3, y);
            assert_eq!(f32::from_bits(depth), s.depth);
            assert_eq!(flags, s.flags);
            assert_eq!(mask_layer, s.mask_layer);
            assert_eq!(grain_layer, s.grain_layer);
        }
        // Style 256 wraps to the start of row 1.
        assert_eq!(texel(&data, 0, 1)[0], 256.0f32.to_bits());
        // Past the last style everything is zero.
        assert_eq!(texel(&data, 300 % 256 * 4, 1), [0; 4]);
        assert_eq!(texel(&data, STYLE_TEXTURE_WIDTH - 1, 1), [0; 4]);
    }

    #[test]
    fn style_texture_height() {
        assert_eq!(pack_styles(&[], 0).0, 1);
        assert_eq!(pack_styles(&[], 64).0, 1);
        assert_eq!(pack_styles(&[], 256).0, 1);
        assert_eq!(pack_styles(&[], 257).0, 2);
        assert_eq!(pack_styles(&[], 1024).0, 4);
        // More styles than capacity still fit.
        let styles = vec![StrokeStyle::default(); 300];
        assert_eq!(pack_styles(&styles, 64).0, 2);
        let (height, data) = pack_styles(&[], 64);
        assert_eq!(height, 1);
        assert!(data.iter().all(|&b| b == 0));
    }
}
