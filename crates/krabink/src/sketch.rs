//! Page ink rendering: the open note's page layer (strokes anchored to
//! source lines) renders in an off-screen bevy scene (own render layer +
//! camera) into an `Image` that the editor paints under its text. The
//! scene covers the part of the galley on screen plus some overscan, and
//! the camera follows the scroll; every element sits at its line's origin
//! from the editor's [`PageLayout`], so ink moves with the text.
//!
//! Committed CRDT strokes become ink meshes drawn with [`InkMaterial`];
//! wet ink from the ephemeral channel renders as provisional meshes on top
//! and is dropped once the authoritative stroke lands (or after a timeout).
//! Draw order is z order: committed element `k` sits at `k / 100`, remote
//! wet stroke `j` at `990 + j / 100`, and the ink pipeline writes depth.
//!
//! Peers' pens show as pointers above everything: the tip's footprint at
//! reduced alpha (the same hover dab the iPad draws locally) inside a ring
//! coloured per device. Pointers arrive on the ephemeral channel too and
//! vanish when withdrawn or after a short silence.
//!
//! The paper is opaque: the highlighter multiplies with what is under it,
//! so the texture clears to the paper colour and the text is painted over
//! it rather than the ink over the text.

use std::collections::HashMap;

use bevy::asset::RenderAssetUsages;
use bevy::camera::visibility::RenderLayers;
use bevy::camera::{ImageRenderTarget, RenderTarget, ScalingMode};
use bevy::color::{Hsla, Srgba};
use bevy::mesh::{Indices, PrimitiveTopology};
use bevy::prelude::*;
use bevy::render::render_resource::TextureUsages;
use bevy::render::storage::ShaderBuffer;
use bevy_egui::{EguiTextureHandle, EguiUserTextures, egui};
use krabink_core::{
    Anchor, BrushSpec, DEFAULT_TOLERANCE, DeviceId, DocKey, ElementId, Ink, InkMesh, InkStyle,
    NoteDoc, NoteId, Rgba, StrokeEnd, StrokeId, StrokePoint, Tool, WetInk,
};

use crate::docs::{Docs, now_ms};
use crate::ink_assets::{InkAssets, sync_assets};
use crate::ink_material::{
    ATTRIBUTE_INK_OPACITY, ATTRIBUTE_INK_UV, InkMaterial, InkMaterialPlugin, InkPalette, InkParams,
    InkSlot,
};
use crate::theme::{Palette, Theme};
use crate::ui::{EditorState, PageLayout, resolve_origin};

/// Wet ink lingers this long after `End` if the committed stroke never shows.
const WET_TTL_MS: u64 = 5_000;
/// Render-target size bounds in points (the texture is `scale` times
/// bigger in pixels).
const MIN_TARGET: u32 = 256;
const MAX_TARGET: u32 = 4096;
/// Extra galley above and below the visible window in the target, so a
/// scroll shows rendered ink while the camera catches up.
const OVERSCAN: f32 = 256.0;
/// The off-screen scene's render layer (0 is the main window).
const LAYER: usize = 1;
/// z spacing between consecutive strokes.
const Z_STEP: f32 = 0.01;
/// Remote wet strokes sit above every committed element.
const WET_Z_BASE: f32 = 990.0;
/// Wet z slots wrap here so they stay under the pointers and the camera's
/// far plane (1000).
const WET_Z_SLOTS: u16 = 900;
/// Remote pointers sit above every wet stroke: the tip's footprint, then
/// the ring around it.
const POINTER_DAB_Z: f32 = 999.2;
const POINTER_RING_Z: f32 = 999.4;
/// A pointer that stops updating is dropped after this long.
const POINTER_TTL_MS: u64 = 1_500;
/// Alpha of the pointer's footprint relative to the ink's own, hovering
/// and drawing.
const POINTER_HOVER_ALPHA: f32 = 0.35;
const POINTER_DOWN_ALPHA: f32 = 0.7;
/// The ring: line width in canvas units, and its radius floor around a
/// thin tool.
const POINTER_RING_WIDTH: f32 = 1.5;
const POINTER_RING_MIN_RADIUS: f32 = 6.0;

