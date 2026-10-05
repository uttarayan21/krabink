//! Running Apple tasks from Linux: mirror the worktree to the Mac build
//! machine with rsync and run the same `cargo xtask` invocation there over
//! ssh, streaming its output.
//!
//!   KRABINK_MAC_HOST  ssh host (default: shiro)
//!   KRABINK_MAC_DIR   remote checkout, relative to the remote $HOME
//!                     (default: Porject/krabink-<worktree dir name>, or
//!                     Porject/krabink when the checkout is named krabink)
//!
//! The KRABINK_* task variables are forwarded, so `KRABINK_TEAM=… cargo
//! xtask archive ios` behaves the same from either side.

use std::io::IsTerminal;

use crate::cmd::{Cmd, shell_quote};
use crate::error::Result;
use crate::repo::Repo;

const DEFAULT_HOST: &str = "shiro";

/// Task variables that cross to the Mac when set.
const FORWARDED_ENV: [&str; 12] = [
    "KRABINK_IPAD_ID",
    "KRABINK_TEAM",
    "KRABINK_SIM_ID",
    "KRABINK_CONFIGURATION",
    "KRABINK_BUILD_NUMBER",
    "KRABINK_MARKETING_VERSION",
    "KRABINK_EXPORT_METHOD",
    "KRABINK_EXPORT_DESTINATION",
    "KRABINK_ASC_KEY_PATH",
    "KRABINK_ASC_KEY_ID",
    "KRABINK_ASC_ISSUER_ID",
    "RUST_LOG",
];

/// Paths that never leave this machine. Excluded paths are also never
/// deleted remotely, so cargo's target/, the generated Xcode projects and
/// the built xcframework survive between runs.
const EXCLUDES: [&str; 11] = [
    ".git",
    "target",
    ".direnv",
    "result*",
    "ios/Krabink/build",
    "ios/Krabink/build-*",
    "ios/Krabink/Krabink.xcodeproj",
    "ios/Krabink/Info.plist",
    "ios/KrabinkCore/KrabinkCoreFFI.xcframework",
    "ios/KrabinkCore/Sources",
    "ios/KrabinkCore/.build",
];

/// On macOS: nothing to do, returns `false`. Elsewhere: run this very
/// invocation (plus `extra_args`, with `extra_env` set) on the Mac and
/// return `true`, so the caller just returns.
pub fn handoff(repo: &Repo, extra_env: &[(&str, String)], extra_args: &[&str]) -> Result<bool> {
    if cfg!(target_os = "macos") {
        return Ok(false);
    }

    let host = std::env::var("KRABINK_MAC_HOST").unwrap_or_else(|_| DEFAULT_HOST.to_owned());
    let dir = std::env::var("KRABINK_MAC_DIR").unwrap_or_else(|_| {
        match repo.root().file_name().map(|n| n.to_string_lossy()) {
            Some(name) if name == "krabink" => "Porject/krabink".to_owned(),
            Some(name) => format!("Porject/krabink-{name}"),
            None => "Porject/krabink".to_owned(),
        }
    });

    tracing::info!("==> syncing to {host}:{dir}");
    Cmd::new("ssh")
        .arg(&host)
        .arg(format!("mkdir -p {}", shell_quote(&dir)))
        .run()?;
    // The Mac ships openrsync (no --filter, no --info), so plain excludes only.
    Cmd::new("rsync")
        .args(["-az", "--delete"])
        .args(EXCLUDES.iter().flat_map(|e| ["--exclude", e]))
        .arg(format!("{}/", repo.root().display()))
        .arg(format!("{host}:{dir}/"))
        .run()?;

    let env: String = FORWARDED_ENV
        .iter()
        .filter_map(|var| std::env::var(var).ok().map(|value| (*var, value)))
        .chain(extra_env.iter().map(|(k, v)| (*k, v.clone())))
        .map(|(k, v)| format!(" {k}={}", shell_quote(&v)))
        .collect();
    let args: String = std::env::args()
        .skip(1)
        .chain(extra_args.iter().map(|a| (*a).to_owned()))
        .map(|a| format!(" {}", shell_quote(&a)))
        .collect();

    // The mirror has no .git (a worktree's .git is a pointer into the main
    // checkout). Nix flake commands on the Mac want a git tree, so keep a
    // throwaway repo there with everything staged.
    let remote = format!(
        "cd {dir} && {{ [ -d .git ] || git init -q; }} && git add -A && env{env} cargo xtask{args}",
        dir = shell_quote(&dir)
    );
    let ssh = if std::io::stdin().is_terminal() {
        Cmd::new("ssh").arg("-t")
    } else {
        Cmd::new("ssh")
    };
    ssh.arg(&host).arg(remote).run()?;
    Ok(true)
}
