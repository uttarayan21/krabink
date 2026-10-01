//! Shared Apple tooling: xcrun lookups, keychain unlocking over ssh,
//! XcodeGen, and the App Store Connect export/upload step.

use std::fmt;
use std::path::{Path, PathBuf};

use crate::cmd::{Cmd, which};
use crate::error::{Error, Result, ResultExt};
use crate::repo::{Repo, TEAM};

/// `xcrun --sdk <sdk> --show-sdk-path`.
pub fn sdk_path(sdk: &str) -> Result<String> {
    Cmd::xcrun()
        .args(["--sdk", sdk, "--show-sdk-path"])
        .output()
}

/// `xcrun [--sdk <sdk>] --find <tool>`.
pub fn find(sdk: Option<&str>, tool: &str) -> Result<String> {
    let cmd = match sdk {
        Some(sdk) => Cmd::xcrun().args(["--sdk", sdk]),
        None => Cmd::xcrun(),
    };
    cmd.args(["--find", tool]).output()
}

/// Over ssh there is no GUI keychain session, so codesign cannot reach the
/// signing identity until the keychains holding it are unlocked. A GUI
/// login already has them open; then this is a no-op. The password comes
/// from `~/.keychain-pw` on the Mac; without that file nothing happens.
pub fn unlock_keychains() -> Result<()> {
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
        return Ok(());
    };
    let pw_file = home.join(".keychain-pw");
    if !pw_file.is_file() {
        return Ok(());
    }
    let pw =
        std::fs::read_to_string(&pw_file).change_context_lazy(|| Error::Io(pw_file.clone()))?;
    let pw = pw.trim_end_matches(['\n', '\r']);
    for name in ["login", "rscad-codesign"] {
        let keychain = home.join(format!("Library/Keychains/{name}.keychain-db"));
        if !keychain.is_file() {
            continue;
        }
        let unlocked = Cmd::new("security")
            .args(["unlock-keychain", "-p", pw])
            .arg(&keychain)
            .succeeds()?;
        if !unlocked {
            tracing::warn!("warning: could not unlock {name} keychain");
        }
    }
    Ok(())
}

/// `xcodegen generate` in `dir`, from PATH or through nix.
pub fn xcodegen(dir: &Path, quiet: bool) -> Result<()> {
    let cmd = match which("xcodegen") {
        Some(bin) => Cmd::new(bin).arg("generate"),
        None => Cmd::new("nix").args(["run", "nixpkgs#xcodegen", "--", "generate"]),
    };
    let cmd = if quiet { cmd.arg("--quiet") } else { cmd };
    cmd.current_dir(dir).run()
}

/// Where `xcodebuild -exportArchive` sends the result.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Destination {
    /// Write the .ipa / .pkg under build-archive/export.
    Export,
    /// Upload to App Store Connect (TestFlight).
    Upload,
}

impl fmt::Display for Destination {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Export => "export",
            Self::Upload => "upload",
        })
    }
}

/// App Store Connect API key, for uploads without an Xcode account session
/// on the Mac (the `.p8` path is on the Mac). Without it, upload uses the
/// Xcode account session.
#[derive(Debug, Clone, clap::Args)]
pub struct AscAuth {
    /// Path to the AuthKey_<id>.p8 on the Mac.
    #[arg(long, env = "KRABINK_ASC_KEY_PATH", requires_all = ["asc_key_id", "asc_issuer_id"])]
    pub asc_key_path: Option<PathBuf>,
    #[arg(long, env = "KRABINK_ASC_KEY_ID")]
    pub asc_key_id: Option<String>,
    #[arg(long, env = "KRABINK_ASC_ISSUER_ID")]
    pub asc_issuer_id: Option<String>,
}

impl AscAuth {
    fn args(&self) -> Vec<String> {
        match (&self.asc_key_path, &self.asc_key_id, &self.asc_issuer_id) {
            (Some(path), Some(id), Some(issuer)) => vec![
                "-authenticationKeyPath".into(),
                path.display().to_string(),
                "-authenticationKeyID".into(),
                id.clone(),
                "-authenticationKeyIssuerID".into(),
                issuer.clone(),
            ],
            _ => Vec::new(),
        }
    }
}

