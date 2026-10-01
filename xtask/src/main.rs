//! Repository tasks, run as `cargo xtask <task>` (alias in
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
    /// Run the desktop app (release build); arguments go to the binary.
    Run(desktop::RunArgs),
    /// Build the KrabinkCore XCFramework and Swift bindings.
    BuildIosCore,
    /// Compile and run the Swift bindings smoke test.
    SwiftSmoke,
    /// Regenerate ios/Krabink/Krabink.xcodeproj from project.yml.
    GenXcodeproj,
    /// Compile the iPad app for the simulator (no signing).
    CheckIpad(ipad::CheckArgs),
    /// Build, install and launch on a connected iPad.
    RunIpad(ipad::DeployArgs),
    /// Archive the iPad app and export it for (or upload it to) App Store Connect.
    ArchiveIos(apple::ArchiveArgs),
    /// Archive the Mac app and export the Mac App Store .pkg (or upload it).
    ArchiveMacos(apple::ArchiveArgs),
    /// Upload the tagged version to App Store Connect, both platforms.
    Release(release::ReleaseArgs),
    /// Render the 1024px App Store icon.
    GenAppIcon(icon::AppIconArgs),
    /// Render the Mac icon set from the iPad icon.
    GenMacIcon,
    /// Bump the app version in Cargo.toml, Cargo.lock and both project.yml.
    Bump {
        #[arg(value_enum)]
        part: Part,
    },
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
        Task::Run(args) => desktop::run(&repo, args),
        Task::BuildIosCore => ios_core::build(&repo),
        Task::SwiftSmoke => swift_smoke::run(&repo),
        Task::GenXcodeproj => xcodeproj::generate(&repo),
        Task::CheckIpad(args) => ipad::check(&repo, args),
        Task::RunIpad(args) => ipad::deploy(&repo, args),
        Task::ArchiveIos(args) => archive::ios(&repo, args),
        Task::ArchiveMacos(args) => archive::macos(&repo, args),
        Task::Release(args) => release::run(&repo, args),
        Task::GenAppIcon(args) => icon::app_icon(&repo, args),
        Task::GenMacIcon => icon::mac_icons(&repo),
        Task::Bump { part } => bump::run(&repo, *part),
    }
}
