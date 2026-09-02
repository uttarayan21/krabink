#[derive(Debug, clap::Parser)]
pub struct Cli {
    #[clap(subcommand)]
    pub cmd: SubCommand,
}

#[derive(Debug, clap::Subcommand)]
pub enum SubCommand {
    #[clap(name = "add")]
    Add(Add),
    #[clap(name = "list")]
    List(List),
    #[clap(name = "completions")]
    Completions { shell: clap_complete::Shell },
}

#[derive(Debug, clap::Args)]
pub struct Add {
    #[clap(short, long)]
    pub name: String,
}

#[derive(Debug, clap::Args)]
pub struct List {}

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
