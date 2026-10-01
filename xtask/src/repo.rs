//! The checkout: paths, git queries and the workspace version.

use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use crate::cmd::Cmd;
use crate::error::{Error, Report, Result, ResultExt};

/// Apple Developer team for automatic signing (`DEVELOPMENT_TEAM`).
pub const TEAM: &str = "YD2FVR5QH2";
pub const BUNDLE_ID: &str = "dev.darksailor.krabink";
pub const IOS_APP_DIR: &str = "ios/Krabink";
pub const IOS_CORE_DIR: &str = "ios/KrabinkCore";
pub const MACOS_APP_DIR: &str = "macos";
/// XcodeGen specs carrying `MARKETING_VERSION`, kept equal to `Cargo.toml`.
pub const PROJECT_SPECS: [&str; 2] = ["ios/Krabink/project.yml", "macos/project.yml"];

pub struct Repo {
    root: PathBuf,
}

impl Repo {
    /// The repository containing the current directory.
    pub fn discover() -> Result<Self> {
        let root = Cmd::git()
            .args(["rev-parse", "--show-toplevel"])
            .output()
            .change_context(Error::Git)
            .attach("run from inside the krabink checkout")?;
        Ok(Self {
            root: PathBuf::from(root),
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn path(&self, rel: impl AsRef<Path>) -> PathBuf {
        self.root.join(rel)
    }

    /// `cargo` in the repository root.
    pub fn cargo(&self) -> Cmd {
        Cmd::cargo().current_dir(&self.root)
    }

    pub fn git(&self) -> Cmd {
        Cmd::git().current_dir(&self.root)
    }

    pub fn read(&self, rel: impl AsRef<Path>) -> Result<String> {
        let path = self.path(rel);
        std::fs::read_to_string(&path).change_context_lazy(|| Error::Io(path.clone()))
    }

    pub fn write(&self, rel: impl AsRef<Path>, contents: impl AsRef<[u8]>) -> Result<()> {
        let path = self.path(rel);
        std::fs::write(&path, contents).change_context_lazy(|| Error::Io(path.clone()))
    }

    /// `[workspace.package].version` from `Cargo.toml`.
    pub fn version(&self) -> Result<Version> {
        parse_workspace_version(&self.read("Cargo.toml")?)
    }

    /// Commit count on HEAD: the build number (`CFBundleVersion`).
    pub fn commit_count(&self) -> Result<u64> {
        let count = self
            .git()
            .args(["rev-list", "--count", "HEAD"])
            .output()
            .change_context(Error::Git)?;
        count
            .parse()
            .change_context(Error::Git)
            .attach_with(|| format!("unexpected commit count `{count}`"))
    }

    pub fn head(&self) -> Result<String> {
        self.git()
            .args(["rev-parse", "HEAD"])
            .output()
            .change_context(Error::Git)
    }

    /// The commit a tag points at, if the tag exists.
    pub fn tag_commit(&self, tag: &str) -> Result<Option<String>> {
        self.git()
            .args([
                "rev-parse",
                "-q",
                "--verify",
                &format!("refs/tags/{tag}^{{commit}}"),
            ])
            .output_opt()
            .change_context(Error::Git)
    }

    pub fn is_clean(&self) -> Result<bool> {
        self.git()
            .args(["status", "--porcelain"])
            .output()
            .map(|status| status.is_empty())
            .change_context(Error::Git)
    }
}

/// The first top-level `version = "…"` line of a `Cargo.toml`.
pub fn parse_workspace_version(cargo_toml: &str) -> Result<Version> {
    cargo_toml
        .lines()
        .find_map(|line| {
            line.strip_prefix("version = \"")
                .and_then(|rest| rest.strip_suffix('"'))
        })
        .ok_or_else(|| Report::new(Error::Version("<missing>".into())))
        .attach("no `version = \"…\"` line in Cargo.toml")?
        .parse()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Version {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
}

impl Version {
    pub fn bump(self, part: Part) -> Self {
        match part {
            Part::Major => Self {
                major: self.major + 1,
                minor: 0,
                patch: 0,
            },
            Part::Minor => Self {
                minor: self.minor + 1,
                patch: 0,
                ..self
            },
            Part::Patch => Self {
                patch: self.patch + 1,
                ..self
            },
        }
    }

    /// The git tag for this version.
    pub fn tag(self) -> String {
        format!("v{self}")
    }
}

impl FromStr for Version {
    type Err = Report<Error>;

    fn from_str(s: &str) -> Result<Self> {
        let invalid = || Report::new(Error::Version(s.to_owned()));
        let mut parts = s.split('.').map(|part| part.parse::<u64>().ok());
        match (parts.next(), parts.next(), parts.next(), parts.next()) {
            (Some(Some(major)), Some(Some(minor)), Some(Some(patch)), None) => Ok(Self {
                major,
                minor,
                patch,
            }),
            _ => Err(invalid()),
        }
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Part {
    Major,
    Minor,
    Patch,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_bumps() {
        let v: Version = "0.1.2".parse().unwrap();
        assert_eq!(v.bump(Part::Patch).to_string(), "0.1.3");
        assert_eq!(v.bump(Part::Minor).to_string(), "0.2.0");
        assert_eq!(v.bump(Part::Major).to_string(), "1.0.0");
        assert_eq!(v.tag(), "v0.1.2");
        assert!("0.1".parse::<Version>().is_err());
        assert!("0.1.2.3".parse::<Version>().is_err());
        assert!("0.1.x".parse::<Version>().is_err());
    }

    #[test]
    fn reads_the_workspace_version() {
        let toml = "[workspace]\nmembers = [\"crates/*\"]\n\n[workspace.package]\nversion = \"0.1.2\"\nedition = \"2024\"\n";
        assert_eq!(parse_workspace_version(toml).unwrap().to_string(), "0.1.2");
        assert!(parse_workspace_version("[workspace]\n").is_err());
    }
}
