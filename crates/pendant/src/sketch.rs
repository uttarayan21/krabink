//! Sketch rendering: each sketch in the open note gets an off-screen bevy
//! scene (own render layer + camera) rendered into an `Image`; that image is
//! exposed to egui through a custom `pendant://` texture loader so the
//! markdown preview embeds it inline.
//!
//! Committed CRDT strokes become ribbon meshes; wet ink from the ephemeral
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
    DocKey, PointSize, Rgba, SKETCH_URI_PREFIX, SketchId, StrokeId, StrokePoint, WetInk, WetPoint,
    flatten_stroke, ribbon,
};

use crate::docs::{Docs, now_ms};
use crate::ui::EditorState;

/// Wet ink lingers this long after `End` if the committed stroke never shows.
const WET_TTL_MS: u64 = 5_000;
/// Render-target size bounds (pixels; 1 canvas unit = 1 pixel).
const MIN_TARGET: u32 = 256;
const MAX_TARGET: u32 = 2048;
const WET_WIDTH_FALLBACK: f32 = 2.0;

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
    entity: Entity,
    mesh: Handle<Mesh>,
    base_width: f32,
    points: Vec<WetPoint>,
    last_seq: u32,
    /// Set at `End`: drop at this deadline even without a commit.
    expires_ms: Option<u64>,
}

struct SketchScene {
    layer: usize,
    image: Handle<Image>,
    camera: Entity,
    size: UVec2,
    /// Committed stroke id → mesh entity.
    strokes: HashMap<StrokeId, Entity>,
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
        let strokes = match note.strokes(sketch) {
            Ok(strokes) => strokes,
            Err(err) => {
                tracing::error!(%err, %sketch, "reading strokes failed");
                continue;
            }
        };

        // Content bounds decide the render-target size.
        let max = strokes
            .iter()
            .flat_map(|s| &s.points)
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

        // Diff committed strokes.
        let mut stale: HashMap<_, _> = scene.strokes.clone();
        for (z, stroke) in strokes.iter().enumerate() {
            if stale.remove(&stroke.id).is_some() {
                continue;
            }
            let flat = flatten_stroke(stroke);
            let mesh = meshes.add(ribbon_mesh(&flat, stroke.base_width));
            let entity = commands
                .spawn((
                    Mesh2d(mesh),
                    MeshMaterial2d(materials.add(color_of(stroke.color))),
                    Transform::from_xyz(0.0, 0.0, z as f32 * 0.01),
                    RenderLayers::layer(scene.layer),
                ))
                .id();
            scene.strokes.insert(stroke.id, entity);
            // Committed stroke replaces its wet-ink preview.
            if let Some(wet) = scenes.wet.remove(&stroke.id) {
                commands.entity(wet.entity).despawn();
            }
        }
        for (id, entity) in stale {
            commands.entity(entity).despawn();
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

/// Canvas-space ribbon → bevy mesh (y flipped into bevy's y-up space).
fn ribbon_mesh(points: &[StrokePoint], base_width: f32) -> Mesh {
    let ribbon = ribbon(points, base_width);
    let positions: Vec<[f32; 3]> = ribbon
        .positions
        .iter()
        .map(|[x, y]| [*x, -*y, 0.0])
        .collect();
    Mesh::new(
        PrimitiveTopology::TriangleList,
        RenderAssetUsages::default(),
    )
    .with_inserted_attribute(Mesh::ATTRIBUTE_POSITION, positions)
    .with_inserted_indices(Indices::U32(ribbon.indices))
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
                color,
                base_width,
                ..
            } => {
                let Some(scene) = scenes.scenes.get(&sketch) else {
                    continue; // sketch not on screen yet; CRDT commit will cover it
                };
                let mesh = meshes.add(ribbon_mesh(&[], base_width));
                let entity = commands
                    .spawn((
                        Mesh2d(mesh.clone()),
                        MeshMaterial2d(materials.add(color_of(color))),
                        Transform::from_xyz(0.0, 0.0, 500.0),
                        RenderLayers::layer(scene.layer),
                    ))
                    .id();
                if let Some(old) = scenes.wet.insert(
                    stroke,
                    WetStroke {
                        entity,
                        mesh,
                        base_width,
                        points: Vec::new(),
                        last_seq: 0,
                        expires_ms: None,
                    },
                ) {
                    commands.entity(old.entity).despawn();
                }
            }
            WetInk::Points {
                stroke,
                seq,
                sent_ms,
                points,
            } => {
                latency.samples_ms.push(now_ms() as f64 - sent_ms as f64);
                let Some(wet) = scenes.wet.get_mut(&stroke) else {
                    continue; // joined mid-stroke; wait for the commit
                };
                if seq <= wet.last_seq && wet.last_seq != 0 {
                    continue;
                }
                wet.last_seq = seq;
                wet.points.extend(points);
                let flat: Vec<StrokePoint> = wet
                    .points
                    .iter()
                    .map(|p| StrokePoint {
                        x: p.x,
                        y: p.y,
                        force: p.force,
                        t_ms: 0,
                        tilt: None,
                        size: p.width.map(|w| PointSize { w, h: w }),
                    })
                    .collect();
                if let Err(err) = meshes.insert(
                    &wet.mesh,
                    ribbon_mesh(&flat, wet.base_width.max(WET_WIDTH_FALLBACK)),
                ) {
                    tracing::error!(%err, "wet-ink mesh update failed");
                }
            }
            WetInk::End { stroke, sent_ms } => {
                latency.samples_ms.push(now_ms() as f64 - sent_ms as f64);
                if let Some(wet) = scenes.wet.get_mut(&stroke) {
                    wet.expires_ms = Some(now_ms() + WET_TTL_MS);
                }
                report_latency(&mut latency);
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
        if let Some(wet) = scenes.wet.remove(&id) {
            commands.entity(wet.entity).despawn();
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
