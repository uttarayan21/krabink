//! `gen-xcodeproj`: (re)generate ios/Krabink/Krabink.xcodeproj from
//! project.yml when it is missing, older than the spec, or the set of source
//! files changed (xcodegen lists every file explicitly, so a new .swift file
//! needs a regenerate). macOS only; the iPad tasks call this.

use std::path::Path;

use crate::apple::xcodegen;
use crate::error::{Error, Result, ResultExt};
use crate::repo::{IOS_APP_DIR, Repo};

pub fn generate(repo: &Repo) -> Result<()> {
    if !cfg!(target_os = "macos") {
        return Err(Error::MacOnly("gen-xcodeproj").into());
    }
    let app_dir = repo.path(IOS_APP_DIR);
    let project = app_dir.join("Krabink.xcodeproj/project.pbxproj");
    let stamp = app_dir.join("Krabink.xcodeproj/.sources-stamp");

    let sources = source_list(&app_dir)?;
    if project.is_file()
        && !newer_than(&app_dir.join("project.yml"), &project)
        && std::fs::read_to_string(&stamp).is_ok_and(|s| s == sources)
    {
        return Ok(());
    }
    tracing::info!("==> generating Krabink.xcodeproj");
    xcodegen(&app_dir, false)?;
    std::fs::write(&stamp, sources).change_context_lazy(|| Error::Io(stamp.clone()))
}

/// Every file under Sources and UITests, one relative path per line, in
/// byte order.
fn source_list(app_dir: &Path) -> Result<String> {
    let mut files = Vec::new();
    for top in ["Sources", "UITests"] {
        walk(&app_dir.join(top), app_dir, &mut files)?;
    }
    files.sort();
    Ok(files.join("\n"))
}

fn walk(dir: &Path, base: &Path, out: &mut Vec<String>) -> Result<()> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e).change_context_lazy(|| Error::Io(dir.to_owned())),
    };
    for entry in entries {
        let entry = entry.change_context_lazy(|| Error::Io(dir.to_owned()))?;
        let path = entry.path();
        if path.is_dir() {
            walk(&path, base, out)?;
        } else if let Ok(rel) = path.strip_prefix(base) {
            out.push(rel.to_string_lossy().into_owned());
        }
    }
    Ok(())
}

/// `a -nt b`: `a` is newer than `b`, or `b` is unreadable.
fn newer_than(a: &Path, b: &Path) -> bool {
    let mtime = |p: &Path| std::fs::metadata(p).and_then(|m| m.modified()).ok();
    match (mtime(a), mtime(b)) {
        (Some(a), Some(b)) => a > b,
        (Some(_), None) => true,
        _ => false,
    }
}
