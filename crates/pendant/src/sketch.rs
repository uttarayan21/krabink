//! Sketch rendering: each sketch in the open note gets an off-screen bevy
//! scene (own render layer + camera) rendered into an `Image`; that image is
//! exposed to egui through a custom `pendant://` texture loader so the
//! markdown preview embeds it inline.
//!
//! Committed CRDT strokes become ink meshes drawn with [`InkMaterial`];
//! wet ink from the ephemeral channel renders as provisional meshes on top
//! and is dropped once the authoritative stroke lands (or after a timeout).
//! Draw order is z order: committed element `k` sits at `k / 100`, remote
//! wet stroke `j` at `990 + j / 100`, and the ink pipeline writes depth.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use bevy::asset::RenderAssetUsages;
use bevy::camera::visibility::RenderLayers;
use bevy::camera::{ImageRenderTarget, RenderTarget, ScalingMode};
use bevy::mesh::{Indices, PrimitiveTopology};
use bevy::prelude::*;
use bevy::render::render_resource::TextureUsages;
use bevy::render::storage::ShaderBuffer;
use bevy_egui::{EguiContexts, EguiPrimaryContextPass, EguiTextureHandle, EguiUserTextures, egui};
use pendant_core::{
    BrushSpec, DEFAULT_TOLERANCE, DocKey, Element, Ink, InkStyle, Rgba, SKETCH_URI_PREFIX,
    SketchId, StrokeEnd, StrokeId, StrokePoint, Tool, WetInk,
};

use crate::docs::{Docs, now_ms};
use crate::ink_material::{
    ATTRIBUTE_INK_OPACITY, ATTRIBUTE_INK_UV, InkMaterial, InkMaterialPlugin, InkPalette, InkParams,
    InkSlot,
};
use crate::ui::EditorState;

/// Wet ink lingers this long after `End` if the committed stroke never shows.
const WET_TTL_MS: u64 = 5_000;
/// Render-target size bounds (pixels; 1 canvas unit = 1 pixel, which is
/// why [`DEFAULT_TOLERANCE`] is the right cap/join flattening tolerance).
const MIN_TARGET: u32 = 256;
const MAX_TARGET: u32 = 2048;
/// 1 canvas unit = 1 pixel: grain renders at full strength.
const ZOOM: f32 = 1.0;
/// z spacing between consecutive strokes.
const Z_STEP: f32 = 0.01;
/// Remote wet strokes sit above every committed element.
const WET_Z_BASE: f32 = 990.0;
/// Wet z slots wrap here so they stay under the camera's far plane.
const WET_Z_SLOTS: u16 = 1000;

/// An ephemeral frame relayed by the server (raised by the sync layer).
#[derive(Message)]
pub struct WetInkFrame {
    pub doc: DocKey,
    pub payload: Vec<u8>,
}

/// Texture table shared with the egui loader: uri → (texture, size).
#[derive(Resource, Clone, Default)]
pub struct SketchTextures(Arc<Mutex<HashMap<String, (egui::TextureId, egui::Vec2)>>>);

/// A spawned ink entity and the palette slot it draws with.
#[derive(Debug, Clone, Copy)]
struct InkEntity {
    entity: Entity,
    slot: InkSlot,
}

struct WetStroke {
    sketch: SketchId,
    /// Spawned on the first batch with drawable geometry.
    drawn: Option<InkEntity>,
    mesh: Option<Handle<Mesh>>,
    layer: usize,
    tool: Tool,
    color: Rgba,
    base_width: f32,
    /// A custom brush's spec from `Begin`; `None` draws the tool's preset.
    spec: Option<BrushSpec>,
    points: Vec<StrokePoint>,
    last_seq: u32,
    /// Set at `End`: drop at this deadline even without a commit.
    expires_ms: Option<u64>,
}

impl WetStroke {
    fn ink(&self) -> Ink<'_> {
        match &self.spec {
            Some(spec) => Ink {
                spec: std::borrow::Cow::Borrowed(spec),
                color: self.color,
                base_width: self.base_width,
                seed: 0,
            },
            None => Ink::preset(self.tool, self.color, self.base_width),
        }
    }
}

/// The off-screen image a sketch renders into and the camera drawing it.
struct SceneTarget {
    image: Handle<Image>,
    camera: Entity,
    size: UVec2,
}