/// An ephemeral frame relayed by the server (raised by the sync layer).
#[derive(Message)]
pub struct WetInkFrame {
    pub doc: DocKey,
    pub payload: Vec<u8>,
    /// The device that sent it (pointers are keyed by sender).
    pub from: DeviceId,
}

/// The rendered page texture, for the editor to paint under its text.
#[derive(Debug, Clone, Copy)]
pub struct PageTarget {
    pub texture: egui::TextureId,
    /// Size in points.
    pub size: egui::Vec2,
    /// Galley-space top-left of the rendered region.
    pub window_min: egui::Vec2,
}

#[derive(Resource, Default)]
pub struct PageTexture(pub Option<PageTarget>);

/// A spawned ink entity, the palette slot it draws with and its z.
#[derive(Debug, Clone, Copy)]
struct InkEntity {
    entity: Entity,
    slot: InkSlot,
    z: f32,
}

struct WetStroke {
    anchor: Anchor,
    /// Resolved when the entity spawns; refreshed on every layout change.
    origin: Option<Vec2>,
    /// Spawned on the first batch with drawable geometry.
    drawn: Option<InkEntity>,
    mesh: Option<Handle<Mesh>>,
    tool: Tool,
    color: Rgba,
    base_width: f32,
    /// A custom brush's spec from `BeginAnchored`; `None` draws the tool's
    /// preset.
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

/// One drawable piece of a remote pointer (the footprint or the ring),
/// kept across updates so a move re-uploads the mesh and nothing else.
#[derive(Default)]
struct PointerPart {
    drawn: Option<InkEntity>,
    mesh: Option<Handle<Mesh>>,
    style: Option<InkStyle>,
}

impl PointerPart {
    /// Show `ink` placed by `at` (its line's origin and z), or nothing.
    fn set(
        &mut self,
        commands: &mut Commands,
        scene: &mut PageScene,
        meshes: &mut Assets<Mesh>,
        ink_assets: &InkAssets,
        ink: Option<(Mesh, InkStyle)>,
        at: Transform,
    ) {
        let Some((mesh, style)) = ink else {
            self.clear(commands, Some(scene));
            return;
        };
        let handle = match &self.mesh {
            Some(handle) => {
                if let Err(err) = meshes.insert(handle, mesh) {
                    tracing::error!(%err, "pointer mesh update failed");
                }
                handle.clone()
            }
            None => {
                let handle = meshes.add(mesh);
                self.mesh = Some(handle.clone());
                handle
            }
        };
        let style_changed = self.style.as_ref() != Some(&style);
        match self.drawn {
            Some(drawn) if !style_changed => {
                commands.entity(drawn.entity).insert(at);
            }
            Some(drawn) => {
                // Tool or colour changed: restyle the entity in place.
                scene.palette.remove(drawn.slot);
                let slot = scene.palette.insert(&style, ink_assets);
                let (tag, material) = scene.palette.components(slot);
                commands.entity(drawn.entity).insert((tag, material, at));
                self.drawn = Some(InkEntity {
                    entity: drawn.entity,
                    slot,
                    z: at.translation.z,
                });
            }
            None => {
                let slot = scene.palette.insert(&style, ink_assets);
                let (tag, material) = scene.palette.components(slot);
                let entity = commands
                    .spawn((
                        Mesh2d(handle),
                        material,
                        tag,
                        at,
                        RenderLayers::layer(LAYER),
                    ))
                    .id();
                self.drawn = Some(InkEntity {
                    entity,
                    slot,
                    z: at.translation.z,
                });
            }
        }
        self.style = Some(style);
    }

    /// Despawn; `scene` is `None` only when the scene itself is gone.
    fn clear(&mut self, commands: &mut Commands, scene: Option<&mut PageScene>) {
        if let Some(drawn) = self.drawn.take() {
            commands.entity(drawn.entity).despawn();
            if let Some(scene) = scene {
                scene.palette.remove(drawn.slot);
            }
        }
        self.mesh = None;
        self.style = None;
    }

