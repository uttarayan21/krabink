//! Sketch rendering: each sketch in the open note gets an off-screen bevy
//! scene (own render layer + camera) rendered into an `Image`; that image is
//! exposed to egui through a custom `pendant://` texture loader so the
//! markdown preview embeds it inline.
//!
//! Committed CRDT strokes become ink meshes; wet ink from the ephemeral
//! channel renders as provisional meshes on top and is dropped once the
//! authoritative stroke lands (or after a timeout).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use bevy::asset::RenderAssetUsages;
use bevy::camera::visibility::RenderLayers;
use bevy::camera::{ImageRenderTarget, RenderTarget, ScalingMode};
use bevy::mesh::{Indices, PrimitiveTopology};
use bevy::prelude::*;
use bevy::render::render_resource::TextureUsages;
use bevy_egui::{EguiContexts, EguiPrimaryContextPass, EguiTextureHandle, EguiUserTextures, egui};
use pendant_core::{
    DEFAULT_TOLERANCE, DocKey, Element, Ink, Rgba, SKETCH_URI_PREFIX, SketchId, StrokeEnd,
    StrokeId, StrokePoint, Tool, WetInk,
};

use crate::docs::{Docs, now_ms};
use crate::ui::EditorState;

/// Wet ink lingers this long after `End` if the committed stroke never shows.
const WET_TTL_MS: u64 = 5_000;
/// Render-target size bounds (pixels; 1 canvas unit = 1 pixel, which is
/// why [`DEFAULT_TOLERANCE`] is the right cap/join flattening tolerance).
const MIN_TARGET: u32 = 256;
const MAX_TARGET: u32 = 2048;

/// An ephemeral frame relayed by the server (raised by the sync layer).
#[derive(Message)]
pub struct WetInkFrame {
    pub doc: DocKey,
    pub payload: Vec<u8>,
}

/// Texture table shared with the egui loader: uri → (texture, size).
#[derive(Resource, Clone, Default)]
pub struct SketchTextures(Arc<Mutex<HashMap<String, (egui::TextureId, egui::Vec2)>>>);

struct WetStroke {
    /// Spawned on the first batch with drawable geometry.
    entity: Option<Entity>,
    mesh: Option<Handle<Mesh>>,
    layer: usize,
    tool: Tool,
    color: Rgba,
    base_width: f32,
    points: Vec<StrokePoint>,
    last_seq: u32,
    /// Set at `End`: drop at this deadline even without a commit.
    expires_ms: Option<u64>,
}

struct SketchScene {
    layer: usize,
    image: Handle<Image>,
    camera: Entity,
    size: UVec2,
    /// Committed element id → mesh entity (`None` for elements with no ink).
    strokes: HashMap<StrokeId, Option<Entity>>,
}

#[derive(Resource, Default)]
struct SketchScenes {
    scenes: HashMap<SketchId, SketchScene>,
    wet: HashMap<StrokeId, WetStroke>,
    next_layer: usize,
}

/// Receive-side latency of wet-ink batches (sender clock → local clock).
#[derive(Resource, Default)]
struct WetLatency {
    samples_ms: Vec<f64>,
}

pub struct SketchPlugin;

impl Plugin for SketchPlugin {
    fn build(&self, app: &mut App) {
        app.add_message::<WetInkFrame>()
            .init_resource::<SketchScenes>()
            .init_resource::<SketchTextures>()
            .init_resource::<WetLatency>()
            .add_systems(Update, (sync_sketch_scenes, apply_wet_ink).chain())
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
    mut materials: ResMut<Assets<ColorMaterial>>,
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
            let scene = new_scene(
                &mut commands,
                &mut images,
                &mut egui_textures,
                &textures,
                sketch,
                scenes.next_layer,
                desired,
            );
            scenes.scenes.insert(sketch, scene);
        }
        let scene = scenes.scenes.get_mut(&sketch).expect("inserted above");

        if scene.size != desired {
            let replacement = new_scene(
                &mut commands,
                &mut images,
                &mut egui_textures,
                &textures,
                sketch,
                scene.layer,
                desired,
            );
            commands.entity(scene.camera).despawn();
            egui_textures.remove_image(scene.image.id());
            let strokes_kept = std::mem::take(&mut scene.strokes);
            *scene = SketchScene {
                strokes: strokes_kept,
                ..replacement
            };
        }

        // Diff committed elements.
        let mut stale: HashMap<_, _> = scene.strokes.clone();
        for (z, (element, outline)) in outlines.iter().enumerate() {
            let id = element.id();
            if stale.remove(&id).is_some() {
                continue;
            }
            let entity = ink_mesh(&element.ink(), outline, StrokeEnd::Complete).map(|mesh| {
                commands
                    .spawn((
                        Mesh2d(meshes.add(mesh)),
                        MeshMaterial2d(materials.add(color_of(element.color()))),
                        Transform::from_xyz(0.0, 0.0, z as f32 * 0.01),
                        RenderLayers::layer(scene.layer),
                    ))
                    .id()
            });
            scene.strokes.insert(id, entity);
            // A committed element (stroke or snapped shape) replaces its
            // wet-ink preview.
            if let Some(wet) = scenes.wet.remove(&id)
                && let Some(entity) = wet.entity
            {
                commands.entity(entity).despawn();
            }
        }
        for (id, entity) in stale {
            if let Some(entity) = entity {
                commands.entity(entity).despawn();
            }
            scene.strokes.remove(&id);
        }
    }
}