struct SketchScene {
    layer: usize,
    target: SceneTarget,
    palette: InkPalette,
    /// Committed element id → ink entity (`None` for elements with no ink).
    strokes: HashMap<StrokeId, Option<InkEntity>>,
    /// Committed elements spawned so far; the next one's z slot.
    committed: u16,
    /// Next remote wet stroke's z slot.
    wet_serial: u16,
}

#[derive(Resource, Default)]
struct SketchScenes {
    scenes: HashMap<SketchId, SketchScene>,
    wet: HashMap<StrokeId, WetStroke>,
    next_layer: usize,
}

impl SketchScenes {
    /// Drop a wet stroke's entity and free its style slot.
    fn despawn_wet(&mut self, commands: &mut Commands, id: StrokeId) {
        let Some(wet) = self.wet.remove(&id) else {
            return;
        };
        despawn_wet(commands, &mut self.scenes, wet);
    }
}

fn despawn_wet(
    commands: &mut Commands,
    scenes: &mut HashMap<SketchId, SketchScene>,
    wet: WetStroke,
) {
    if let Some(drawn) = wet.drawn {
        commands.entity(drawn.entity).despawn();
        if let Some(scene) = scenes.get_mut(&wet.sketch) {
            scene.palette.remove(drawn.slot);
        }
    }
}

/// Receive-side latency of wet-ink batches (sender clock → local clock).
#[derive(Resource, Default)]
struct WetLatency {
    samples_ms: Vec<f64>,
}

pub struct SketchPlugin;

impl Plugin for SketchPlugin {
    fn build(&self, app: &mut App) {
        app.add_plugins(InkMaterialPlugin)
            .add_message::<WetInkFrame>()
            .init_resource::<SketchScenes>()
            .init_resource::<SketchTextures>()
            .init_resource::<WetLatency>()
            .add_systems(
                Update,
                (sync_sketch_scenes, apply_wet_ink, flush_palettes).chain(),
            )
            .add_systems(EguiPrimaryContextPass, install_loader);
    }
}

// ---- egui loader ----

struct PendantTextureLoader {
    textures: SketchTextures,
}

impl egui::load::TextureLoader for PendantTextureLoader {
    fn id(&self) -> &'static str {
        "pendant-sketch-loader"
    }

    fn load(
        &self,
        _ctx: &egui::Context,
        uri: &str,
        _texture_options: egui::TextureOptions,
        _size_hint: egui::SizeHint,
    ) -> egui::load::TextureLoadResult {
        if !uri.starts_with(SKETCH_URI_PREFIX) {
            return Err(egui::load::LoadError::NotSupported);
        }
        let table = self.textures.0.lock().expect("sketch texture table");
        match table.get(uri) {
            Some((id, size)) => Ok(egui::load::TexturePoll::Ready {
                texture: egui::load::SizedTexture::new(*id, *size),
            }),
            None => Ok(egui::load::TexturePoll::Pending { size: None }),
        }
    }

    fn forget(&self, _uri: &str) {}
    fn forget_all(&self) {}
    fn byte_size(&self) -> usize {
        0
    }
}

fn install_loader(
    mut contexts: EguiContexts,
    textures: Res<SketchTextures>,
    mut installed: Local<bool>,
) -> Result {
    if !*installed {
        contexts
            .ctx_mut()?
            .add_texture_loader(Arc::new(PendantTextureLoader {
                textures: textures.clone(),
            }));
        *installed = true;
    }
    Ok(())
}

// ---- scene / stroke sync ----

