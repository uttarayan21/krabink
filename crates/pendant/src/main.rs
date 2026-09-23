// bevy's `AsBindGroup` derive on `InkMaterial` needs more than the default
// 128 for the auto-trait proof of its render-world system params.
#![recursion_limit = "256"]

mod cli;
mod config;
mod docs;
mod errors;
mod ink_assets;
mod ink_material;
mod lab;
mod node;
mod replay;
mod settings;
mod sketch;
mod sync;
mod theme;
mod ui;

use bevy::prelude::*;
use clap::Parser;
use errors::{Error, Result, ResultExt};
use pendant_core::Store;
use pendant_local::{Node, NodeConfig, Role, direct_addrs};

use crate::config::RuntimeConfig;
use crate::docs::Docs;
use crate::node::SyncNode;
use crate::sync::{SyncPlugin, SyncTransport};
use crate::ui::EditorUiPlugin;

/// The tokio runtime behind the sync node. Lives as a resource so systems
/// can drive the node's async calls.
#[derive(Resource)]
pub struct Runtime(pub tokio::runtime::Runtime);

fn main() -> Result<()> {
    let args = cli::Cli::parse();
    match args.cmd {
        Some(cli::SubCommand::Completions { shell }) => {
            cli::Cli::completions(shell);
            Ok(())
        }
        Some(cli::SubCommand::Replay {
            pair,
            strokes,
            pointer,
        }) => replay::run(replay::ReplayArgs {
            pair,
            strokes,
            pointer,
        }),
        Some(cli::SubCommand::BrushLab {
            corpus,
            presets,
            models,
            out,
            svg,
            metrics,
        }) => lab::run(lab::LabArgs {
            corpus,
            presets,
            models,
            out,
            svg,
            metrics,
        }),
        Some(cli::SubCommand::Pair { uri }) => config::adopt_pair(&uri),
        None => run_app(args),
    }
}

fn run_app(args: cli::Cli) -> Result<()> {
    let config = RuntimeConfig::resolve(args.data_dir, args.relay, args.token)?;
    sync::set_local_device_name(config.device_name.clone());
    let store = Store::open(&config.store_path)
        .change_context(Error)
        .attach_with(|| format!("opening store {}", config.store_path.display()))?;
    let docs = Docs::load(store)
        .change_context(Error)
        .attach("loading workspace")?;

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .change_context(Error)
        .attach("tokio runtime")?;

    // The node accepts our own token plus the adopted workspace's, so one
    // QR opens every path.
    let node = runtime
        .block_on(Node::start(NodeConfig {
            key_path: config.node_key_path.clone(),
            store_path: config.node_store_path.clone(),
            device: config.device,
            tokens: [config.workspace_token.clone(), config.token.clone()]
                .into_iter()
                .filter(|t| !t.is_empty())
                .collect(),
            relay: config.relay_target(),
            bind_port: None,
            role: Role::Device,
        }))
        .change_context(Error)
        .attach("starting sync node")?;
    let peers = config.peer_targets();
    if peers.is_empty() {
        tracing::info!("no peers configured; waiting to be paired");
    }
    runtime.block_on(node.set_peers(peers));

    let transport = {
        let _guard = runtime.enter();
        SyncTransport::new(node.clone(), config.workspace_token.clone())
    };
    let addrs = direct_addrs(&runtime.block_on(node.addr()))
        .into_iter()
        .map(|a| a.to_string())
        .collect();
    let pair = pendant_core::PairInfo {
        node: node.id().to_string(),
        token: config.pair_token(),
        relay: config.relay.as_ref().map(ToString::to_string),
        addrs,
        replica: config.replica.map(|id| id.to_string()),
    };
    let sync_node = SyncNode::new(node, &runtime);

    let mut app = App::new();
    app.add_plugins(DefaultPlugins.set(WindowPlugin {
        primary_window: Some(Window {
            title: "pendant".into(),
            ..default()
        }),
        ..default()
    }))
    .add_plugins(bevy_egui::EguiPlugin::default())
    .add_plugins((
        SyncPlugin,
        theme::ThemePlugin,
        EditorUiPlugin,
        crate::sketch::SketchPlugin,
        settings::SettingsPlugin,
        node::NodePlugin,
    ))
    .insert_resource(docs)
    .insert_resource(Runtime(runtime))
    .insert_resource(sync_node)
    .insert_resource(transport)
    .insert_resource(settings::Settings::new(
        pair,
        config.workspace_token.clone(),
    ))
    .insert_resource(crate::ui::FollowLatest(args.follow_latest))
    .add_systems(Startup, setup)
    .run();
    Ok(())
}

fn setup(mut commands: Commands, sync: Res<SyncNode>, settings: Res<settings::Settings>) {
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