fn new_scene(
    commands: &mut Commands,
    images: &mut Assets<Image>,
    egui_textures: &mut EguiUserTextures,
    table: &SketchTextures,
    sketch: SketchId,
    layer: usize,
    size: UVec2,
) -> SketchScene {
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

    SketchScene {
        layer,
        image,
        camera,
        size,
        strokes: HashMap::new(),
    }
}

fn target_extent(content_max: f32) -> u32 {
    let padded = (content_max + 64.0).ceil() as u32;
    padded.next_multiple_of(64).clamp(MIN_TARGET, MAX_TARGET)
}

fn color_of(rgba: Rgba) -> ColorMaterial {
    let [r, g, b, a] = rgba.0;
    ColorMaterial::from(Color::srgba_u8(r, g, b, a))
}

/// Canvas-space stroke mesh → bevy mesh (y flipped into bevy's y-up space).
/// `None` when there is nothing to draw: bevy's mesh allocator never
/// allocates a zero-vertex mesh but still tries to upload it, logging a
/// "Use-after-free" error every frame the mesh is extracted.
fn ink_mesh(ink: &Ink<'_>, points: &[StrokePoint], end: StrokeEnd) -> Option<Mesh> {
    let ink = ink.mesh(points, end, DEFAULT_TOLERANCE);
    if ink.is_empty() {
        return None;
    }
    let positions: Vec<[f32; 3]> = ink.positions().map(|[x, y]| [x, -y, 0.0]).collect();
    let mesh = Mesh::new(
        PrimitiveTopology::TriangleList,
        RenderAssetUsages::default(),
    )
    .with_inserted_attribute(Mesh::ATTRIBUTE_POSITION, positions)
    .with_inserted_indices(Indices::U32(ink.indices));
    Some(mesh)
}

// ---- wet ink ----

fn apply_wet_ink(
    mut commands: Commands,
    mut frames: MessageReader<WetInkFrame>,
    editor: Res<EditorState>,
    mut scenes: ResMut<SketchScenes>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<ColorMaterial>>,
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
                ..
            } => {
                let Some(layer) = scenes.scenes.get(&sketch).map(|s| s.layer) else {
                    continue; // sketch not on screen yet; CRDT commit will cover it
                };
                let old = scenes.wet.insert(
                    stroke,
                    WetStroke {
                        entity: None,
                        mesh: None,
                        layer,
                        tool,
                        color,
                        base_width,
                        points: Vec::new(),
                        last_seq: 0,
                        expires_ms: None,
                    },
                );
                if let Some(entity) = old.and_then(|w| w.entity) {
                    commands.entity(entity).despawn();
                }
            }
            WetInk::Points {
                stroke,
                seq,
                sent_ms,
                ..
            } => {
                latency.samples_ms.push(now_ms() as f64 - sent_ms as f64);
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
                let ink = Ink::preset(wet.tool, wet.color, wet.base_width);
                let Some(mesh) = ink_mesh(&ink, &wet.points, StrokeEnd::Live) else {
                    continue; // nothing drawable yet
                };
                match &wet.mesh {
                    Some(handle) => {
                        if let Err(err) = meshes.insert(handle, mesh) {
                            tracing::error!(%err, "wet-ink mesh update failed");
                        }
                    }
                    None => {
                        let handle = meshes.add(mesh);
                        let entity = commands
                            .spawn((
                                Mesh2d(handle.clone()),
                                MeshMaterial2d(materials.add(color_of(wet.color))),
                                Transform::from_xyz(0.0, 0.0, 500.0),
                                RenderLayers::layer(wet.layer),
                            ))
                            .id();
                        wet.mesh = Some(handle);
                        wet.entity = Some(entity);
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
                    let ink = Ink::preset(wet.tool, wet.color, wet.base_width);
                    if let (Some(handle), Some(mesh)) =
                        (&wet.mesh, ink_mesh(&ink, &wet.points, StrokeEnd::Complete))
                        && let Err(err) = meshes.insert(handle, mesh)
                    {
                        tracing::error!(%err, "wet-ink mesh update failed");
                    }
                }
                report_latency(&mut latency);
            }
            // No stroke is coming (ruler drag, tool fiddling): drop it now.
            WetInk::Cancel { stroke } => {
                if let Some(entity) = scenes.wet.remove(&stroke).and_then(|w| w.entity) {
                    commands.entity(entity).despawn();
                }
            }
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
        if let Some(entity) = scenes.wet.remove(&id).and_then(|w| w.entity) {
            commands.entity(entity).despawn();
        }
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
