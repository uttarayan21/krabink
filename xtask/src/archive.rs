//! `archive ios` and `archive mac`: Release archives for App Store
//! Connect / TestFlight, exported to disk or uploaded straight away. On
//! Linux the builds run on the Mac; the build number is taken from git here
//! first, because the Mac mirror only has a throwaway repo.

use std::path::Path;

use crate::apple::{
    ArchiveArgs, Destination, ExportOptions, create_dir, export, find_with_extension, remove_dir,
    unlock_keychains, xcodegen,
};
use crate::cmd::Cmd;
use crate::error::Result;
use crate::repo::{IOS_APP_DIR, MACOS_APP_DIR, Repo};
use crate::{ios_core, mac, xcodeproj};

/// Must match LSMinimumSystemVersion (macos/project.yml deploymentTarget).
const MACOS_DEPLOYMENT_TARGET: &str = "12.0";
const MAC_TARGETS: [&str; 2] = ["aarch64-apple-darwin", "x86_64-apple-darwin"];

/// iPad: rebuild the core, archive Release, export an .ipa or upload.
pub fn ios(repo: &Repo, args: &ArchiveArgs) -> Result<()> {
    let build = args.build_number(repo)?;
    if mac::handoff(repo, &[("KRABINK_BUILD_NUMBER", build.to_string())], &[])? {
        return Ok(());
    }
    ios_local(repo, args, build)
}

pub fn ios_local(repo: &Repo, args: &ArchiveArgs, build: u64) -> Result<()> {
    let app_dir = repo.path(IOS_APP_DIR);
    let derived = app_dir.join("build-archive");
    let archive = derived.join("Krabink.xcarchive");
    let export_dir = derived.join("export");
    let options = derived.join("ExportOptions.plist");

    // The store build must carry the current core: always rebuild the
    // XCFramework (cargo caches, so an unchanged core is quick).
    tracing::info!("==> building KrabinkCore xcframework");
    ios_core::build_local(repo)?;
    xcodeproj::generate(repo)?;
    unlock_keychains()?;

    tracing::info!("==> archiving Krabink ({})", args.describe_build(build));
    remove_dir(&archive)?;
    xcodebuild_archive(
        &app_dir.join("Krabink.xcodeproj"),
        "generic/platform=iOS",
        &archive,
        &derived,
        args,
        build,
    )?;

    create_dir(&derived)?;
    ExportOptions {
        method: &args.method,
        destination: args.destination,
        team: &args.team,
        upload_symbols: true,
    }
    .write(&options)?;

    tracing::info!("==> exporting ({}, {})", args.method, args.destination);
    export(&archive, &options, &export_dir, &args.auth)?;

    match args.destination {
        Destination::Upload => tracing::info!("==> uploaded build {build} to App Store Connect"),
        Destination::Export => {
            let ipa = find_with_extension(&export_dir, "ipa").unwrap_or(export_dir);
            tracing::info!("==> exported {}", ipa.display());
            tracing::info!(
                "    upload: xcrun altool --upload-app -f <ipa> -t ios --apiKey <id> --apiIssuer <issuer>"
            );
            tracing::info!("    or rerun with --destination upload");
        }
    }
    Ok(())
}

/// Mac: universal Rust binary, Xcode wraps it, export a signed .pkg or
/// upload.
pub fn macos(repo: &Repo, args: &ArchiveArgs) -> Result<()> {
    let build = args.build_number(repo)?;
    if mac::handoff(repo, &[("KRABINK_BUILD_NUMBER", build.to_string())], &[])? {
        return Ok(());
    }
    macos_local(repo, args, build)
}

pub fn macos_local(repo: &Repo, args: &ArchiveArgs, build: u64) -> Result<()> {
    let app_dir = repo.path(MACOS_APP_DIR);
    let derived = app_dir.join("build-archive");
    let archive = derived.join("Krabink.xcarchive");
    let export_dir = derived.join("export");
    let options = derived.join("ExportOptions.plist");
    let universal = repo.path("target/universal-apple-darwin/release");

    // Apple's clang for both mac targets, like `build core` does for iOS:
    // the nix devshell's cc-wrapper only links for the host arch.
    let sdk = crate::apple::sdk_path("macosx")?;
    let clang = crate::apple::find(Some("macosx"), "clang")?;
    let ar = crate::apple::find(None, "ar")?;
    let mut env = vec![
        (
            "MACOSX_DEPLOYMENT_TARGET".to_owned(),
            MACOS_DEPLOYMENT_TARGET.to_owned(),
        ),
        ("SDKROOT".to_owned(), sdk),
    ];
    for target in MAC_TARGETS {
        let triple = target.replace('-', "_");
        env.push((format!("CC_{triple}"), clang.clone()));
        env.push((format!("AR_{triple}"), ar.clone()));
        env.push((
            format!("CARGO_TARGET_{}_LINKER", triple.to_uppercase()),
            clang.clone(),
        ));
    }

    tracing::info!("==> building krabink (aarch64 + x86_64)");
    for target in MAC_TARGETS {
        repo.cargo()
            .args(["build", "-p", "krabink", "--release", "--target", target])
            .envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
            .run()?;
    }
    create_dir(&universal)?;
    let binary = universal.join("krabink");
    Cmd::new("lipo")
        .arg("-create")
        .args(MAC_TARGETS.map(|t| repo.path(format!("target/{t}/release/krabink"))))
        .arg("-output")
        .arg(&binary)
        .run()?;
    Cmd::new("lipo").arg("-info").arg(&binary).run()?;

    tracing::info!("==> generating macos/Krabink.xcodeproj");
    xcodegen(&app_dir, true)?;
    unlock_keychains()?;

    tracing::info!(
        "==> archiving Krabink for macOS ({})",
        args.describe_build(build)
    );
    remove_dir(&archive)?;
    xcodebuild_archive(
        &app_dir.join("Krabink.xcodeproj"),
        "generic/platform=macOS",
        &archive,
        &derived,
        args,
        build,
    )?;

    // No dSYM (the binary is not linked by Xcode), so no symbol upload.
    create_dir(&derived)?;
    ExportOptions {
        method: &args.method,
        destination: args.destination,
        team: &args.team,
        upload_symbols: false,
    }
    .write(&options)?;

    tracing::info!("==> exporting ({}, {})", args.method, args.destination);
    export(&archive, &options, &export_dir, &args.auth)?;

    match args.destination {
        Destination::Upload => {
            tracing::info!("==> uploaded macOS build {build} to App Store Connect")
        }
        Destination::Export => {
            let pkg = find_with_extension(&export_dir, "pkg").unwrap_or(export_dir);
            tracing::info!("==> exported {}", pkg.display());
            tracing::info!("    rerun with --destination upload to send it to App Store Connect");
        }
    }
    Ok(())
}

fn xcodebuild_archive(
    project: &Path,
    destination: &str,
    archive: &Path,
    derived: &Path,
    args: &ArchiveArgs,
    build: u64,
) -> Result<()> {
    Cmd::xcodebuild()
        .arg("-project")
        .arg(project)
        .args(["-scheme", "Krabink", "-configuration", "Release"])
        .args(["-destination", destination])
        .arg("-archivePath")
        .arg(archive)
        .arg("-derivedDataPath")
        .arg(derived)
        .arg("-allowProvisioningUpdates")
        .arg(format!("DEVELOPMENT_TEAM={}", args.team))
        .arg("CODE_SIGN_STYLE=Automatic")
        .arg(format!("CURRENT_PROJECT_VERSION={build}"))
        .opt_arg(args.marketing_setting())
        .arg("archive")
        .run()
}
