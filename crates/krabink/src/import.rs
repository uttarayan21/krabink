//! Markdown files opened with the desktop app (`krabink FILE.md…`, the
//! Linux file association, a drop on the window) become notes: each file
//! is copied into a new note, which syncs like any other, so the iPad can
//! annotate it with the Pencil while the desktop follows live. The file
//! itself is not linked; edits and ink stay in the note.
//!
//! One app owns a data dir (its redb stores are locked), so a second
//! launch with files hands them to the running one over a unix socket in
//! the data dir and exits.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, mpsc};

use bevy::prelude::*;
use bevy::window::FileDragAndDrop;
use krabink_core::DocKey;

use crate::docs::Docs;
use crate::sync::{LocalCommit, SubscribeNeeded};
use crate::ui::EditorState;

/// What a dropped file must end in to be imported; anything else dropped
/// on the window is ignored. Files named on the command line are taken
/// as they are.
const MARKDOWN_EXTENSIONS: &[&str] = &["md", "markdown", "mdown", "mkd", "txt"];

/// Files waiting to become notes: the command line's, then whatever a
/// later launch forwards.
#[derive(Resource)]
pub struct Inbox {
    pending: Vec<PathBuf>,
    /// Paths a forwarding launch sent; `None` when nothing listens.
    forwarded: Option<Mutex<mpsc::Receiver<PathBuf>>>,
}

/// How a launch with files to open resolved.
pub enum Launch {
    /// Another app owns the data dir and took the files.
    Forwarded,
    /// Nothing else is running: this launch opens them itself.
    Primary(Vec<PathBuf>),
}

impl Launch {
    /// Hand `files` to the app already running on `socket`, if any.
    /// Paths are made absolute first: the running app has its own working
    /// directory.
    pub fn resolve(socket: &Path, files: Vec<PathBuf>) -> Launch {
        let files: Vec<PathBuf> = files
            .into_iter()
            .map(|f| std::path::absolute(&f).unwrap_or(f))
            .collect();
        if files.is_empty() || !platform::forward(socket, &files) {
            Launch::Primary(files)
        } else {
            Launch::Forwarded
        }
    }
}

impl Inbox {
    /// Start listening for forwarded files. Call only once the stores are
    /// open: their lock is what makes this launch the owner, so a socket
    /// left by a crashed run can be replaced safely.
    pub fn listen(socket: &Path, pending: Vec<PathBuf>) -> Self {
        let forwarded = platform::listen(socket)
            .inspect_err(|err| tracing::warn!(%err, "not listening for opened files"))
            .ok()
            .map(Mutex::new);
        Self { pending, forwarded }
    }

    /// Everything queued since the last call.
    fn drain(&mut self) -> Vec<PathBuf> {
        let forwarded: Vec<PathBuf> = self
            .forwarded
            .as_ref()
            .and_then(|rx| rx.lock().ok())
            .map(|rx| rx.try_iter().collect())
            .unwrap_or_default();
        self.pending.extend(forwarded);
        std::mem::take(&mut self.pending)
    }
}

/// Read a markdown file as note text. Undecodable bytes become U+FFFD
/// rather than failing the import; a BOM is dropped and CRLF / CR folded
/// to LF, since the editor and ink anchors split lines on `\n` alone (the
/// iPad importer does the same).
pub fn read_markdown(path: &Path) -> std::io::Result<String> {
    let bytes = std::fs::read(path)?;
    let text = String::from_utf8_lossy(&bytes);
    Ok(text
        .strip_prefix('\u{feff}')
        .unwrap_or(&text)
        .replace("\r\n", "\n")
        .replace('\r', "\n"))
}

fn is_markdown(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| {
            MARKDOWN_EXTENSIONS
                .iter()
                .any(|md| ext.eq_ignore_ascii_case(md))
        })
}

pub struct ImportPlugin;

impl Plugin for ImportPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(Update, import_files);
    }
}

/// Turn queued and dropped files into notes; the last one imported opens.
fn import_files(
    mut inbox: ResMut<Inbox>,
    mut drops: MessageReader<FileDragAndDrop>,
    mut docs: ResMut<Docs>,
    mut editor: ResMut<EditorState>,
    mut commits: MessageWriter<LocalCommit>,
    mut subscribes: MessageWriter<SubscribeNeeded>,
) {
    let dropped = drops.read().filter_map(|drop| match drop {
        FileDragAndDrop::DroppedFile { path_buf, .. } if is_markdown(path_buf) => {
            Some(path_buf.clone())
        }
        _ => None,
    });
    let files: Vec<PathBuf> = inbox.drain().into_iter().chain(dropped).collect();
    let last = files
        .iter()
        .filter_map(|path| {
            let text = read_markdown(path)
                .inspect_err(
                    |err| tracing::error!(%err, path = %path.display(), "reading file failed"),
                )
                .ok()?;
            let (id, payloads) = docs
                .import_note(&text)
                .inspect_err(
                    |err| tracing::error!(%err, path = %path.display(), "importing file failed"),
                )
                .ok()?;
            commits.write_batch(
                payloads
                    .into_iter()
                    .map(|(doc, payload)| LocalCommit { doc, payload }),
            );
            subscribes.write(SubscribeNeeded(DocKey::from(id)));
            tracing::info!(path = %path.display(), note = %id, "imported markdown file");
            Some(id)
        })
        .last();
    if let Some(id) = last {
        editor.preview = false;
        crate::ui::open_note(&mut docs, &mut editor, id);
    }
}