    /// Move the entity to a new line origin.
    fn replace(&self, commands: &mut Commands, origin: Vec2) {
        if let Some(drawn) = self.drawn {
            commands.entity(drawn.entity).insert(place(origin, drawn.z));
        }
    }
}

/// What a pointer draws this update: each part `None` to hide it.
struct PointerLook {
    dab: Option<(Mesh, InkStyle)>,
    ring: Option<(Mesh, InkStyle)>,
}

/// A peer's pen over the page, in the anchor space of `anchor`.
struct RemotePointer {
    anchor: Anchor,
    dab: PointerPart,
    ring: PointerPart,
    /// Local clock at the last update; dropped after [`POINTER_TTL_MS`].
    seen_ms: u64,
}

/// The off-screen image the page renders into and the camera drawing it.
struct SceneTarget {
    image: Handle<Image>,
    texture: egui::TextureId,
    camera: Entity,
    size_px: UVec2,
    size_pts: Vec2,
    /// Galley-space top-left of the rendered region.
    window_min: Vec2,
}

struct PageScene {
    note: NoteId,
    /// Pixels per point the target and the grain were built for.
    scale: f32,
    target: SceneTarget,
    palette: InkPalette,
    /// The `InkAssets` generation the strokes were styled against.
    assets_generation: u32,
    /// The `PageLayout` generation the elements were placed against.
    layout_generation: Option<u64>,
    /// Committed element id → ink entity (`None` for elements with no ink).
    strokes: HashMap<ElementId, Option<InkEntity>>,
    /// Committed elements spawned so far; the next one's z slot.
    committed: u16,
    /// Next remote wet stroke's z slot.
    wet_serial: u16,
}

#[derive(Resource, Default)]
struct PageScenes {
    scene: Option<PageScene>,
    wet: HashMap<StrokeId, WetStroke>,
    /// One pointer per peer device.
    pointers: HashMap<DeviceId, RemotePointer>,
}

impl PageScenes {
    /// Drop a wet stroke's entity and free its style slot.
    fn despawn_wet(&mut self, commands: &mut Commands, id: StrokeId) {
        let Some(wet) = self.wet.remove(&id) else {
            return;
        };
        despawn_wet(commands, self.scene.as_mut(), wet);
    }

    /// Redraw `from`'s pointer as `look`, placed at `origin` (its line).
    #[expect(
        clippy::too_many_arguments,
        reason = "every ECS handle a pointer needs, threaded from one system"
    )]
    fn update_pointer(
        &mut self,
        commands: &mut Commands,
        meshes: &mut Assets<Mesh>,
        ink_assets: &InkAssets,
        from: DeviceId,
        anchor: Anchor,
        origin: Vec2,
        look: PointerLook,
    ) {
        let Some(scene) = self.scene.as_mut() else {
            return; // page not on screen; nothing to draw into
        };
        let pointer = self.pointers.entry(from).or_insert_with(|| RemotePointer {
            anchor: anchor.clone(),
            dab: PointerPart::default(),
            ring: PointerPart::default(),
            seen_ms: 0,
        });
        pointer.anchor = anchor;
        pointer.dab.set(
            commands,
            scene,
            meshes,
            ink_assets,
            look.dab,
            place(origin, POINTER_DAB_Z),
        );
        pointer.ring.set(
            commands,
            scene,
            meshes,
            ink_assets,
            look.ring,
            place(origin, POINTER_RING_Z),
        );
        pointer.seen_ms = now_ms();
    }

    /// Drop `from`'s pointer and free its style slots.
    fn despawn_pointer(&mut self, commands: &mut Commands, from: DeviceId) {
        let Some(mut pointer) = self.pointers.remove(&from) else {
            return;
        };
        pointer.dab.clear(commands, self.scene.as_mut());
        pointer.ring.clear(commands, self.scene.as_mut());
    }

    /// Despawn everything: the scene is rebuilt for another note or scale.
    fn teardown(&mut self, commands: &mut Commands, egui_textures: &mut EguiUserTextures) {
        for (_, wet) in self.wet.drain() {
            if let Some(drawn) = wet.drawn {
                commands.entity(drawn.entity).despawn();
            }
        }
        for (_, mut pointer) in self.pointers.drain() {
            pointer.dab.clear(commands, None);
            pointer.ring.clear(commands, None);
        }
        if let Some(scene) = self.scene.take() {
            for drawn in scene.strokes.into_values().flatten() {
                commands.entity(drawn.entity).despawn();
            }
            commands.entity(scene.target.camera).despawn();
            egui_textures.remove_image(scene.target.image.id());
        }
    }
}

