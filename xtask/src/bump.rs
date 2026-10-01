//! `bump major|minor|patch`: the app version in Cargo.toml, Cargo.lock and
//! `MARKETING_VERSION` in both XcodeGen specs.

use crate::error::{Error, Report, Result};
use crate::repo::{PROJECT_SPECS, Part, Repo, Version, parse_workspace_version};

pub fn run(repo: &Repo, part: Part) -> Result<()> {
    let cargo_toml = repo.read("Cargo.toml")?;
    let current = parse_workspace_version(&cargo_toml)?;
    let specs = PROJECT_SPECS
        .iter()
        .map(|spec| repo.read(spec).map(|text| (*spec, text)))
        .collect::<Result<Vec<_>>>()?;
    for (spec, text) in &specs {
        if !text.contains(&marketing_line(current)) {
            return Err(Report::new(Error::Version(current.to_string()))
                .attach(format!("{spec} MARKETING_VERSION is not {current}")));
        }
    }

    let next = current.bump(part);
    repo.write(
        "Cargo.toml",
        cargo_toml.replacen(&version_line(current), &version_line(next), 1),
    )?;
    for (spec, text) in &specs {
        repo.write(
            spec,
            text.replace(&marketing_line(current), &marketing_line(next)),
        )?;
    }
    repo.cargo()
        .args(["update", "--workspace", "--offline", "--quiet"])
        .run()?;

    tracing::info!("{current} -> {next}");
    Ok(())
}

fn version_line(v: Version) -> String {
    format!("version = \"{v}\"")
}

fn marketing_line(v: Version) -> String {
    format!("MARKETING_VERSION: \"{v}\"")
}
