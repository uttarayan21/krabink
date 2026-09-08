mod cli;
mod config;
mod docs;
mod errors;
mod replay;
mod sketch;
mod sync;
mod ui;

use bevy::prelude::*;
use clap::Parser;
use errors::{Error, Result, ResultExt};
use pendant_core::Store;

use crate::config::RuntimeConfig;
use crate::docs::Docs;
use crate::sync::{SyncPlugin, SyncTransport};
use crate::ui::EditorUiPlugin;

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

    let transport = match &config.server {
        Some(server) => SyncTransport::connect(server.clone(), config.token.clone(), config.device),
        None => {
            tracing::warn!("no server configured; running offline");
            SyncTransport::disabled(config.device)
        }
    };

    App::new()
        .insert_resource(ClearColor(Color::srgb(0.09, 0.09, 0.11)))
        .add_plugins(DefaultPlugins.set(WindowPlugin {
            primary_window: Some(Window {
                title: "pendant".into(),
                ..default()
            }),
            ..default()
        }))
        .add_plugins(bevy_egui::EguiPlugin::default())
        .add_plugins((SyncPlugin, EditorUiPlugin, crate::sketch::SketchPlugin))
        .insert_resource(docs)
        .insert_resource(transport)
        .insert_resource(crate::ui::FollowLatest(args.follow_latest))
        .add_systems(Startup, setup)
        .run();
    Ok(())
}

fn setup(mut commands: Commands) {
    commands.spawn(Camera2d);
}