fn despawn_wet(commands: &mut Commands, scene: Option<&mut PageScene>, wet: WetStroke) {
    if let Some(drawn) = wet.drawn {
        commands.entity(drawn.entity).despawn();
        if let Some(scene) = scene {
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
            .init_resource::<PageScenes>()
            .init_resource::<PageTexture>()
            .init_resource::<WetLatency>()
            .add_systems(PreStartup, init_ink_assets)
            .add_systems(
                Update,
                (
                    sync_assets,
                    apply_theme,
                    sync_page_scene,
                    apply_wet_ink,
                    flush_palettes,
                )
                    .chain(),
            );
    }
}

// ---- placement ----

/// The transform putting anchor-space geometry at its line's origin
/// (galley space, y down) in the scene (y up).
fn place(origin: Vec2, z: f32) -> Transform {
    Transform::from_xyz(origin.x, -origin.y, z)
}

fn to_bevy(v: egui::Vec2) -> Vec2 {
    Vec2::new(v.x, v.y)
}

fn to_egui(v: Vec2) -> egui::Vec2 {
    egui::vec2(v.x, v.y)
}

/// Where `anchor`'s line sits, in galley space (through the reading
/// view's source map when that is what the galley shows).
fn origin_of(layout: &PageLayout, galley: &egui::Galley, note: &NoteDoc, anchor: &Anchor) -> Vec2 {
    to_bevy(resolve_origin(galley, note, anchor, layout.source_map()))
}

/// The render target region for a visible galley `window`: the window
/// plus [`OVERSCAN`] above and below, rounded up to 64 points and clamped.
/// Returns (galley-space top-left, size in points).
fn target_region(window_min: Vec2, window_size: Vec2) -> (Vec2, Vec2) {
    let size = Vec2::new(
        target_extent(window_size.x) as f32,
        target_extent(window_size.y + 2.0 * OVERSCAN) as f32,
    );
    (Vec2::new(window_min.x, window_min.y - OVERSCAN), size)
}

fn target_extent(len: f32) -> u32 {
    (len.max(0.0).ceil() as u32)
        .next_multiple_of(64)
        .clamp(MIN_TARGET, MAX_TARGET)
}

/// The camera looking at the region, centred on it. Galley y grows down
/// and meshes negate y, so the region `[min, min + size]` is the world
/// rect `[min.x, min.x + w] x [-(min.y + h), -min.y]`.
fn camera_transform(window_min: Vec2, size_pts: Vec2) -> Transform {
    Transform::from_xyz(
        window_min.x + size_pts.x / 2.0,
        -(window_min.y + size_pts.y / 2.0),
        0.0,
    )
}

// ---- scene / stroke sync ----

#[expect(
    clippy::too_many_arguments,
    reason = "bevy system; each param is a distinct ECS resource"
)]
fn sync_page_scene(
    mut commands: Commands,
    docs: Res<Docs>,
    editor: Res<EditorState>,
    layout: Res<PageLayout>,
    mut scenes: ResMut<PageScenes>,
    mut page_texture: ResMut<PageTexture>,
    mut images: ResMut<Assets<Image>>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<InkMaterial>>,
    mut buffers: ResMut<Assets<ShaderBuffer>>,
    mut egui_textures: ResMut<EguiUserTextures>,
    ink_assets: Res<InkAssets>,
    theme: Res<Theme>,
) {
    let Some(note_id) = editor.open else {
        return;
    };
    if layout.note != Some(note_id) {
        return; // the editor has not laid this note out yet
    }
    let (Some(note), Some(galley)) = (docs.note(note_id), layout.galley.as_ref()) else {
        return;
    };

    // Meshes, slots and z are per note; the target size and the grain
    // per pixel density. Either change rebuilds the scene.
    if scenes
        .scene
        .as_ref()
        .is_some_and(|s| s.note != note_id || s.scale != layout.scale)
    {
        scenes.teardown(&mut commands, &mut egui_textures);
    }

    let (window_min, size_pts) = target_region(
        to_bevy(layout.window.min.to_vec2()),
        to_bevy(layout.window.size()),
    );
    let size_px = (size_pts * layout.scale).round().as_uvec2();

    let PageScenes {
        scene,
        wet,
        pointers,
    } = &mut *scenes;
    let scene = scene.get_or_insert_with(|| {
        let target = new_target(
            &mut commands,
            &mut images,
            &mut egui_textures,
            size_px,
            size_pts,
            window_min,
            theme.palette(),
        );
        let palette = InkPalette::new(
            &mut buffers,
            &mut materials,
            // Canvas units per pixel: grain renders at full strength at
            // 1x and stays crisp on HiDPI.
            InkParams::new(1.0 / layout.scale),
            &ink_assets,
            theme.palette().paper_tone(),
        );
        PageScene {
            note: note_id,
            scale: layout.scale,
            target,
            palette,
            assets_generation: ink_assets.generation,
            layout_generation: None,
            strokes: HashMap::new(),
            committed: 0,
            wet_serial: 0,
        }
    });

    if scene.target.size_px != size_px {
        commands.entity(scene.target.camera).despawn();
        egui_textures.remove_image(scene.target.image.id());
        scene.target = new_target(
            &mut commands,
            &mut images,
            &mut egui_textures,
            size_px,
            size_pts,
            window_min,
            theme.palette(),
        );
    } else if scene.target.window_min != window_min {
        // Scrolled: the camera follows the window.
        commands
            .entity(scene.target.camera)
            .insert(camera_transform(window_min, size_pts));
        scene.target.window_min = window_min;
    }
    page_texture.0 = Some(PageTarget {
        texture: scene.target.texture,
        size: to_egui(scene.target.size_pts),
        window_min: to_egui(scene.target.window_min),
    });

    // New texture arrays: every stroke's layers may have moved, so
    // restyle them all by respawning.
    if scene.assets_generation != ink_assets.generation {
        scene.assets_generation = ink_assets.generation;
        scene.palette.set_assets(&mut materials, &ink_assets);
        for drawn in scene.strokes.drain().filter_map(|(_, d)| d) {
            commands.entity(drawn.entity).despawn();
            scene.palette.remove(drawn.slot);
        }
        scene.layout_generation = None;
    }

    if scene.layout_generation == Some(layout.generation) {
        return; // nothing moved and nothing changed
    }
    scene.layout_generation = Some(layout.generation);
    let tolerance = DEFAULT_TOLERANCE / layout.scale;

    // Diff committed elements; re-place the ones that stay.
    let mut stale: HashMap<_, _> = scene.strokes.clone();
    for el in note.page_elements() {
        let id = el.element.id();
        let origin = layout
            .origins
            .get(&id)
            .map(|o| to_bevy(*o))
            .unwrap_or_else(|| origin_of(&layout, galley, note, &el.anchor));
        if let Some(drawn) = stale.remove(&id) {
            if let Some(drawn) = drawn {
                commands.entity(drawn.entity).insert(place(origin, drawn.z));
            }
            continue;
        }
        let outline = el.element.outline();
        let drawn = ink_mesh(&el.element.ink(), &outline, StrokeEnd::Complete, tolerance).map(
            |(mesh, style)| {
                let slot = scene.palette.insert(&style, &ink_assets);
                let z = committed_z(scene.committed);
                scene.committed = scene.committed.saturating_add(1);
                let (tag, material) = scene.palette.components(slot);
                let entity = commands
                    .spawn((
                        Mesh2d(meshes.add(mesh)),
                        material,
                        tag,
                        place(origin, z),
                        RenderLayers::layer(LAYER),
                    ))
                    .id();
                InkEntity { entity, slot, z }
            },
        );
        scene.strokes.insert(id, drawn);
        // A committed element (stroke or snapped shape) replaces its
        // wet-ink preview.
        if let Some(wet) = wet.remove(&id)
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

    // Wet strokes and pointers follow their lines too.
    for wet in wet.values_mut() {
        let origin = origin_of(&layout, galley, note, &wet.anchor);
        wet.origin = Some(origin);
        if let Some(drawn) = wet.drawn {
            commands.entity(drawn.entity).insert(place(origin, drawn.z));
        }
    }
    for pointer in pointers.values() {
        let origin = origin_of(&layout, galley, note, &pointer.anchor);
        pointer.dab.replace(&mut commands, origin);
        pointer.ring.replace(&mut commands, origin);
    }
}

/// Upload the page's style edits after the frame's diffs.
fn flush_palettes(
    mut scenes: ResMut<PageScenes>,
    mut materials: ResMut<Assets<InkMaterial>>,
    mut buffers: ResMut<Assets<ShaderBuffer>>,
) {
    if let Some(scene) = scenes.scene.as_mut() {
        scene.palette.flush(&mut buffers, &mut materials);
    }
}

/// z of the `k`th committed element; saturates far below the wet band.
fn committed_z(k: u16) -> f32 {
    f32::from(k) * Z_STEP
}

/// z of the `j`th remote wet stroke, above every committed element.
fn wet_z(j: u16) -> f32 {
    WET_Z_BASE + f32::from(j) * Z_STEP
}

/// The theme changed: the paper takes the new colour and the highlighter
/// re-specialises for its tone.
fn apply_theme(
    theme: Res<Theme>,
    scenes: Res<PageScenes>,
    mut cameras: Query<&mut Camera>,
    mut materials: ResMut<Assets<InkMaterial>>,
) {
    if !theme.is_changed() {
        return;
    }
    let palette = theme.palette();
    if let Some(scene) = scenes.scene.as_ref() {
        if let Ok(mut camera) = cameras.get_mut(scene.target.camera) {
            camera.clear_color = ClearColorConfig::Custom(palette.paper_color());
        }
        scene
            .palette
            .set_paper(&mut materials, palette.paper_tone());
    }
}

fn new_target(
    commands: &mut Commands,
    images: &mut Assets<Image>,
    egui_textures: &mut EguiUserTextures,
    size_px: UVec2,
    size_pts: Vec2,
    window_min: Vec2,
    palette: &Palette,
) -> SceneTarget {
    let mut image = Image::new_fill(
        bevy::render::render_resource::Extent3d {
            width: size_px.x.max(1),
            height: size_px.y.max(1),
            depth_or_array_layers: 1,
        },
        bevy::render::render_resource::TextureDimension::D2,
        &palette.paper_bytes(),
        bevy::render::render_resource::TextureFormat::Bgra8UnormSrgb,
        RenderAssetUsages::default(),
    );
    image.texture_descriptor.usage |= TextureUsages::RENDER_ATTACHMENT;
    let image = images.add(image);
    let texture = egui_textures.add_image(EguiTextureHandle::Strong(image.clone()));

    let camera = commands
        .spawn((
            Camera2d,
            Camera {
                clear_color: ClearColorConfig::Custom(palette.paper_color()),
                order: -1,
                ..default()
            },
            RenderTarget::Image(ImageRenderTarget::from(image.clone())),
            // Pinned, not left to bevy's default: the iPad's Metal view
            // samples 4x too, so both platforms antialias ink edges alike.
            Msaa::Sample4,
            Projection::Orthographic(OrthographicProjection {
                scaling_mode: ScalingMode::Fixed {
                    width: size_pts.x,
                    height: size_pts.y,
                },
                ..OrthographicProjection::default_2d()
            }),
            camera_transform(window_min, size_pts),
            RenderLayers::layer(LAYER),
        ))
        .id();

    SceneTarget {
        image,
        texture,
        camera,
        size_px,
        size_pts,
        window_min,
    }
}

/// Anchor-space stroke mesh → bevy mesh (y flipped into bevy's y-up space)
/// plus the style it draws with.
/// `None` when there is nothing to draw: bevy's mesh allocator never
/// allocates a zero-vertex mesh but still tries to upload it, logging a
/// "Use-after-free" error every frame the mesh is extracted.
fn ink_mesh(
    ink: &Ink<'_>,
    points: &[StrokePoint],
    end: StrokeEnd,
    tolerance: f32,
) -> Option<(Mesh, InkStyle)> {
    bevy_mesh(ink.mesh(points, end, tolerance))
}

/// A tessellated ink mesh as a bevy mesh (canvas y down → world y up).
fn bevy_mesh(ink: InkMesh) -> Option<(Mesh, InkStyle)> {
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

// ---- remote pointers ----

/// The footprint the peer's tip would leave at (`x`, `y`): the iPad's own
/// hover dab, faint while hovering and stronger while drawing.
fn pointer_dab(
    tool: Tool,
    color: Rgba,
    base_width: f32,
    x: f32,
    y: f32,
    tilt: Option<krabink_core::Tilt>,
    down: bool,
) -> Option<(Mesh, InkStyle)> {
    let scale = if down {
        POINTER_DOWN_ALPHA
    } else {
        POINTER_HOVER_ALPHA
    };
    let [r, g, b, a] = color.0;
    let alpha = (f32::from(a) * scale).round().clamp(0.0, 255.0) as u8;
    let ink = Ink::preset(tool, Rgba([r, g, b, alpha]), base_width);
    bevy_mesh(ink.hover_dab(x, y, tilt, DEFAULT_TOLERANCE))
}

/// The ring around a pointer, in the sender's device colour: a closed
/// monoline circle through the same ink pipeline as everything else.
fn pointer_ring(from: DeviceId, x: f32, y: f32, radius: f32) -> Option<(Mesh, InkStyle)> {
    const SEGMENTS: u32 = 48;
    let points: Vec<StrokePoint> = (0..=SEGMENTS)
        .map(|i| {
            let angle = std::f32::consts::TAU * i as f32 / SEGMENTS as f32;
            StrokePoint {
                x: x + radius * angle.cos(),
                y: y + radius * angle.sin(),
                force: 0.5,
                t_ms: i * 4,
                tilt: None,
                size: None,
            }
        })
        .collect();
    let ink = Ink::preset(Tool::Monoline, device_color(from), POINTER_RING_WIDTH);
    bevy_mesh(ink.mesh(&points, StrokeEnd::Complete, DEFAULT_TOLERANCE))
}

/// A stable, readable-on-dark-paper colour per device: the hue hashed
/// from its id.
fn device_color(device: DeviceId) -> Rgba {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    device.to_string().hash(&mut hasher);
    let hue = (hasher.finish() % 360) as f32;
    Rgba(Srgba::from(Hsla::new(hue, 0.7, 0.65, 0.9)).to_u8_array())
}

// ---- wet ink ----

#[expect(
    clippy::too_many_arguments,
    reason = "bevy system; each param is a distinct ECS resource"
)]
fn apply_wet_ink(
    mut commands: Commands,
    mut frames: MessageReader<WetInkFrame>,
    docs: Res<Docs>,
    editor: Res<EditorState>,
    layout: Res<PageLayout>,
    mut scenes: ResMut<PageScenes>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut latency: ResMut<WetLatency>,
    ink_assets: Res<InkAssets>,
) {
    let open_doc = editor.open.map(DocKey::from);
    // The page on screen, if the editor has laid it out.
    let page = match (editor.open, layout.galley.as_ref()) {
        (Some(id), Some(galley)) if layout.note == Some(id) => {
            docs.note(id).map(|note| (note, galley.as_ref()))
        }
        _ => None,
    };
    let tolerance = DEFAULT_TOLERANCE / layout.scale;

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
            WetInk::BeginAnchored {
                stroke,
                anchor,
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
                if scenes.scene.is_none() {
                    continue; // page not on screen yet; CRDT commit will cover it
                }
                scenes.despawn_wet(&mut commands, stroke);
                scenes.wet.insert(
                    stroke,
                    WetStroke {
                        anchor: Anchor(anchor),
                        origin: None,
                        drawn: None,
                        mesh: None,
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
                let Some((mesh, style)) = ink_mesh(&ink, &wet.points, StrokeEnd::Live, tolerance)
                else {
                    continue; // nothing drawable yet
                };
                match &wet.mesh {
                    Some(handle) => {
                        if let Err(err) = meshes.insert(handle, mesh) {
                            tracing::error!(%err, "wet-ink mesh update failed");
                        }
                    }
                    None => {
                        let (Some(scene), Some((note, galley))) = (scenes.scene.as_mut(), page)
                        else {
                            continue; // page went away; the commit will cover it
                        };
                        let origin = *wet
                            .origin
                            .get_or_insert_with(|| origin_of(&layout, galley, note, &wet.anchor));
                        let slot = scene.palette.insert(&style, &ink_assets);
                        let z = wet_z(scene.wet_serial);
                        scene.wet_serial = (scene.wet_serial + 1) % WET_Z_SLOTS;
                        let (tag, material) = scene.palette.components(slot);
                        let handle = meshes.add(mesh);
                        let entity = commands
                            .spawn((
                                Mesh2d(handle.clone()),
                                material,
                                tag,
                                place(origin, z),
                                RenderLayers::layer(LAYER),
                            ))
                            .id();
                        wet.mesh = Some(handle);
                        wet.drawn = Some(InkEntity { entity, slot, z });
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
                    if let (Some(handle), Some((mesh, _))) = (
                        &wet.mesh,
                        ink_mesh(&ink, &wet.points, StrokeEnd::Complete, tolerance),
                    ) && let Err(err) = meshes.insert(handle, mesh)
                    {
                        tracing::error!(%err, "wet-ink mesh update failed");
                    }
                }
                report_latency(&mut latency);
            }
            // No stroke is coming (ruler drag, tool fiddling): drop it now.
            WetInk::Cancel { stroke } => scenes.despawn_wet(&mut commands, stroke),
            WetInk::PointerAnchored {
                anchor,
                x,
                y,
                tilt,
                tool,
                color,
                base_width,
                down,
                ..
            } => {
                let Some((note, galley)) = page else {
                    continue; // nowhere to place it
                };
                let anchor = Anchor(anchor);
                let origin = origin_of(&layout, galley, note, &anchor);
                let dab =
                    tool.and_then(|tool| pointer_dab(tool, color, base_width, x, y, tilt, down));
                // The eraser's ring is its reach; a tool's hugs the tip.
                let radius = match tool {
                    Some(_) => (base_width * 0.75).max(POINTER_RING_MIN_RADIUS),
                    None => base_width / 2.0,
                };
                let ring = pointer_ring(frame.from, x, y, radius);
                scenes.update_pointer(
                    &mut commands,
                    &mut meshes,
                    &ink_assets,
                    frame.from,
                    anchor,
                    origin,
                    PointerLook { dab, ring },
                );
            }
            WetInk::PointerAnchoredGone => scenes.despawn_pointer(&mut commands, frame.from),
            // Embedded sketches are no longer shown on the desktop.
            WetInk::Begin { sketch, .. } | WetInk::Pointer { sketch, .. } => {
                tracing::debug!(%sketch, "sketch-keyed wet ink ignored");
            }
            WetInk::PointerGone { .. } => {}
        }
    }

    // Drop pointers whose sender went quiet (a lost `PointerGone`).
    let now = now_ms();
    let stale: Vec<DeviceId> = scenes
        .pointers
        .iter()
        .filter(|(_, p)| p.seen_ms + POINTER_TTL_MS <= now)
        .map(|(id, _)| *id)
        .collect();
    for id in stale {
        scenes.despawn_pointer(&mut commands, id);
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

/// The ink texture arrays, before any scene exists.
fn init_ink_assets(mut commands: Commands, mut images: ResMut<Assets<Image>>) {
    commands.insert_resource(InkAssets::new(&mut images));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn target_extent_rounds_and_clamps() {
        assert_eq!(target_extent(0.0), MIN_TARGET);
        assert_eq!(target_extent(300.0), 320);
        assert_eq!(target_extent(320.0), 320);
        assert_eq!(target_extent(1e6), MAX_TARGET);
    }

    #[test]
    fn target_region_overscans_above_and_below() {
        let (min, size) = target_region(Vec2::new(-4.0, 1000.0), Vec2::new(700.0, 500.0));
        assert_eq!(min, Vec2::new(-4.0, 1000.0 - OVERSCAN));
        assert_eq!(size, Vec2::new(704.0, 1024.0));
    }

    #[test]
    fn camera_centres_on_region_with_y_flipped() {
        let t = camera_transform(Vec2::new(0.0, 100.0), Vec2::new(200.0, 400.0));
        assert_eq!(t.translation, Vec3::new(100.0, -300.0, 0.0));
    }

    #[test]
    fn placement_negates_y_only() {
        let t = place(Vec2::new(3.0, 7.0), 0.5);
        assert_eq!(t.translation, Vec3::new(3.0, -7.0, 0.5));
    }
}
