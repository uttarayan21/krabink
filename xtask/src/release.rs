//! `release`: push the current version to App Store Connect. Checks the
//! release is tagged and consistent, then archives and uploads the iPad and
//! Mac apps with one shared build number. On Linux the checks run here and
//! the builds on the Mac.

use crate::apple::{ArchiveArgs, AscAuth, Destination};
use crate::error::{Error, Report, Result};
use crate::repo::{PROJECT_SPECS, Repo, TEAM, Version};
use crate::{archive, mac};

#[derive(Debug, Clone, clap::Args)]
pub struct ReleaseArgs {
    /// iPad only.
    #[arg(long, conflicts_with = "macos")]
    pub ios: bool,
    /// Mac only.
    #[arg(long)]
    pub macos: bool,
    /// Archive and export, no upload.
    #[arg(long)]
    pub export: bool,
    /// Create and push v<version> at HEAD first (which also starts the Gitea
    /// Linux package build).
    #[arg(long)]
    pub tag: bool,
    /// `cargo clean` afterwards.
    #[arg(long)]
    pub clean: bool,
    /// Skip the clean-tree, version and tag checks (set automatically on the
    /// Mac mirror, whose git repo is throwaway).
    #[arg(long)]
    pub skip_checks: bool,
    /// DEVELOPMENT_TEAM for automatic signing.
    #[arg(long, env = "KRABINK_TEAM", default_value = TEAM)]
    pub team: String,
    #[command(flatten)]
    pub auth: AscAuth,
}

impl ReleaseArgs {
    fn destination(&self) -> Destination {
        if self.export {
            Destination::Export
        } else {
            Destination::Upload
        }
    }

    fn platforms(&self) -> (bool, bool) {
        match (self.ios, self.macos) {
            (true, false) => (true, false),
            (false, true) => (false, true),
            _ => (true, true),
        }
    }
}

pub fn run(repo: &Repo, args: &ReleaseArgs) -> Result<()> {
    let version = repo.version()?;
    if !args.skip_checks {
        check(repo, version, args.tag)?;
    }

    let build = match std::env::var("KRABINK_BUILD_NUMBER") {
        Ok(n) => n.parse().map_err(|_| {
            Report::new(Error::Check).attach(format!("KRABINK_BUILD_NUMBER=`{n}` is not a number"))
        })?,
        Err(_) => repo.commit_count()?,
    };
    // The checks above already ran; the mirror must not repeat them.
    if mac::handoff(
        repo,
        &[("KRABINK_BUILD_NUMBER", build.to_string())],
        &["--skip-checks"],
    )? {
        return Ok(());
    }

    let destination = args.destination();
    let (ios, macos) = args.platforms();
    tracing::info!("==> releasing {version} (build {build}, {destination})");

    let archive_args = ArchiveArgs {
        team: args.team.clone(),
        method: "app-store-connect".to_owned(),
        destination,
        build_number: Some(build),
        marketing_version: None,
        auth: args.auth.clone(),
    };
    let mut done = Vec::new();
    if ios {
        archive::ios_local(repo, &archive_args, build)?;
        done.push("iPad");
    }
    if macos {
        archive::macos_local(repo, &archive_args, build)?;
        done.push("Mac");
    }

    if args.clean {
        tracing::info!("==> cargo clean");
        repo.cargo().arg("clean").run()?;
    }

    tracing::info!("");
    tracing::info!(
        "==> {version} build {build}: {} {destination}ed",
        done.join(" + ")
    );
    if destination == Destination::Upload {
        tracing::info!("    next, in App Store Connect once processing finishes (5-30 min):");
        tracing::info!("    attach build {build} to version {version}, fill What's New, submit.");
    }
    Ok(())
}

/// Clean tree, specs agree with Cargo.toml, and v<version> is at HEAD
/// (created and pushed when `tag` is set and it does not exist yet).
fn check(repo: &Repo, version: Version, tag: bool) -> Result<()> {
    let fail = |msg: String| Err(Report::new(Error::Check).attach(msg));

    if !repo.is_clean()? {
        return fail("working tree is not clean; commit or stash first".into());
    }
    for spec in PROJECT_SPECS {
        if !repo
            .read(spec)?
            .contains(&format!("MARKETING_VERSION: \"{version}\""))
        {
            return fail(format!(
                "{spec} MARKETING_VERSION is not {version} (run cargo xtask bump)"
            ));
        }
    }

    let tag_name = version.tag();
    let head = repo.head()?;
    match repo.tag_commit(&tag_name)? {
        Some(tagged) if tagged == head => Ok(()),
        Some(tagged) => fail(format!(
            "tag {tag_name} is at {}, HEAD is {}; release from the tag, or bump the version for a new release",
            &tagged[..7.min(tagged.len())],
            &head[..7.min(head.len())],
        )),
        None if tag => {
            tracing::info!("==> tagging {tag_name} at {}", &head[..7.min(head.len())]);
            repo.git()
                .args(["tag", "-a", &tag_name, "-m", &format!("Release {version}")])
                .run()?;
            repo.git().args(["push", "origin", &tag_name]).run()
        }
        None => fail(format!(
            "no tag {tag_name}; rerun with --tag to create and push it"
        )),
    }
}
