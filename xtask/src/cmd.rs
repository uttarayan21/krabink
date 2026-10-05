//! Thin layer over `std::process::Command`: every task is mostly a sequence
//! of external tools (cargo, xcodebuild, xcrun, git, rsync), so this keeps
//! the "which command failed and how" in the error and the call sites short.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::error::{Error, Report, Result, ResultExt};

/// What a nix devShell exports for its cc-wrapper, as build settings
/// xcodebuild would pick up (each also as `<NAME>_FOR_BUILD`).
const NIX_TOOLCHAIN_VARS: [&str; 17] = [
    "AR",
    "AS",
    "CC",
    "CXX",
    "LD",
    "NM",
    "OBJCOPY",
    "OBJDUMP",
    "RANLIB",
    "SIZE",
    "STRINGS",
    "STRIP",
    "SDKROOT",
    "DEVELOPER_DIR",
    "MACOSX_DEPLOYMENT_TARGET",
    "LD_DYLD_PATH",
    "CONFIG_SHELL",
];

pub struct Cmd {
    inner: Command,
    display: Vec<String>,
}

impl Cmd {
    pub fn new(program: impl AsRef<OsStr>) -> Self {
        let program = program.as_ref();
        Self {
            inner: Command::new(program),
            display: vec![program.to_string_lossy().into_owned()],
        }
    }

    /// `cargo`, as the cargo that invoked us when run through the alias.
    pub fn cargo() -> Self {
        Self::new(std::env::var_os("CARGO").unwrap_or_else(|| OsString::from("cargo")))
    }

    pub fn git() -> Self {
        Self::new("git")
    }

    /// Apple's `xcrun`, with the nix devShell's toolchain overrides
    /// scrubbed. By absolute path: a nix devShell puts its own `xcrun`
    /// shim first on PATH, which knows nothing of devicectl or the iOS SDKs.
    pub fn xcrun() -> Self {
        Self::new("/usr/bin/xcrun").without_nix_toolchain()
    }

    /// Apple's `xcodebuild`, with the nix devShell's toolchain overrides
    /// scrubbed.
    pub fn xcodebuild() -> Self {
        Self::new("/usr/bin/xcodebuild").without_nix_toolchain()
    }

    /// Drop the toolchain variables a nix devShell (direnv, `nix develop`)
    /// exports. xcodebuild turns environment variables into build settings,
    /// so nix's `LD`, `CC`, `SDKROOT`, `DEVELOPER_DIR`, … would make Xcode
    /// drive nix's wrappers with Apple flags ("ld: unknown options:
    /// -Xlinker -isysroot …"), and xcrun would resolve into the nix SDK.
    pub fn without_nix_toolchain(mut self) -> Self {
        let scrubbed = std::env::vars_os().filter(|(key, _)| {
            let key = key.to_string_lossy();
            let base = key.strip_suffix("_FOR_BUILD").unwrap_or(&key);
            key.starts_with("NIX_") || NIX_TOOLCHAIN_VARS.contains(&base)
        });
        for (key, _) in scrubbed {
            self.inner.env_remove(key);
        }
        self
    }

