#[derive(Debug, clap::Parser)]
#[clap(version, about = "krabink desktop client")]
pub struct Cli {
    #[clap(subcommand)]
    pub cmd: Option<SubCommand>,
    /// Override the data directory (stores, node key, device id). Lets
    /// several instances run side by side.
    #[clap(long)]
    pub data_dir: Option<std::path::PathBuf>,
    /// Home relay URL, e.g. https://relay.example.org: brokers the
    /// handshake with peers and carries traffic when hole punching fails.
    /// Overrides config.toml.
    #[clap(long)]
    pub relay: Option<String>,
    /// Workspace token (relay access + peer handshake). Overrides
    /// config.toml.
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
    /// Adopt sync settings from a `krabink://pair` URI (QR pairing) by
    /// writing them to config.toml.
    Pair {
        /// URI shown next to the QR code on the sharing device.
        uri: String,
    },
    /// Replay recorded strokes through the input models and presets and
    /// write SVG grids + metrics.json (the brush engine's tuning bench).
    BrushLab {
        /// A recording (`# krabink-stroke v2` text) or a directory of them.
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
    /// Stream synthetic 120Hz pen strokes to a paired node (latency rig).
    Replay {
        /// The target's `krabink://pair` URI (from its settings screen).
        #[clap(long)]
        pair: String,
        /// How many strokes to draw.
        #[clap(long, default_value_t = 3)]
        strokes: usize,
        /// Hover a pen pointer over each stroke's start before drawing it,
        /// and keep it on the tip while drawing.
        #[clap(long)]
        pointer: bool,
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
