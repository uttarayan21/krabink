#[derive(Debug, clap::Parser)]
#[clap(version, about = "pendant desktop client")]
pub struct Cli {
    #[clap(subcommand)]
    pub cmd: Option<SubCommand>,
    /// Override the data directory (store + device id). Lets several
    /// instances run side by side.
    #[clap(long)]
    pub data_dir: Option<std::path::PathBuf>,
    /// Dedicated relay url, e.g. wss://relay.example.com/ws, used as the
    /// fallback path when a device cannot reach this desktop's embedded
    /// relay directly. Overrides config.toml.
    #[clap(long)]
    pub server: Option<String>,
    /// Bearer token for the dedicated relay. Overrides config.toml.
    #[clap(long)]
    pub token: Option<String>,
    /// Address the embedded relay listens on. Port 0 (the default) picks
    /// a free port each launch so the app never collides with a dedicated
    /// pendant-server on 8722; paired devices re-find it over mDNS. Pin a
    /// port only for firewall rules or scripted clients (falls back to an
    /// ephemeral port when taken).
    #[clap(long, default_value = crate::relay::DEFAULT_LISTEN)]
    pub relay_listen: std::net::SocketAddr,
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
    /// Adopt sync settings from a `pendant://pair` URI (QR pairing) by
    /// writing them to config.toml.
    Pair {
        /// URI shown next to the QR code on the sharing device.
        uri: String,
    },
    /// Replay recorded strokes through the input models and presets and
    /// write SVG grids + metrics.json (the brush engine's tuning bench).
    BrushLab {
        /// A recording (`# pendant-stroke v2` text) or a directory of them.
        #[clap(long)]
        corpus: std::path::PathBuf,
        /// Columns: tool names (pen, pencil, marker, monoline, fountain),
        /// bundled brush ids (builtin:crayon) or `self`, the recording's
        /// own tool.
        #[clap(long, value_delimiter = ',', default_value = "self")]
        presets: Vec<String>,
        /// Rows: ema, ism (needs the `ism` feature).
        #[clap(long, value_delimiter = ',', default_value = "ema")]
        models: Vec<String>,
        /// Output directory.
        #[clap(long, default_value = "target/lab")]
        out: std::path::PathBuf,
        /// Write one SVG per recording.
        #[clap(long)]
        svg: bool,
        /// Write metrics.json.
        #[clap(long)]
        metrics: bool,
    },
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