    pub fn arg(mut self, arg: impl AsRef<OsStr>) -> Self {
        let arg = arg.as_ref();
        self.display.push(arg.to_string_lossy().into_owned());
        self.inner.arg(arg);
        self
    }

    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        for arg in args {
            self = self.arg(arg);
        }
        self
    }

    /// `arg` only when `value` is set: for optional xcodebuild settings.
    pub fn opt_arg(self, value: Option<impl AsRef<OsStr>>) -> Self {
        match value {
            Some(value) => self.arg(value),
            None => self,
        }
    }

    pub fn env(mut self, key: impl AsRef<OsStr>, value: impl AsRef<OsStr>) -> Self {
        self.inner.env(key, value);
        self
    }

    pub fn envs<I, K, V>(mut self, vars: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: AsRef<OsStr>,
        V: AsRef<OsStr>,
    {
        self.inner.envs(vars);
        self
    }

    pub fn env_clear(mut self) -> Self {
        self.inner.env_clear();
        self
    }

    pub fn current_dir(mut self, dir: impl AsRef<Path>) -> Self {
        self.inner.current_dir(dir);
        self
    }

    pub fn stdout(mut self, cfg: Stdio) -> Self {
        self.inner.stdout(cfg);
        self
    }

    fn describe(&self) -> String {
        self.display.join(" ")
    }

    fn program(&self) -> String {
        self.display[0].clone()
    }

    /// Run with inherited stdio and fail on a non-zero exit.
    pub fn run(mut self) -> Result<()> {
        tracing::debug!("$ {}", self.describe());
        let status = self
            .inner
            .status()
            .change_context_lazy(|| Error::Spawn(self.program()))?;
        if status.success() {
            Ok(())
        } else {
            Err(Report::new(Error::Exit {
                program: self.program(),
                status,
            })
            .attach(self.describe()))
        }
    }

    /// Run and report whether it exited 0, without failing: for retries and
    /// best-effort steps.
    pub fn succeeds(mut self) -> Result<bool> {
        tracing::debug!("$ {}", self.describe());
        self.inner
            .status()
            .map(|status| status.success())
            .change_context_lazy(|| Error::Spawn(self.program()))
    }

    /// Run with stdout captured (stderr inherited); trailing whitespace
    /// trimmed. Fails on a non-zero exit.
    pub fn output(mut self) -> Result<String> {
        tracing::debug!("$ {}", self.describe());
        let out = self
            .inner
            .stdout(Stdio::piped())
            .output()
            .change_context_lazy(|| Error::Spawn(self.program()))?;
        if !out.status.success() {
            return Err(Report::new(Error::Exit {
                program: self.program(),
                status: out.status,
            })
            .attach(self.describe()));
        }
        Ok(String::from_utf8_lossy(&out.stdout).trim_end().to_owned())
    }

    /// Like [`Cmd::output`] but a failure yields `None`: for queries whose
    /// non-zero exit is an answer (`git rev-parse --verify`).
    pub fn output_opt(mut self) -> Result<Option<String>> {
        tracing::debug!("$ {}", self.describe());
        let out = self
            .inner
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output()
            .change_context_lazy(|| Error::Spawn(self.program()))?;
        Ok(out
            .status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).trim_end().to_owned()))
    }

    /// Replace this process with the command (the last step of a task that
    /// just launches something, so signals go straight to it).
    #[cfg(unix)]
    pub fn exec(mut self) -> Result<()> {
        use std::os::unix::process::CommandExt;
        tracing::debug!("$ {}", self.describe());
        // `exec` only returns when it failed to replace the process.
        let err = self.inner.exec();
        Err(Report::new(err).change_context(Error::Spawn(self.program())))
    }

    pub fn into_inner(self) -> Command {
        self.inner
    }
}

/// First `name` on `PATH`, like `command -v`.
pub fn which(name: &str) -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|path| {
        std::env::split_paths(&path)
            .map(|dir| dir.join(name))
            .find(|candidate| candidate.is_file())
    })
}

/// Shell-quote for a command line that runs through `ssh`.
pub fn shell_quote(arg: &str) -> String {
    if !arg.is_empty()
        && arg
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_./=:,@+".contains(&b))
    {
        arg.to_owned()
    } else {
        format!("'{}'", arg.replace('\'', "'\\''"))
    }
}

#[cfg(test)]
mod tests {
    use super::shell_quote;

    #[test]
    fn quotes_only_when_needed() {
        assert_eq!(shell_quote("--destination=upload"), "--destination=upload");
        assert_eq!(shell_quote("a b"), "'a b'");
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
        assert_eq!(shell_quote(""), "''");
    }
}
