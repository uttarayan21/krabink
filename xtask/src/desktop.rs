//! `run`: `cargo run` for the desktop app, for paseo's "run-desktop" script
//! and by hand. Paseo's script runner inherits the paseo server's
//! environment, which on Linux is a user service with no graphical session
//! variables, so winit panics with "neither WAYLAND_DISPLAY nor
//! WAYLAND_SOCKET nor DISPLAY is set". Point it at the running compositor's
//! socket (or Xwayland) first.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use crate::cmd::{Cmd, which};
use crate::error::{Error, Report, Result};
use crate::repo::Repo;

/// Loro logs every encoded block at INFO; keep the terminal readable.
const DEFAULT_RUST_LOG: &str = "info,loro_internal=warn,wgpu=error,naga=warn";

#[derive(Debug, Clone, clap::Args)]
pub struct RunArgs {
    /// Arguments for the krabink binary, e.g. `--data-dir /tmp/b`.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub args: Vec<OsString>,
}

pub fn run(repo: &Repo, args: &RunArgs) -> Result<()> {
    let mut env: Vec<(String, OsString)> = Vec::new();
    let headless =
        std::env::var_os("WAYLAND_DISPLAY").is_none() && std::env::var_os("DISPLAY").is_none();
    if cfg!(target_os = "linux") && headless {
        env.extend(display_env()?);
    }
    if std::env::var_os("RUST_LOG").is_none() {
        env.push(("RUST_LOG".into(), DEFAULT_RUST_LOG.into()));
    }

    // The toolchain's linker (`cc`) and Bevy's system libs come from the
    // flake devShell; paseo's env has neither, so enter the shell when
    // needed.
    let run_args = ["run", "-r", "-p", "krabink", "--"];
    let cmd = if which("cc").is_some() {
        Cmd::cargo().args(run_args)
    } else if which("direnv").is_some() {
        Cmd::new("direnv")
            .args(["exec", ".", "cargo"])
            .args(run_args)
    } else {
        Cmd::new("nix")
            .args(["develop", "--command", "cargo"])
            .args(run_args)
    };
    let cmd = cmd.args(&args.args).envs(env).current_dir(repo.root());
    #[cfg(unix)]
    {
        cmd.exec()
    }
    #[cfg(not(unix))]
    {
        cmd.run()
    }
}

/// WAYLAND_DISPLAY from the first `wayland-*` socket in XDG_RUNTIME_DIR, or
/// DISPLAY=:0 when an X socket exists.
fn display_env() -> Result<Vec<(String, OsString)>> {
    let mut env = Vec::new();
    let runtime_dir = match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(dir) => PathBuf::from(dir),
        None => {
            let dir = PathBuf::from(format!("/run/user/{}", uid()));
            env.push(("XDG_RUNTIME_DIR".to_owned(), dir.clone().into_os_string()));
            dir
        }
    };
    if let Some(socket) = wayland_socket(&runtime_dir) {
        env.push(("WAYLAND_DISPLAY".to_owned(), socket));
    } else if Path::new("/tmp/.X11-unix/X0").exists() {
        env.push(("DISPLAY".to_owned(), OsString::from(":0")));
    } else {
        return Err(Report::new(Error::Display).attach(format!(
            "no Wayland socket in {} and no X display",
            runtime_dir.display()
        )));
    }
    let shown: Vec<String> = env
        .iter()
        .map(|(k, v)| format!("{k}={}", v.to_string_lossy()))
        .collect();
    tracing::info!("==> display: {}", shown.join(" "));
    Ok(env)
}

fn wayland_socket(runtime_dir: &Path) -> Option<OsString> {
    let mut sockets: Vec<OsString> = std::fs::read_dir(runtime_dir)
        .ok()?
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().starts_with("wayland-"))
        .filter(|e| is_socket(&e.path()))
        .map(|e| e.file_name())
        .collect();
    sockets.sort();
    sockets.into_iter().next()
}

#[cfg(unix)]
fn is_socket(path: &Path) -> bool {
    use std::os::unix::fs::FileTypeExt;
    std::fs::metadata(path).is_ok_and(|m| m.file_type().is_socket())
}

#[cfg(not(unix))]
fn is_socket(_path: &Path) -> bool {
    false
}

fn uid() -> String {
    Cmd::new("id")
        .arg("-u")
        .output()
        .unwrap_or_else(|_| "1000".to_owned())
}
