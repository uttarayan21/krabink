//! `check-ipad` (simulator compile, no signing) and `run-ipad` (device
//! build + install + launch). Both need Xcode, so from Linux they run on
//! the Mac.

use std::io::{BufRead, BufReader};
use std::process::Stdio;
use std::time::Duration;

use crate::apple::unlock_keychains;
use crate::cmd::Cmd;
use crate::error::{Error, Report, Result, ResultExt};
use crate::repo::{BUNDLE_ID, IOS_APP_DIR, Repo, TEAM};
use crate::{ios_core, mac, xcodeproj};

/// The iPad used when none is connected and none is given.
const DEFAULT_IPAD: &str = "E894963B-F801-5AFA-B709-3F61603301BA";

#[derive(Debug, Clone, clap::Args)]
pub struct CheckArgs {
    /// Simulator UDID to target (default: generic simulator, which needs no
    /// booted device).
    #[arg(long, env = "KRABINK_SIM_ID")]
    pub sim_id: Option<String>,
    /// Debug, or Release to check the store build (iPad-only, dev screens
    /// compiled out).
    #[arg(long, env = "KRABINK_CONFIGURATION", default_value = "Debug")]
    pub configuration: String,
}

pub fn check(repo: &Repo, args: &CheckArgs) -> Result<()> {
    if mac::handoff(repo, &[], &[])? {
        return Ok(());
    }
    ios_core::ensure(repo)?;
    xcodeproj::generate(repo)?;

    let destination = match &args.sim_id {
        Some(id) => format!("platform=iOS Simulator,id={id}"),
        None => "generic/platform=iOS Simulator".to_owned(),
    };

    tracing::info!("==> building for simulator");
    // ARCHS=arm64: the generic simulator destination also wants x86_64, and
    // the KrabinkCore xcframework only carries an arm64 simulator slice.
    let mut child = Cmd::new("xcodebuild")
        .arg("-project")
        .arg(repo.path(IOS_APP_DIR).join("Krabink.xcodeproj"))
        .args(["-scheme", "Krabink", "-configuration", &args.configuration])
        .args(["-destination", &destination])
        .arg("-derivedDataPath")
        .arg(repo.path(IOS_APP_DIR).join("build"))
        .args(["CODE_SIGNING_ALLOWED=NO", "ARCHS=arm64", "build"])
        .stdout(Stdio::piped())
        .into_inner()
        .spawn()
        .change_context(Error::Spawn("xcodebuild".into()))?;

    // Only the lines worth reading: errors, warnings in our sources, and
    // the verdict.
    if let Some(stdout) = child.stdout.take() {
        for line in BufReader::new(stdout).lines().map_while(|l| l.ok()) {
            if line.contains("error:")
                || line.contains("** BUILD")
                || (line.contains("warning: ") && line.contains("Sources/"))
            {
                tracing::info!("{line}");
            }
        }
    }
    let status = child
        .wait()
        .change_context(Error::Spawn("xcodebuild".into()))?;
    if status.success() {
        Ok(())
    } else {
        Err(Error::Exit {
            program: "xcodebuild".into(),
            status,
        }
        .into())
    }
}

#[derive(Debug, Clone, clap::Args)]
pub struct DeployArgs {
    /// devicectl device UDID (default: first connected iPad, else the known
    /// iPad Pro 11 M4).
    #[arg(long, env = "KRABINK_IPAD_ID")]
    pub device: Option<String>,
    /// DEVELOPMENT_TEAM for automatic signing.
    #[arg(long, env = "KRABINK_TEAM", default_value = TEAM)]
    pub team: String,
}

pub fn deploy(repo: &Repo, args: &DeployArgs) -> Result<()> {
    if mac::handoff(repo, &[], &[])? {
        return Ok(());
    }
    let app_dir = repo.path(IOS_APP_DIR);
    let derived = app_dir.join("build-device");

    let device = match &args.device {
        Some(device) => device.clone(),
        None => connected_ipad()?.unwrap_or_else(|| DEFAULT_IPAD.to_owned()),
    };
    tracing::info!("==> target device {device}");

    ios_core::ensure(repo)?;
    xcodeproj::generate(repo)?;
    unlock_keychains()?;

    tracing::info!("==> building for device");
    Cmd::new("xcodebuild")
        .arg("-project")
        .arg(app_dir.join("Krabink.xcodeproj"))
        .args(["-scheme", "Krabink", "-configuration", "Debug"])
        .args(["-destination", &format!("platform=iOS,id={device}")])
        .args(["-destination-timeout", "180"])
        .arg("-derivedDataPath")
        .arg(&derived)
        .arg("-allowProvisioningUpdates")
        .arg(format!("DEVELOPMENT_TEAM={}", args.team))
        .args(["CODE_SIGN_STYLE=Automatic", "build"])
        .run()?;

    let app = derived.join("Build/Products/Debug-iphoneos/Krabink.app");
    if !app.is_dir() {
        return Err(Report::new(Error::Missing(app)).attach("not found after build"));
    }

    tracing::info!("==> installing");
    retry(|| {
        Cmd::xcrun()
            .args(["devicectl", "device", "install", "app", "--device", &device])
            .arg(&app)
            .succeeds()
    })?;

    tracing::info!("==> launching {BUNDLE_ID}");
    retry(|| {
        Cmd::xcrun()
            .args([
                "devicectl",
                "device",
                "process",
                "launch",
                "--device",
                &device,
            ])
            .args(["--terminate-existing", BUNDLE_ID])
            .succeeds()
    })?;
    tracing::info!("==> done");
    Ok(())
}

/// devicectl is flaky right after a reconnect (NWError 60, launch
/// 10002/4000); a short wait and retry succeeds.
fn retry(mut attempt: impl FnMut() -> Result<bool>) -> Result<()> {
    for n in 1..=3 {
        if attempt()? {
            return Ok(());
        }
        tracing::warn!("   attempt {n} failed, retrying in 10s");
        std::thread::sleep(Duration::from_secs(10));
    }
    if attempt()? {
        Ok(())
    } else {
        Err(Error::Device.into())
    }
}

/// The first connected iPad in `xcrun devicectl list devices`, if any.
fn connected_ipad() -> Result<Option<String>> {
    let listing = Cmd::xcrun()
        .args(["devicectl", "list", "devices"])
        .output_opt()?
        .unwrap_or_default();
    Ok(first_connected_ipad(&listing))
}

fn first_connected_ipad(listing: &str) -> Option<String> {
    listing
        .lines()
        .filter(|line| line.contains("iPad") && line.contains("connected"))
        .find_map(|line| {
            line.split_whitespace()
                .find(|field| {
                    field.len() == 36
                        && field
                            .bytes()
                            .all(|b| b.is_ascii_digit() || (b'A'..=b'F').contains(&b) || b == b'-')
                })
                .map(str::to_owned)
        })
}

#[cfg(test)]
mod tests {
    use super::first_connected_ipad;

    #[test]
    fn picks_the_connected_ipad_udid() {
        let listing = "\
Name     Hostname   Identifier                             State                Model
iPad     ipad.local 11111111-2222-3333-4444-555555555555   unavailable          iPad Pro
iPad Pro ipad.local E894963B-F801-5AFA-B709-3F61603301BA   connected            iPad Pro 11 (M4)
iPhone   ip.local   AAAAAAAA-0000-0000-0000-000000000000   connected            iPhone
";
        assert_eq!(
            first_connected_ipad(listing).as_deref(),
            Some("E894963B-F801-5AFA-B709-3F61603301BA")
        );
        assert_eq!(first_connected_ipad(""), None);
    }
}
