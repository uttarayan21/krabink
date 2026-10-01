//! One error context for every task. Details travel as `error-stack`
//! attachments, so a failed `xcodebuild` shows the command, its exit status
//! and the step it was part of.

use std::path::PathBuf;
use std::process::ExitStatus;

pub use error_stack::{Report, ResultExt};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("`{0}` is macOS only")]
    MacOnly(&'static str),
    #[error("could not start `{0}`")]
    Spawn(String),
    #[error("`{program}` exited with {status}")]
    Exit { program: String, status: ExitStatus },
    #[error("io error on {0}")]
    Io(PathBuf),
    #[error("git query failed")]
    Git,
    #[error("can't parse version `{0}`")]
    Version(String),
    #[error("release check failed")]
    Check,
    #[error("{0} not found")]
    Missing(PathBuf),
    #[error("no iPad found")]
    Device,
    #[error("icon rendering failed")]
    Icon,
    #[error("no display: log in graphically first")]
    Display,
}

pub type Result<T, E = Report<Error>> = core::result::Result<T, E>;