/// Options shared by `archive-ios` and `archive-macos`.
#[derive(Debug, Clone, clap::Args)]
pub struct ArchiveArgs {
    /// DEVELOPMENT_TEAM for automatic signing (needs a paid Apple Developer
    /// Program membership for app-store-connect).
    #[arg(long, env = "KRABINK_TEAM", default_value = TEAM)]
    pub team: String,
    /// Export method: app-store-connect, release-testing (ad hoc),
    /// debugging (development), developer-id (Mac).
    #[arg(
        long,
        env = "KRABINK_EXPORT_METHOD",
        default_value = "app-store-connect"
    )]
    pub method: String,
    #[arg(long, env = "KRABINK_EXPORT_DESTINATION", default_value_t = Destination::Export, value_enum)]
    pub destination: Destination,
    /// CFBundleVersion (default: commit count).
    #[arg(long, env = "KRABINK_BUILD_NUMBER")]
    pub build_number: Option<u64>,
    /// CFBundleShortVersionString (default: project.yml).
    #[arg(long, env = "KRABINK_MARKETING_VERSION")]
    pub marketing_version: Option<String>,
    #[command(flatten)]
    pub auth: AscAuth,
}

impl ArchiveArgs {
    /// The build number, resolved from git when not given. Computed before
    /// any Mac handoff: the mirror only has a throwaway repo.
    pub fn build_number(&self, repo: &Repo) -> Result<u64> {
        match self.build_number {
            Some(n) => Ok(n),
            None => repo.commit_count(),
        }
    }

    /// `MARKETING_VERSION=<v>` for xcodebuild when overridden.
    pub fn marketing_setting(&self) -> Option<String> {
        self.marketing_version
            .as_ref()
            .map(|v| format!("MARKETING_VERSION={v}"))
    }

    pub fn describe_build(&self, build: u64) -> String {
        match &self.marketing_version {
            Some(v) => format!("build {build}, version {v}"),
            None => format!("build {build}"),
        }
    }
}

/// The `ExportOptions.plist` for `xcodebuild -exportArchive`.
/// `manageAppVersionAndBuildNumber=false` keeps the build number we set
/// instead of letting Xcode bump it to whatever ASC last saw.
pub struct ExportOptions<'a> {
    pub method: &'a str,
    pub destination: Destination,
    pub team: &'a str,
    pub upload_symbols: bool,
}

impl ExportOptions<'_> {
    pub fn write(&self, path: &Path) -> Result<()> {
        let symbols = if self.upload_symbols { "true" } else { "false" };
        let plist = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>method</key>
	<string>{method}</string>
	<key>destination</key>
	<string>{destination}</string>
	<key>teamID</key>
	<string>{team}</string>
	<key>signingStyle</key>
	<string>automatic</string>
	<key>uploadSymbols</key>
	<{symbols}/>
	<key>manageAppVersionAndBuildNumber</key>
	<false/>
</dict>
</plist>
"#,
            method = self.method,
            destination = self.destination,
            team = self.team,
        );
        std::fs::write(path, plist).change_context_lazy(|| Error::Io(path.to_owned()))
    }
}

/// `xcodebuild -exportArchive` into a fresh `export_dir`.
pub fn export(archive: &Path, options: &Path, export_dir: &Path, auth: &AscAuth) -> Result<()> {
    remove_dir(export_dir)?;
    Cmd::new("xcodebuild")
        .arg("-exportArchive")
        .arg("-archivePath")
        .arg(archive)
        .arg("-exportOptionsPlist")
        .arg(options)
        .arg("-exportPath")
        .arg(export_dir)
        .arg("-allowProvisioningUpdates")
        .args(auth.args())
        .run()
}

/// `rm -rf`, fine with a missing path.
pub fn remove_dir(path: &Path) -> Result<()> {
    match std::fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).change_context_lazy(|| Error::Io(path.to_owned())),
    }
}

pub fn create_dir(path: &Path) -> Result<()> {
    std::fs::create_dir_all(path).change_context_lazy(|| Error::Io(path.to_owned()))
}

pub fn copy(from: &Path, to: &Path) -> Result<()> {
    std::fs::copy(from, to)
        .map(|_| ())
        .change_context_lazy(|| Error::Io(to.to_owned()))
        .attach_with(|| format!("copying {}", from.display()))
}

/// The first `*.<ext>` directly under `dir`, for the "exported …" message.
pub fn find_with_extension(dir: &Path, ext: &str) -> Option<PathBuf> {
    std::fs::read_dir(dir).ok().and_then(|entries| {
        entries
            .filter_map(|e| e.ok().map(|e| e.path()))
            .find(|p| p.extension().is_some_and(|e| e == ext))
    })
}
