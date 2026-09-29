//! Desktop entry: CLI subcommands, the data directory, a tokio runtime
//! and a node on a remembered UDP port.

use clap::Parser;
use krabink_core::Store;
use krabink_local::{Node, NodeConfig, Role, direct_addrs};

use crate::app::{self, AppInputs};
use crate::config;
use crate::docs::Docs;
use crate::errors::{Error, Result, ResultExt};
use crate::node::SyncNode;
use crate::runtime::Runtime;
use crate::sync::SyncTransport;
use crate::{cli, import, lab, replay, settings, sync, theme};

pub fn main() -> Result<()> {
    // Finder/LaunchServices can still pass a `-psn_<n>_<n>` process serial
    // number to bundled apps; clap would reject it and the app would quit
    // on launch.
    let args = cli::Cli::parse_from(
        std::env::args_os().filter(|arg| !arg.to_string_lossy().starts_with("-psn_")),
    );
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
    let config = config::resolve(args.data_dir, args.relay, args.token)?;
    let paths = &config.paths;
    let files = match import::Launch::resolve(&paths.open_socket_path, args.files) {
        import::Launch::Forwarded => return Ok(()),
        import::Launch::Primary(files) => files,
    };
    sync::set_local_device_name(config.device_name.clone());
    let store = Store::open(&paths.store_path)
        .change_context(Error)
        .attach_with(|| format!("opening store {}", paths.store_path.display()))?;
    let docs = Docs::load(store)
        .change_context(Error)
        .attach("loading workspace")?;
    // The store lock is held: this is the data dir's owner from here on.
    let inbox = import::Inbox::listen(&paths.open_socket_path, files);

    let runtime = Runtime::new(
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .change_context(Error)
            .attach("tokio runtime")?,
    );

    // The node accepts our own token plus the adopted workspace's, so one
    // QR opens every path. The UDP port is the one from the last run when
    // it is still free: peers keep dialling `ip:port` hints from the QR
    // and mDNS, and a restart must not invalidate them.
    let node_config = |bind_port| NodeConfig {
        key_path: paths.node_key_path.clone(),
        store_path: paths.node_store_path.clone(),
        device: config.device,
        tokens: [config.workspace_token.clone(), config.token.clone()]
            .into_iter()
            .filter(|t| !t.is_empty())
            .collect(),
        relay: config.relay_target(),
        bind_port,
        role: Role::Device,
    };
    let saved_port = config::load_node_port(&paths.node_port_path);
    let node = match runtime.block_on(Node::start(node_config(saved_port))) {
        Ok(node) => node,
        Err(err) if saved_port.is_some() => {
            tracing::warn!(%err, port = saved_port, "saved port unavailable; binding a fresh one");
            runtime
                .block_on(Node::start(node_config(None)))
                .change_context(Error)
                .attach("starting sync node")?
        }
        Err(err) => {
            return Err(err).change_context(Error).attach("starting sync node");
        }
    };
    if let Some(port) = runtime.block_on(node.bound_port()) {
        config::save_node_port(&paths.node_port_path, port);
    }
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
    let pair = krabink_core::PairInfo {
        node: node.id().to_string(),
        token: config.pair_token(),
        relay: config.relay_url(),
        addrs,
        replica: config.replica.map(|id| id.to_string()),
    };
    let sync_node = SyncNode::new(node.clone(), &runtime);
    let own_relay = config.relay_url();

    app::build(AppInputs {
        docs,
        inbox,
        theme: theme::Theme::new(config.theme, config.paper),
        sync_theme: config.sync_theme,
        runtime: runtime.clone(),
        sync_node,
        transport,
        settings: settings::Settings::new(pair, config.workspace_token.clone(), own_relay),
        follow_latest: args.follow_latest,
    })
    .run();
    // Final checkpoint and endpoint close; the endpoint must not be
    // dropped open.
    if let Err(err) = runtime.block_on(node.shutdown()) {
        tracing::warn!(%err, "node shutdown failed");
    }
    Ok(())
}