#[expect(
    clippy::too_many_arguments,
    reason = "bevy system; each param is a distinct ECS resource"
)]
fn sync_sketch_scenes(
    mut commands: Commands,
    docs: Res<Docs>,
    editor: Res<EditorState>,
    mut scenes: ResMut<SketchScenes>,
    textures: Res<SketchTextures>,
    mut images: ResMut<Assets<Image>>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<InkMaterial>>,
    mut buffers: ResMut<Assets<ShaderBuffer>>,
    mut egui_textures: ResMut<EguiUserTextures>,
) {
    let Some(note_id) = editor.open else { return };
    let Some(note) = docs.note(note_id) else {
        return;
    };

    for sketch in note.sketch_ids() {
        let elements = match note.elements(sketch) {
            Ok(elements) => elements,
            Err(err) => {
                tracing::error!(%err, %sketch, "reading elements failed");
                continue;
            }
        };
        // Strokes and shapes alike render their outline.
        let outlines: Vec<(&Element, Vec<StrokePoint>)> =
            elements.iter().map(|el| (el, el.outline())).collect();

        // Content bounds decide the render-target size.
        let max = outlines
            .iter()
            .flat_map(|(_, pts)| pts)
            .fold((0.0f32, 0.0f32), |(mx, my), p| (mx.max(p.x), my.max(p.y)));
        let desired = UVec2::new(target_extent(max.0), target_extent(max.1));

        let scenes = &mut *scenes;
        if !scenes.scenes.contains_key(&sketch) {
            scenes.next_layer += 1; // layer 0 = main window
            let layer = scenes.next_layer;
            let target = new_target(
                &mut commands,
                &mut images,
                &mut egui_textures,
                &textures,
                sketch,
                layer,
                desired,
            );
            let palette = InkPalette::new(&mut buffers, &mut materials, InkParams::new(ZOOM));
            scenes.scenes.insert(
                sketch,
                SketchScene {
                    layer,
                    target,
                    palette,
                    strokes: HashMap::new(),
                    committed: 0,
                    wet_serial: 0,
                },
            );
        }
        let scene = scenes.scenes.get_mut(&sketch).expect("inserted above");

        if scene.target.size != desired {
            commands.entity(scene.target.camera).despawn();
            egui_textures.remove_image(scene.target.image.id());
            scene.target = new_target(
                &mut commands,
                &mut images,
                &mut egui_textures,
                &textures,
                sketch,
                scene.layer,
                desired,
            );
        }

        // Diff committed elements.
        let mut stale: HashMap<_, _> = scene.strokes.clone();
        for (element, outline) in &outlines {
            let id = element.id();
            if stale.remove(&id).is_some() {
                continue;
            }
            let drawn =
                ink_mesh(&element.ink(), outline, StrokeEnd::Complete).map(|(mesh, style)| {
                    let slot = scene.palette.insert(style);
                    let z = committed_z(scene.committed);
                    scene.committed = scene.committed.saturating_add(1);
                    let (tag, material) = scene.palette.components(slot);
                    let entity = commands
                        .spawn((
                            Mesh2d(meshes.add(mesh)),
                            material,
                            tag,
                            Transform::from_xyz(0.0, 0.0, z),
                            RenderLayers::layer(scene.layer),
                        ))
                        .id();
                    InkEntity { entity, slot }
                });
            scene.strokes.insert(id, drawn);
            // A committed element (stroke or snapped shape) replaces its
            // wet-ink preview.
            if let Some(wet) = scenes.wet.remove(&id)
                && let Some(drawn) = wet.drawn
            {
                commands.entity(drawn.entity).despawn();
                scene.palette.remove(drawn.slot);
            }
        }
        for (id, drawn) in stale {
            if let Some(drawn) = drawn {
                commands.entity(drawn.entity).despawn();
                scene.palette.remove(drawn.slot);
            }
            scene.strokes.remove(&id);
        }
    }
}

/// Upload every sketch's style edits after the frame's diffs.
fn flush_palettes(
    mut scenes: ResMut<SketchScenes>,
    mut materials: ResMut<Assets<InkMaterial>>,
    mut buffers: ResMut<Assets<ShaderBuffer>>,
) {
    scenes
        .scenes
        .values_mut()
        .for_each(|scene| scene.palette.flush(&mut buffers, &mut materials));
}

/// z of the `k`th committed element; saturates far below the wet band.
fn committed_z(k: u16) -> f32 {
    f32::from(k) * Z_STEP
}

/// z of the `j`th remote wet stroke, above every committed element.
fn wet_z(j: u16) -> f32 {
    WET_Z_BASE + f32::from(j) * Z_STEP
}

