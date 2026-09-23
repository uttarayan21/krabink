//! Image assets for the ink shader: the tip masks and paper grains a
//! brush can sample, as two `r8` texture arrays (masks clamped, grains
//! repeating, both mipmapped) built from the bundled PNGs plus whatever
//! the workspace's `assets` map holds. Strokes address a layer by asset
//! id through [`InkAssets::mask_layer`] / [`InkAssets::grain_layer`]; an
//! asset the array lacks falls back to the shape mask or value noise, and
//! a change of the set bumps [`InkAssets::generation`] so scenes redraw
//! their strokes with the new layers.

use std::collections::HashMap;

use bevy::asset::RenderAssetUsages;
use bevy::image::{ImageAddressMode, ImageFilterMode, ImageSampler, ImageSamplerDescriptor};
use bevy::prelude::*;
use bevy::render::render_resource::{
    Extent3d, TextureDataOrder, TextureDimension, TextureFormat, TextureViewDescriptor,
    TextureViewDimension,
};
use krabink_core::{Asset, AssetId, AssetKind, BrushSpec};

use crate::docs::Docs;

/// Side of every mask layer, texels.
const MASK_SIZE: u32 = 256;
/// Side of every grain layer, texels.
const GRAIN_SIZE: u32 = 512;

#[derive(Resource)]
pub struct InkAssets {
    pub masks: Handle<Image>,
    pub grains: Handle<Image>,
    mask_layers: HashMap<AssetId, u32>,
    grain_layers: HashMap<AssetId, u32>,
    /// The workspace asset ids the arrays were built from.
    workspace_ids: Vec<AssetId>,
    /// Bumped whenever the arrays are rebuilt.
    pub generation: u32,
}

impl InkAssets {
    /// The bundled assets only; the workspace's join through
    /// [`sync_assets`].
    pub fn new(images: &mut Assets<Image>) -> Self {
        let mut out = Self {
            masks: Handle::default(),
            grains: Handle::default(),
            mask_layers: HashMap::new(),
            grain_layers: HashMap::new(),
            workspace_ids: Vec::new(),
            generation: 0,
        };
        out.rebuild(images, &BrushSpec::builtin_assets());
        out
    }

    pub fn mask_layer(&self, id: &AssetId) -> Option<u32> {
        self.mask_layers.get(id).copied()
    }

    pub fn grain_layer(&self, id: &AssetId) -> Option<u32> {
        self.grain_layers.get(id).copied()
    }

    fn rebuild(&mut self, images: &mut Assets<Image>, assets: &[Asset]) {
        let mut masks = Vec::new();
        let mut grains = Vec::new();
        self.mask_layers.clear();
        self.grain_layers.clear();
        for asset in assets {
            let (layers, table, size) = match asset.kind {
                AssetKind::Mask => (&mut masks, &mut self.mask_layers, MASK_SIZE),
                AssetKind::Grain => (&mut grains, &mut self.grain_layers, GRAIN_SIZE),
            };
            match decode_luma(&asset.png, size) {
                Ok(texels) => {
                    table.insert(asset.id.clone(), u32::try_from(layers.len()).unwrap_or(0));
                    layers.push(texels);
                }
                Err(err) => tracing::warn!(%err, asset = %asset.id, "undecodable ink asset"),
            }
        }
        self.masks = images.add(array_image(masks, MASK_SIZE, false));
        self.grains = images.add(array_image(grains, GRAIN_SIZE, true));
        self.generation = self.generation.wrapping_add(1);
    }
}

/// Decode a greyscale PNG and resample it to `size`².
fn decode_luma(png: &[u8], size: u32) -> Result<Vec<u8>, image::ImageError> {
    let decoded = image::load_from_memory_with_format(png, image::ImageFormat::Png)?;
    let luma = decoded.to_luma8();
    let resized = if luma.width() == size && luma.height() == size {
        luma
    } else {
        image::imageops::resize(&luma, size, size, image::imageops::FilterType::Triangle)
    };
    Ok(resized.into_raw())
}

