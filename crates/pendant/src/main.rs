mod cli;
mod config;
mod docs;
mod errors;
mod ink_assets;
mod ink_material;
mod lab;
mod relay;
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

use crate::config::RuntimeConfig;
use crate::docs::Docs;
use crate::relay::EmbeddedRelay;
use crate::sync::{LinkKind, SyncPlugin, SyncTransport};
use crate::ui::EditorUiPlugin;

/// The tokio runtime behind the embedded relay and every sync link. Lives
/// as a resource so it outlives transport swaps (joining a workspace).
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
            server,
            token,
            strokes,
        }) => replay::run(replay::ReplayArgs {
            server,
            token,
            strokes,
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
    let config = RuntimeConfig::resolve(args.data_dir, args.server, args.token)?;
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

    // Always serve our own relay: accepts the per-install token plus the
    // remote relays' token so one QR opens every path.
    let relay = EmbeddedRelay::start(
        runtime.handle(),
        args.relay_listen,
        &config.relay_store_path,
        [config.relay_token.clone(), config.token.clone()]
            .into_iter()
            .filter(|t| !t.is_empty())
            .collect(),
        config.device,
    )?;

    let mut transport = SyncTransport::new(config.device);
    transport.add_link(
        runtime.handle(),
        LinkKind::Embedded,
        relay.local.clone(),
        config.relay_token.clone(),
    );
    let remotes = config.remote_relays();
    if remotes.is_empty() {
        tracing::info!("no dedicated relay configured; direct pairing only");
    }
    for server in remotes {
        transport.add_link(
            runtime.handle(),
            LinkKind::Remote,
            server,
            config.token.clone(),
        );
    }

    let pair = pendant_core::PairInfo {
        server: relay.advertised.clone(),
        token: config.pair_token(),
        fallback: config.server.clone(),
        alt: relay.alt.clone(),
        relay_id: Some(config.device.to_string()),
    };

    App::new()
        .add_plugins(DefaultPlugins.set(WindowPlugin {
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
        ))
        .insert_resource(docs)
        .insert_resource(Runtime(runtime))
        .insert_resource(relay)
        .insert_resource(transport)
        .insert_resource(settings::Settings::new(pair))
        .insert_resource(crate::ui::FollowLatest(args.follow_latest))
        .add_systems(Startup, setup)
        .run();
    Ok(())
}

fn setup(mut commands: Commands, relay: Res<EmbeddedRelay>) {
    commands.spawn(Camera2d);
    // Bevy's LogPlugin owns the subscriber; anything logged before App::run
    // is lost, so announce the relay here.
    info!(
        advertised = %relay.advertised,
        alt = ?relay.alt,
        mdns = relay.mdns_name.as_deref().unwrap_or("off"),
        "embedded relay up"
    );
}
