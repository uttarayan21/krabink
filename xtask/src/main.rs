//! Repository tasks, run as `cargo xtask <group> <task>` (alias in
//! .cargo/config.toml). The Apple tasks need Xcode; started on Linux they
//! mirror the worktree to the Mac build machine and run there (see `mac`).

mod apple;
mod archive;
mod bump;
mod cmd;
mod desktop;
mod error;
mod icon;
mod ios_core;
mod ipad;
mod mac;
mod release;
mod repo;
mod swift_smoke;
mod xcodeproj;

use std::process::ExitCode;

use clap::Parser;
use tracing_subscriber::EnvFilter;

use crate::error::Result;
use crate::repo::{Part, Repo};

#[derive(Debug, Parser)]
#[command(name = "cargo xtask", about = "krabink repository tasks", version)]
struct Cli {
    #[command(subcommand)]
    task: Task,
}

#[derive(Debug, clap::Subcommand)]
enum Task {
    /// Run an app.
    #[command(subcommand)]
    Run(Run),
    /// Build a component.
    #[command(subcommand)]
    Build(Build),
    /// Compile checks that need no device.
    #[command(subcommand)]
    Check(Check),
    /// Smoke tests.
    #[command(subcommand)]
    Test(Test),
    /// Generate committed or ignored sources.
    #[command(subcommand)]
    Gen(Gen),
    /// Release archives for App Store Connect.
    #[command(subcommand)]
    Archive(Archive),
    /// Upload the tagged version to App Store Connect, both platforms.
    Release(release::ReleaseArgs),
    /// Bump the app version in Cargo.toml, Cargo.lock and both project.yml.
    Bump {
        #[arg(value_enum)]
        part: Part,
    },
}

#[derive(Debug, clap::Subcommand)]
enum Run {
    /// The desktop app (release build); arguments go to the binary.
    Desktop(desktop::RunArgs),
    /// Build, install and launch the iPad app on a connected device.
    Ios(ipad::DeployArgs),
}

#[derive(Debug, clap::Subcommand)]
enum Build {
    /// The KrabinkCore XCFramework and Swift bindings for the iPad app.
    Core,
}

#[derive(Debug, clap::Subcommand)]
enum Check {
    /// Compile the iPad app for the simulator (no signing).
    Ios(ipad::CheckArgs),
}

#[derive(Debug, clap::Subcommand)]
enum Test {
    /// Compile and run the Swift bindings smoke test.
    Swift,
}

#[derive(Debug, clap::Subcommand)]
enum Gen {
    /// Regenerate ios/Krabink/Krabink.xcodeproj from project.yml.
    Xcodeproj,
    /// App icons.
    #[command(subcommand)]
    Icon(Icon),
}

#[derive(Debug, clap::Subcommand)]
enum Icon {
    /// Render the 1024px App Store icon for the iPad app.
    Ios(icon::AppIconArgs),
    /// Render the Mac icon set from the iPad icon.
    Mac,
}

#[derive(Debug, clap::Subcommand)]
enum Archive {
    /// Archive the iPad app and export it for (or upload it to) App Store Connect.
    Ios(apple::ArchiveArgs),
    /// Archive the Mac app and export the Mac App Store .pkg (or upload it).
    Mac(apple::ArchiveArgs),
}

fn main() -> ExitCode {
    // Reports go through tracing, which does not colour; keep the escape
    // codes out of logs.
    error_stack::Report::set_color_mode(error_stack::fmt::ColorMode::None);
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .without_time()
        .with_target(false)
        .with_level(false)
        .init();

    match run(Cli::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(report) => {
            tracing::error!("error: {report:?}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> Result<()> {
    let repo = Repo::discover()?;
    match &cli.task {
        Task::Run(Run::Desktop(args)) => desktop::run(&repo, args),
        Task::Run(Run::Ios(args)) => ipad::deploy(&repo, args),
        Task::Build(Build::Core) => ios_core::build(&repo),
        Task::Check(Check::Ios(args)) => ipad::check(&repo, args),
        Task::Test(Test::Swift) => swift_smoke::run(&repo),
        Task::Gen(Gen::Xcodeproj) => xcodeproj::generate(&repo),
        Task::Gen(Gen::Icon(Icon::Ios(args))) => icon::app_icon(&repo, args),
        Task::Gen(Gen::Icon(Icon::Mac)) => icon::mac_icons(&repo),
        Task::Archive(Archive::Ios(args)) => archive::ios(&repo, args),
        Task::Archive(Archive::Mac(args)) => archive::macos(&repo, args),
        Task::Release(args) => release::run(&repo, args),
        Task::Bump { part } => bump::run(&repo, *part),
    }
}