#[cfg(unix)]
mod platform {
    //! Paths travel NUL-separated, one connection per launch.

    use std::io::{Read, Write};
    use std::os::unix::ffi::{OsStrExt, OsStringExt};
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::path::{Path, PathBuf};
    use std::sync::mpsc;

    /// True once a running app took the files.
    pub fn forward(socket: &Path, files: &[PathBuf]) -> bool {
        let Ok(mut stream) = UnixStream::connect(socket) else {
            return false;
        };
        let message: Vec<u8> = files
            .iter()
            .flat_map(|f| f.as_os_str().as_bytes().iter().copied().chain([0]))
            .collect();
        stream.write_all(&message).is_ok()
    }

    pub fn listen(socket: &Path) -> std::io::Result<mpsc::Receiver<PathBuf>> {
        // A socket file left by a crashed run refuses the bind.
        match std::fs::remove_file(socket) {
            Err(err) if err.kind() != std::io::ErrorKind::NotFound => return Err(err),
            _ => {}
        }
        let listener = UnixListener::bind(socket)?;
        let (tx, rx) = mpsc::channel();
        std::thread::Builder::new()
            .name("krabink-open".into())
            .spawn(move || {
                for stream in listener.incoming() {
                    let mut message = Vec::new();
                    if let Err(err) = stream.and_then(|mut s| s.read_to_end(&mut message)) {
                        tracing::warn!(%err, "reading forwarded files failed");
                        continue;
                    }
                    let sent = message
                        .split(|b| *b == 0)
                        .filter(|path| !path.is_empty())
                        .map(|path| PathBuf::from(std::ffi::OsString::from_vec(path.to_vec())))
                        .try_for_each(|path| tx.send(path));
                    if sent.is_err() {
                        break; // the app is gone
                    }
                }
            })?;
        Ok(rx)
    }
}

#[cfg(not(unix))]
mod platform {
    //! No single-instance hand-off: a second launch opens its files itself
    //! (and fails on the locked store if another app owns the data dir).

    use std::path::{Path, PathBuf};
    use std::sync::mpsc;

    pub fn forward(_socket: &Path, _files: &[PathBuf]) -> bool {
        false
    }

    pub fn listen(_socket: &Path) -> std::io::Result<mpsc::Receiver<PathBuf>> {
        Err(std::io::ErrorKind::Unsupported.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_markdown_normalises_line_endings_and_bom() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("plan.md");
        std::fs::write(&path, b"\xef\xbb\xbf# Plan\r\n\r\nold\rmac\n\xffend").unwrap();
        assert_eq!(
            read_markdown(&path).unwrap(),
            "# Plan\n\nold\nmac\n\u{fffd}end"
        );
    }

    #[test]
    fn dropped_files_filter_by_extension() {
        assert!(is_markdown(Path::new("/a/notes.MD")));
        assert!(is_markdown(Path::new("readme.markdown")));
        assert!(!is_markdown(Path::new("photo.png")));
        assert!(!is_markdown(Path::new("Makefile")));
    }

    #[cfg(unix)]
    #[test]
    fn second_launch_forwards_files_to_the_running_app() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("open.sock");
        let files = vec![dir.path().join("a.md"), dir.path().join("b c.md")];

        // Nobody listening: the launch keeps its files.
        assert!(matches!(
            Launch::resolve(&socket, files.clone()),
            Launch::Primary(kept) if kept == files
        ));

        // A stale socket file from a crashed run does not block listening.
        std::fs::write(&socket, b"").unwrap();
        let mut inbox = Inbox::listen(&socket, Vec::new());
        assert!(matches!(
            Launch::resolve(&socket, files.clone()),
            Launch::Forwarded
        ));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut got = Vec::new();
        while got.len() < files.len() && std::time::Instant::now() < deadline {
            got.extend(inbox.drain());
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(got, files);

        // Without files a launch never forwards (it opens the app).
        assert!(matches!(
            Launch::resolve(&socket, Vec::new()),
            Launch::Primary(kept) if kept.is_empty()
        ));
    }
}
