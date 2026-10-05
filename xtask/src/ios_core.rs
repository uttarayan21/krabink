//! `build core`: the KrabinkCore XCFramework + generated Swift bindings
//! for the iOS app. Needs xcodebuild, so on Linux it runs on the Mac. The
//! rust toolchain there needs the aarch64-apple-ios{,-sim} targets (rustup
//! or `nix develop`).

use crate::apple::{copy, create_dir, remove_dir};
use crate::cmd::Cmd;
use crate::error::Result;
use crate::mac;
use crate::repo::{IOS_CORE_DIR, Repo};

const TARGETS: [&str; 2] = ["aarch64-apple-ios", "aarch64-apple-ios-sim"];
const PROFILE: &str = "release";
const DEPLOYMENT_TARGET: &str = "17.0";

pub fn build(repo: &Repo) -> Result<()> {
    if mac::handoff(repo, &[], &[])? {
        return Ok(());
    }
    build_local(repo)
}

/// Build only when the xcframework is missing (gitignored, so fresh
/// worktrees need it).
pub fn ensure(repo: &Repo) -> Result<()> {
    if repo
        .path(IOS_CORE_DIR)
        .join("KrabinkCoreFFI.xcframework")
        .is_dir()
    {
        return Ok(());
    }
    tracing::info!("==> building KrabinkCore xcframework");
    build_local(repo)
}

/// The build itself, for callers that already know they are on the Mac.
pub fn build_local(repo: &Repo) -> Result<()> {
    let out = repo.path(IOS_CORE_DIR);
    let generated = repo.path("target/uniffi-ios");

    // `ring` (iroh's TLS) builds C with `cc`; the nix devshell's CC targets
    // macOS, so point each iOS target at the matching Apple clang + SDK.
    let ios_sdk = crate::apple::sdk_path("iphoneos")?;
    let sim_sdk = crate::apple::sdk_path("iphonesimulator")?;
    let ar = crate::apple::find(None, "ar")?;
    let env = [
        (
            "CC_aarch64_apple_ios",
            crate::apple::find(Some("iphoneos"), "clang")?,
        ),
        ("AR_aarch64_apple_ios", ar.clone()),
        (
            "CFLAGS_aarch64_apple_ios",
            format!("-isysroot {ios_sdk} -target arm64-apple-ios{DEPLOYMENT_TARGET}"),
        ),
        (
            "CC_aarch64_apple_ios_sim",
            crate::apple::find(Some("iphonesimulator"), "clang")?,
        ),
        ("AR_aarch64_apple_ios_sim", ar),
        (
            "CFLAGS_aarch64_apple_ios_sim",
            format!("-isysroot {sim_sdk} -target arm64-apple-ios{DEPLOYMENT_TARGET}-simulator"),
        ),
        ("IPHONEOS_DEPLOYMENT_TARGET", DEPLOYMENT_TARGET.to_owned()),
    ];

    // staticlib only: no link step, so the (macOS-targeting) nix cc-wrapper
    // and its libiconv never get involved. The cdylib crate-type would try
    // to link a per-target dylib nobody needs on iOS.
    for target in TARGETS {
        repo.cargo()
            .args([
                "rustc",
                "-p",
                "krabink-ffi",
                "--release",
                "--features",
                "ism",
            ])
            .args(["--target", target, "--crate-type", "staticlib"])
            .envs(env.iter().map(|(k, v)| (k, v.as_str())))
            .run()?;
    }

    // Library-mode bindgen off one static lib (the metadata is
    // target-independent).
    remove_dir(&generated)?;
    create_dir(&generated)?;
    let staticlib = |target: &str| repo.path(format!("target/{target}/{PROFILE}/libkrabink_ffi.a"));
    repo.cargo()
        .args([
            "run",
            "-q",
            "-p",
            "krabink-ffi",
            "--features",
            "bindgen",
            "--bin",
            "uniffi-bindgen",
            "--",
        ])
        .args(["generate", "--library"])
        .arg(staticlib(TARGETS[0]))
        .args(["--language", "swift", "--out-dir"])
        .arg(&generated)
        .run()?;

    // XCFramework wants a headers dir with a `module.modulemap`.
    let headers = generated.join("include");
    create_dir(&headers)?;
    copy(
        &generated.join("krabinkFFI.h"),
        &headers.join("krabinkFFI.h"),
    )?;
    copy(
        &generated.join("krabinkFFI.modulemap"),
        &headers.join("module.modulemap"),
    )?;

    let xcframework = out.join("KrabinkCoreFFI.xcframework");
    remove_dir(&xcframework)?;
    Cmd::xcodebuild()
        .arg("-create-xcframework")
        .args(TARGETS.iter().flat_map(|target| {
            [
                "-library".into(),
                staticlib(target).display().to_string(),
                "-headers".into(),
                headers.display().to_string(),
            ]
        }))
        .arg("-output")
        .arg(&xcframework)
        .run()?;

    let sources = out.join("Sources/KrabinkCore");
    create_dir(&sources)?;
    copy(
        &generated.join("krabink.swift"),
        &sources.join("Krabink.swift"),
    )?;

    tracing::info!("KrabinkCore ready: {}", out.display());
    Ok(())
}