fn new_target(
    commands: &mut Commands,
    images: &mut Assets<Image>,
    egui_textures: &mut EguiUserTextures,
    table: &SketchTextures,
    sketch: SketchId,
    layer: usize,
    size: UVec2,
) -> SceneTarget {
    let mut image = Image::new_fill(
        bevy::render::render_resource::Extent3d {
            width: size.x,
            height: size.y,
            depth_or_array_layers: 1,
        },
        bevy::render::render_resource::TextureDimension::D2,
        &[255, 255, 255, 255],
        bevy::render::render_resource::TextureFormat::Bgra8UnormSrgb,
        RenderAssetUsages::default(),
    );
    image.texture_descriptor.usage |= TextureUsages::RENDER_ATTACHMENT;
    let image = images.add(image);

    let texture_id = egui_textures.add_image(EguiTextureHandle::Strong(image.clone()));
    table.0.lock().expect("sketch texture table").insert(
        format!("{SKETCH_URI_PREFIX}{sketch}"),
        (texture_id, egui::Vec2::new(size.x as f32, size.y as f32)),
    );

    // Canvas y grows down; meshes negate y, so the visible rect is
    // [0, w] x [-h, 0], centred below.
    let camera = commands
        .spawn((
            Camera2d,
            Camera {
                clear_color: ClearColorConfig::Custom(Color::WHITE),
                order: -1,
                ..default()
            },
            RenderTarget::Image(ImageRenderTarget::from(image.clone())),
            // Pinned, not left to bevy's default: the iPad's Metal view
            // samples 4x too, so both platforms antialias ink edges alike.
            Msaa::Sample4,
            Projection::Orthographic(OrthographicProjection {
                scaling_mode: ScalingMode::Fixed {
                    width: size.x as f32,
                    height: size.y as f32,
                },
                ..OrthographicProjection::default_2d()
            }),
            Transform::from_xyz(size.x as f32 / 2.0, -(size.y as f32) / 2.0, 0.0),
            RenderLayers::layer(layer),
        ))
        .id();

    SceneTarget {
        image,
        camera,
        size,
    }
}

fn target_extent(content_max: f32) -> u32 {
    let padded = (content_max + 64.0).ceil() as u32;
    padded.next_multiple_of(64).clamp(MIN_TARGET, MAX_TARGET)
}

/// Canvas-space stroke mesh → bevy mesh (y flipped into bevy's y-up space)
/// plus the style it draws with.
/// `None` when there is nothing to draw: bevy's mesh allocator never
/// allocates a zero-vertex mesh but still tries to upload it, logging a
/// "Use-after-free" error every frame the mesh is extracted.
fn ink_mesh(ink: &Ink<'_>, points: &[StrokePoint], end: StrokeEnd) -> Option<(Mesh, InkStyle)> {
    let ink = ink.mesh(points, end, DEFAULT_TOLERANCE);
    if ink.is_empty() {
        return None;
    }
    let positions: Vec<[f32; 3]> = ink
        .vertices
        .iter()
        .map(|v| [v.pos[0], -v.pos[1], 0.0])
        .collect();
    let uvs: Vec<[f32; 2]> = ink.vertices.iter().map(|v| v.uv).collect();
    let opacity: Vec<f32> = ink.vertices.iter().map(|v| v.opacity).collect();
    let mesh = Mesh::new(
        PrimitiveTopology::TriangleList,
        RenderAssetUsages::default(),
    )
    .with_inserted_attribute(Mesh::ATTRIBUTE_POSITION, positions)
    .with_inserted_attribute(ATTRIBUTE_INK_UV, uvs)
    .with_inserted_attribute(ATTRIBUTE_INK_OPACITY, opacity)
    .with_inserted_indices(Indices::U32(ink.indices));
    Some((mesh, ink.style))
}

// ---- wet ink ----

