//! `swift-smoke`: build the host cdylib, generate Swift bindings, compile
//! xtask/smoke/main.swift against them and run it. Needs swiftc, so on
//! Linux it runs on the Mac.

use crate::apple::{copy, create_dir, remove_dir};
use crate::cmd::Cmd;
use crate::error::Result;
use crate::mac;
use crate::repo::Repo;

/// The smoke test source, relative to the repo root.
pub const SMOKE_SOURCE: &str = "xtask/smoke/main.swift";

pub fn run(repo: &Repo) -> Result<()> {
    if mac::handoff(repo, &[], &[])? {
        return Ok(());
    }
    let generated = repo.path("target/swift-smoke");
    let debug = repo.path("target/debug");

    repo.cargo()
        .args(["build", "-p", "krabink-ffi", "--lib"])
        .run()?;
    remove_dir(&generated)?;
    create_dir(&generated)?;
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
        .arg(debug.join("libkrabink_ffi.dylib"))
        .args(["--language", "swift", "--out-dir"])
        .arg(&generated)
        .run()?;
    copy(
        &generated.join("krabinkFFI.modulemap"),
        &generated.join("module.modulemap"),
    )?;

    // Clean env: a nix devShell's clang setup breaks Xcode's swiftc
    // ("missing required module 'SwiftShims'").
    let home = std::env::var_os("HOME").unwrap_or_default();
    let smoke = generated.join("smoke");
    Cmd::xcrun()
        .env_clear()
        .env("HOME", home)
        .env("PATH", "/usr/bin:/bin")
        .env("TERM", "dumb")
        .args(["swiftc", "-o"])
        .arg(&smoke)
        .arg(repo.path(SMOKE_SOURCE))
        .arg(generated.join("krabink.swift"))
        .arg("-I")
        .arg(&generated)
        .arg("-L")
        .arg(&debug)
        .arg("-lkrabink_ffi")
        .current_dir(repo.root())
        .run()?;

    Cmd::new(&smoke)
        .env("DYLD_LIBRARY_PATH", &debug)
        .current_dir(repo.root())
        .run()
}