/// One `r8` array texture with a full mip chain (box filtered on the CPU).
/// An empty set still yields one blank layer so the binding is valid.
fn array_image(mut layers: Vec<Vec<u8>>, size: u32, repeat: bool) -> Image {
    if layers.is_empty() {
        layers.push(vec![255; (size * size) as usize]);
    }
    let mip_count = 32 - size.leading_zeros();
    // wgpu lays array textures out mip-major: every layer's level 0, then
    // every layer's level 1, and so on.
    let mut data = Vec::new();
    let mut current: Vec<Vec<u8>> = layers;
    let mut side = size;
    for _ in 0..mip_count {
        for layer in &current {
            data.extend_from_slice(layer);
        }
        if side == 1 {
            break;
        }
        current = current.iter().map(|l| halve(l, side)).collect();
        side /= 2;
    }
    // `Image::new` expects level-0 data only; the mip chain goes in by
    // hand, mip-major as laid out above.
    let mut image = Image::new_uninit(
        Extent3d {
            width: size,
            height: size,
            depth_or_array_layers: u32::try_from(current.len()).unwrap_or(1),
        },
        TextureDimension::D2,
        TextureFormat::R8Unorm,
        RenderAssetUsages::RENDER_WORLD,
    );
    image.data = Some(data);
    image.data_order = TextureDataOrder::MipMajor;
    image.texture_descriptor.mip_level_count = mip_count;
    // One layer would default to a plain 2D view, which the shader's
    // `texture_2d_array` binding rejects.
    image.texture_view_descriptor = Some(TextureViewDescriptor {
        dimension: Some(TextureViewDimension::D2Array),
        ..TextureViewDescriptor::default()
    });
    let address = if repeat {
        ImageAddressMode::Repeat
    } else {
        ImageAddressMode::ClampToEdge
    };
    image.sampler = ImageSampler::Descriptor(ImageSamplerDescriptor {
        address_mode_u: address,
        address_mode_v: address,
        mag_filter: ImageFilterMode::Linear,
        min_filter: ImageFilterMode::Linear,
        mipmap_filter: ImageFilterMode::Linear,
        ..ImageSamplerDescriptor::default()
    });
    image
}

/// The next mip level of a square `r8` layer: each texel the mean of 2×2.
fn halve(texels: &[u8], side: u32) -> Vec<u8> {
    let half = (side / 2) as usize;
    let side = side as usize;
    let mut out = Vec::with_capacity(half * half);
    for y in 0..half {
        for x in 0..half {
            let at = |dx: usize, dy: usize| u32::from(texels[(2 * y + dy) * side + 2 * x + dx]);
            out.push(((at(0, 0) + at(1, 0) + at(0, 1) + at(1, 1) + 2) / 4) as u8);
        }
    }
    out
}

/// Rebuild the arrays when the workspace's asset set changes.
pub fn sync_assets(
    docs: Res<Docs>,
    mut assets: ResMut<InkAssets>,
    mut images: ResMut<Assets<Image>>,
) {
    let ids = docs.workspace.asset_ids();
    if ids == assets.workspace_ids {
        return;
    }
    let mut all = BrushSpec::builtin_assets();
    all.extend(docs.workspace.assets().into_iter().map(|a| a.asset));
    assets.rebuild(&mut images, &all);
    assets.workspace_ids = ids;
    tracing::info!(assets = all.len(), "ink assets rebuilt");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn halving_averages_quads() {
        let side = 4;
        let texels: Vec<u8> = (0..16).map(|i| (i * 16) as u8).collect();
        let half = halve(&texels, side);
        assert_eq!(half.len(), 4);
        assert_eq!(half[0], ((16 + 64 + 80 + 2) / 4) as u8);
    }

    #[test]
    fn bundled_assets_decode_to_their_sizes() {
        for asset in BrushSpec::builtin_assets() {
            let size = match asset.kind {
                AssetKind::Mask => MASK_SIZE,
                AssetKind::Grain => GRAIN_SIZE,
            };
            let texels = decode_luma(&asset.png, size).unwrap();
            assert_eq!(texels.len(), (size * size) as usize);
        }
    }
}