fn apply_wet_ink(
    mut commands: Commands,
    mut frames: MessageReader<WetInkFrame>,
    editor: Res<EditorState>,
    mut scenes: ResMut<SketchScenes>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut latency: ResMut<WetLatency>,
) {
    let open_doc = editor.open.map(DocKey::from);

    for frame in frames.read() {
        if Some(frame.doc) != open_doc {
            continue; // wet ink only matters for the note on screen
        }
        let msg = match WetInk::decode(&frame.payload) {
            Ok(msg) => msg,
            Err(err) => {
                tracing::warn!(%err, "undecodable wet-ink frame");
                continue;
            }
        };
        match msg {
            WetInk::Begin {
                sketch,
                stroke,
                tool,
                color,
                base_width,
                spec,
            } => {
                let spec = spec.and_then(|bytes| match BrushSpec::decode(&bytes) {
                    Ok(spec) => Some(spec),
                    Err(err) => {
                        tracing::warn!(%err, "unreadable wet-ink brush spec; using the preset");
                        None
                    }
                });
                let Some(layer) = scenes.scenes.get(&sketch).map(|s| s.layer) else {
                    continue; // sketch not on screen yet; CRDT commit will cover it
                };
                scenes.despawn_wet(&mut commands, stroke);
                scenes.wet.insert(
                    stroke,
                    WetStroke {
                        sketch,
                        drawn: None,
                        mesh: None,
                        layer,
                        tool,
                        color,
                        base_width,
                        spec,
                        points: Vec::new(),
                        last_seq: 0,
                        expires_ms: None,
                    },
                );
            }
            WetInk::Points {
                stroke,
                seq,
                sent_ms,
                ..
            } => {
                latency.samples_ms.push(now_ms() as f64 - sent_ms as f64);
                let scenes = &mut *scenes;
                let Some(wet) = scenes.wet.get_mut(&stroke) else {
                    continue; // joined mid-stroke; wait for the commit
                };
                if seq <= wet.last_seq && wet.last_seq != 0 {
                    continue;
                }
                wet.last_seq = seq;
                match msg.decode_points() {
                    Ok(points) => wet.points.extend(points),
                    Err(err) => {
                        tracing::warn!(%err, "undecodable wet-ink points");
                        continue;
                    }
                }
                let ink = wet.ink();
                let Some((mesh, style)) = ink_mesh(&ink, &wet.points, StrokeEnd::Live) else {
                    continue; // nothing drawable yet
                };
                match &wet.mesh {
                    Some(handle) => {
                        if let Err(err) = meshes.insert(handle, mesh) {
                            tracing::error!(%err, "wet-ink mesh update failed");
                        }
                    }
                    None => {
                        let Some(scene) = scenes.scenes.get_mut(&wet.sketch) else {
                            continue; // sketch went away; the commit will cover it
                        };
                        let slot = scene.palette.insert(style);
                        let z = wet_z(scene.wet_serial);
                        scene.wet_serial = (scene.wet_serial + 1) % WET_Z_SLOTS;
                        let (tag, material) = scene.palette.components(slot);
                        let handle = meshes.add(mesh);
                        let entity = commands
                            .spawn((
                                Mesh2d(handle.clone()),
                                material,
                                tag,
                                Transform::from_xyz(0.0, 0.0, z),
                                RenderLayers::layer(wet.layer),
                            ))
                            .id();
                        wet.mesh = Some(handle);
                        wet.drawn = Some(InkEntity { entity, slot });
                    }
                }
            }
            WetInk::End {
                stroke, sent_ms, ..
            } => {
                latency.samples_ms.push(now_ms() as f64 - sent_ms as f64);
                if let Some(wet) = scenes.wet.get_mut(&stroke) {
                    wet.expires_ms = Some(now_ms() + WET_TTL_MS);
                    // The landing tail completes the stroke; redraw it
                    // finished so the commit lands without a change.
                    if let Ok(tail) = msg.decode_points() {
                        wet.points.extend(tail);
                    }
                    let ink = wet.ink();
                    if let (Some(handle), Some((mesh, _))) =
                        (&wet.mesh, ink_mesh(&ink, &wet.points, StrokeEnd::Complete))
                        && let Err(err) = meshes.insert(handle, mesh)
                    {
                        tracing::error!(%err, "wet-ink mesh update failed");
                    }
                }
                report_latency(&mut latency);
            }
            // No stroke is coming (ruler drag, tool fiddling): drop it now.
            WetInk::Cancel { stroke } => scenes.despawn_wet(&mut commands, stroke),
        }
    }

    // Expire ended wet strokes whose commit never arrived.
    let now = now_ms();
    let expired: Vec<StrokeId> = scenes
        .wet
        .iter()
        .filter(|(_, w)| w.expires_ms.is_some_and(|at| at <= now))
        .map(|(id, _)| *id)
        .collect();
    for id in expired {
        scenes.despawn_wet(&mut commands, id);
    }
}

fn report_latency(latency: &mut WetLatency) {
    if latency.samples_ms.is_empty() {
        return;
    }
    let mut sorted = latency.samples_ms.clone();
    sorted.sort_by(f64::total_cmp);
    let pick = |q: f64| sorted[((sorted.len() - 1) as f64 * q) as usize];
    tracing::info!(
        samples = sorted.len(),
        p50_ms = pick(0.5),
        p95_ms = pick(0.95),
        max_ms = sorted.last().copied().unwrap_or(0.0),
        "wet-ink receive latency (sender clock -> local clock)"
    );
}
