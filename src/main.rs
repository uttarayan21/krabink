mod errors;
use bevy::prelude::*;

const GRID_SPACING: f32 = 10.0;
const GRID_CELLS: u32 = 16;

pub fn main() {
    App::new()
        .insert_resource(ClearColor(Color::BLACK))
        .add_plugins((
            DefaultPlugins,
            bevy_panorbit_camera::PanOrbitCameraPlugin,
        ))
        .add_systems(Startup, setup)
        .add_systems(Update, draw_grid)
        .run();
}

fn setup(mut commands: Commands) {
    commands.spawn((
        bevy_panorbit_camera::PanOrbitCamera {
            axis: [Vec3::X, Vec3::Z, Vec3::Y],
            allow_upside_down: true,
            zoom_smoothness: 0.0,
            target_focus: Vec3::new(10.0, 10.0, 10.0),
            target_radius: 10.0,
            ..default()
        },
        Transform::from_xyz(50.0, -50.0, 50.0).looking_at(Vec3::ZERO, Vec3::Z),
    ));
}

fn draw_grid(mut gizmos: Gizmos) {
    gizmos.grid(
        Quat::IDENTITY,
        UVec2::splat(GRID_CELLS),
        Vec2::splat(GRID_SPACING),
        Color::srgb(0.35, 0.35, 0.35),
    );

    let axis_length = GRID_SPACING * GRID_CELLS as f32 * 0.5;
    let axis_offset = 0.05;

    gizmos.line(
        Vec3::new(-axis_length, 0.0, axis_offset),
        Vec3::new(axis_length, 0.0, axis_offset),
        Color::srgb(1.0, 0.2, 0.2),
    );
    gizmos.line(
        Vec3::new(0.0, -axis_length, axis_offset),
        Vec3::new(0.0, axis_length, axis_offset),
        Color::srgb(0.2, 1.0, 0.2),
    );
    gizmos.line(
        Vec3::ZERO,
        Vec3::new(0.0, 0.0, axis_length),
        Color::srgb(0.2, 0.4, 1.0),
    );
}
