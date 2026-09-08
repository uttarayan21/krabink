#[derive(Debug, clap::Parser)]
#[clap(version, about = "pendant desktop client")]
pub struct Cli {
    #[clap(subcommand)]
    pub cmd: Option<SubCommand>,
    /// Override the data directory (store + device id). Lets several
    /// instances run side by side.
    #[clap(long)]
    pub data_dir: Option<std::path::PathBuf>,
    /// Sync server url, e.g. ws://127.0.0.1:8722/ws. Overrides config.toml.
    #[clap(long)]
    pub server: Option<String>,
    /// Bearer token for the sync server. Overrides config.toml.
    #[clap(long)]
    pub token: Option<String>,
    /// Auto-open the most recently updated note whenever the library
    /// changes. Meant for demos and the replay latency rig.
    #[clap(long)]
    pub follow_latest: bool,
}

#[derive(Debug, clap::Subcommand)]
pub enum SubCommand {
    /// Generate shell completions.
    #[clap(name = "completions")]
    Completions { shell: clap_complete::Shell },
    /// Stream synthetic 120Hz pen strokes through the relay (latency rig).
    Replay {
        /// Sync server url, e.g. ws://127.0.0.1:8722/ws.
        #[clap(long)]
        server: String,
        /// Bearer token for the sync server.
        #[clap(long)]
        token: String,
        /// How many strokes to draw.
        #[clap(long, default_value_t = 3)]
        strokes: usize,
    },
}

impl Cli {
    pub fn completions(shell: clap_complete::Shell) {
        let mut command = <Cli as clap::CommandFactory>::command();
        clap_complete::generate(
            shell,
            &mut command,
            env!("CARGO_BIN_NAME"),
            &mut std::io::stdout(),
        );
    }
}
