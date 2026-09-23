//! The ink material: one [`Material2d`] for every brush, the desktop twin
//! of the Metal pipeline in `ios/Krabink/Sources/InkRenderer.swift`.
//!
//! Every stroke entity carries a [`MeshTag`] indexing a per-sketch
//! [`StrokeStyle`] array on the GPU ([`InkPalette`]); `ink.wgsl` reads the
//! stroke's colour, edge mask and grain from it. The only pipeline splits
//! are the blend function (Normal/Multiply) and the depth compare that
//! implements write-once ink (Overlap::Discard), keyed by [`InkCombo`], so
//! a sketch needs four materials sharing one style buffer.
//!
//! Depth: bevy's 2D pass uses reversed-Z cleared to 0, so a later stroke
//! (higher z) passes `GreaterEqual` over an earlier one. A Discard stroke
//! uses `Greater` and writes depth: where it covers itself the second
//! fragment sits at the same z and fails.
//!
//! Image masks (kind 2) and grains (kind 2) sample the two texture arrays
//! of [`crate::ink_assets::InkAssets`]; a style names its layer through
//! `mask_layer` / `grain_layer`.

use bevy::asset::{AssetPath, embedded_asset, embedded_path};
use bevy::mesh::{MeshTag, MeshVertexAttribute, MeshVertexBufferLayoutRef, VertexFormat};
use bevy::prelude::*;
use bevy::render::render_resource::{
    AsBindGroup, BlendComponent, BlendFactor, BlendOperation, BlendState, CompareFunction,
    RenderPipelineDescriptor, ShaderType, SpecializedMeshPipelineError,
};
use bevy::render::storage::ShaderBuffer;
use bevy::shader::ShaderRef;
use bevy::sprite_render::{AlphaMode2d, Material2d, Material2dKey, Material2dPlugin};
use krabink_core::{Blend, GrainMapping, InkStyle, MaskStyle, Overlap, Rgba};

use crate::ink_assets::InkAssets;

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
/// same on both platforms.
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

    fn blend_state(self) -> BlendState {
        if self.multiply && crate::theme::PAPER_IS_DARK {
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

impl From<&InkMaterial> for InkCombo {
    fn from(material: &InkMaterial) -> Self {
        material.combo
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
#[bind_group_data(InkCombo)]
pub struct InkMaterial {
    /// `array<StrokeStyle>`, indexed by each entity's [`MeshTag`].
    #[storage(0, read_only)]
    pub styles: Handle<ShaderBuffer>,
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
    /// Blend and depth state; the specialisation key.
    pub combo: InkCombo,
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
        let combo = key.bind_group_data;
        if let Some(fragment) = descriptor.fragment.as_mut() {
            fragment
                .targets
                .iter_mut()
                .flatten()
                .for_each(|target| target.blend = Some(combo.blend_state()));
        }
        // The transparent 2D pipeline disables depth writes; ink relies on
        // them for draw order and write-once overlap.
        if let Some(depth) = descriptor.depth_stencil.as_mut() {
            depth.depth_write_enabled = Some(true);
            depth.depth_compare = Some(combo.depth_compare());
        }
        descriptor.label = Some(
            format!(
                "ink_pipeline_{}_{}",
                if combo.multiply { "multiply" } else { "normal" },
                if combo.discard {
                    "discard"
                } else {
                    "accumulate"
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

/// A sketch's stroke styles: the GPU array every ink entity indexes with
/// its [`MeshTag`], and the four materials (one per [`InkCombo`]) sharing
/// it. Slots are reused through a free list so an entity's tag stays put
/// for its lifetime.
pub struct InkPalette {
    buffer: Handle<ShaderBuffer>,
    materials: [Handle<InkMaterial>; 4],
    styles: Vec<StrokeStyle>,
    free: Vec<u32>,
    /// Styles uploaded per flush; grows geometrically so the GPU buffer is
    /// rewritten in place rather than reallocated on most edits.
    capacity: usize,
    dirty: bool,
}

impl InkPalette {
    /// Slots a fresh palette holds before its first reallocation.
    const INITIAL_CAPACITY: usize = 64;

    pub fn new(
        buffers: &mut Assets<ShaderBuffer>,
        materials: &mut Assets<InkMaterial>,
        params: InkParams,
        assets: &InkAssets,
    ) -> Self {
        let capacity = Self::INITIAL_CAPACITY;
        let mut buffer = ShaderBuffer::default();
        buffer.set_data(vec![StrokeStyle::default(); capacity]);
        let buffer = buffers.add(buffer);
        let materials = InkCombo::ALL.map(|combo| {
            materials.add(InkMaterial {
                styles: buffer.clone(),
                params,
                masks: assets.masks.clone(),
                grains: assets.grains.clone(),
                combo,
            })
        });
        Self {
            buffer,
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
    pub fn flush(
        &mut self,
        buffers: &mut Assets<ShaderBuffer>,
        materials: &mut Assets<InkMaterial>,
    ) {
        if !self.dirty {
            return;
        }
        self.dirty = false;
        let needed = self.styles.len().max(1);
        let mut padded = self.styles.clone();
        if needed > self.capacity {
            // A resized buffer is a new GPU resource, and the materials'
            // bind groups keep the old one: swap in a fresh asset so they
            // are rebuilt against it.
            self.capacity = needed.next_power_of_two();
            padded.resize(self.capacity, StrokeStyle::default());
            let mut buffer = ShaderBuffer::default();
            buffer.set_data(padded);
            self.buffer = buffers.add(buffer);
            for handle in &self.materials {
                if let Some(mut material) = materials.get_mut(handle) {
                    material.styles = self.buffer.clone();
                }
            }
        } else if let Some(mut buffer) = buffers.get_mut(&self.buffer) {
            // Same size: bevy writes the existing GPU buffer in place.
            padded.resize(self.capacity, StrokeStyle::default());
            buffer.set_data(padded);
        }
    }
}
