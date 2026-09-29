//! The Bevy app every target runs, assembled from what the platform's
//! startup produced.

use bevy::prelude::*;

use crate::docs::Docs;
use crate::import::Inbox;
use crate::node::SyncNode;
use crate::runtime::Runtime;
use crate::settings::Settings;
use crate::sync::{SyncPlugin, SyncTransport};
use crate::theme::{self, Theme, ThemeSync};
use crate::ui::{EditorUiPlugin, FollowLatest};

/// Everything the platform's startup produced.
pub struct AppInputs {
    pub docs: Docs,
    pub inbox: Inbox,
    pub theme: Theme,
    pub sync_theme: bool,
    pub runtime: Runtime,
    pub sync_node: SyncNode,
    pub transport: SyncTransport,
    pub settings: Settings,
    pub follow_latest: bool,
}

pub fn build(inputs: AppInputs) -> App {
    let mut app = App::new();
    app.add_plugins(
        DefaultPlugins
            .set(WindowPlugin {
                primary_window: Some(window()),
                ..default()
            })
            // Every asset is embedded; don't probe the web server for
            // `.meta` files.
            .set(AssetPlugin {
                meta_check: bevy::asset::AssetMetaCheck::Never,
                ..default()
            })
            // Loro narrates every block it encodes at info.
            .set(bevy::log::LogPlugin {
                filter: format!("{},loro_internal=warn", bevy::log::DEFAULT_FILTER),
                ..default()
            }),
    )
    .add_plugins(bevy_egui::EguiPlugin::default())
    .add_plugins((
        SyncPlugin,
        theme::ThemePlugin,
        EditorUiPlugin,
        crate::sketch::SketchPlugin,
        crate::settings::SettingsPlugin,
        crate::node::NodePlugin,
        crate::import::ImportPlugin,
    ))
    .insert_resource(inputs.docs)
    .insert_resource(inputs.inbox)
    .insert_resource(inputs.theme.clear_color())
    .insert_resource(inputs.theme)
    .insert_resource(ThemeSync::new(inputs.sync_theme))
    .insert_resource(inputs.runtime)
    .insert_resource(inputs.sync_node)
    .insert_resource(inputs.transport)
    .insert_resource(inputs.settings)
    .insert_resource(FollowLatest(inputs.follow_latest))
    .add_systems(Startup, setup);
    app
}

#[cfg(not(target_arch = "wasm32"))]
fn window() -> Window {
    Window {
        title: "krabink".into(),
        // macOS: draw content under a transparent titlebar so the
        // traffic lights sit on the sidebar. Ignored elsewhere.
        fullsize_content_view: true,
        titlebar_transparent: true,
        titlebar_show_title: false,
        ..default()
    }
}

#[cfg(target_arch = "wasm32")]
fn window() -> Window {
    Window {
        title: "krabink".into(),
        canvas: Some("#krabink".into()),
        fit_canvas_to_parent: true,
        ..default()
    }
}

fn setup(mut commands: Commands, sync: Res<SyncNode>, settings: Res<Settings>) {
    commands.spawn(Camera2d);
    // Bevy's LogPlugin owns the subscriber; anything logged before App::run
    // is lost, so announce the node here.
    info!(
        node = %sync.node.id(),
        addrs = ?settings.info.addrs,
        relay = settings.info.relay.as_deref().unwrap_or("none"),
        mdns = sync.mdns_name().unwrap_or("off"),
        "sync node up"
    );
}
