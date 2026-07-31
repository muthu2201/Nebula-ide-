//! Debounced filesystem watching.
//!
//! Raw inotify/FSEvents/ReadDirectoryChangesW output is unusable directly: a
//! single editor save produces a create, several writes, a rename and a chmod,
//! and a `cargo build` produces tens of thousands of events in a burst. This
//! module coalesces them into one [`FileChange`] per path per debounce window
//! and filters out the paths the IDE does not care about.

use std::path::{Path, PathBuf};
use std::time::Duration;

use crossbeam_channel::{Receiver, Sender, unbounded};
use notify::RecursiveMode;
use notify_debouncer_full::{DebounceEventResult, new_debouncer};

use crate::project::ALWAYS_IGNORED_DIRS;
use crate::{Result, VfsError};

/// Default debounce window.
///
/// Long enough to coalesce an editor's multi-syscall save, short enough that a
/// change made in another window feels immediate.
pub const DEFAULT_DEBOUNCE: Duration = Duration::from_millis(150);

/// What happened to a path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ChangeKind {
    /// The path came into existence.
    Created,
    /// The path's contents changed.
    Modified,
    /// The path went away.
    Removed,
    /// The path was renamed; both endpoints are reported as separate changes.
    Renamed,
}

/// A coalesced change to one path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileChange {
    /// The affected path.
    pub path: PathBuf,
    /// What happened.
    pub kind: ChangeKind,
}

/// A recursive, debounced watcher over one or more roots.
///
/// The watcher owns a background thread; dropping it stops the thread and
/// releases the OS handles.
pub struct FileWatcher {
    // The debouncer must stay alive for events to flow; it is held purely for
    // its `Drop`.
    _debouncer: notify_debouncer_full::Debouncer<
        notify::RecommendedWatcher,
        notify_debouncer_full::RecommendedCache,
    >,
    receiver: Receiver<Vec<FileChange>>,
}

impl FileWatcher {
    /// Watch `root` recursively with the default debounce window.
    pub fn new(root: impl AsRef<Path>) -> Result<Self> {
        Self::with_debounce(root, DEFAULT_DEBOUNCE)
    }

    /// Watch `root` recursively with a custom debounce window.
    pub fn with_debounce(root: impl AsRef<Path>, debounce: Duration) -> Result<Self> {
        let root = root.as_ref().to_path_buf();
        let (tx, rx): (Sender<Vec<FileChange>>, Receiver<Vec<FileChange>>) = unbounded();

        let mut debouncer = new_debouncer(debounce, None, move |result: DebounceEventResult| {
            match result {
                Ok(events) => {
                    let mut changes: Vec<FileChange> = Vec::new();
                    for event in events {
                        for path in &event.paths {
                            if is_uninteresting(path) {
                                continue;
                            }
                            let kind = classify(&event.kind);
                            let change = FileChange { path: path.clone(), kind };
                            // Coalesce: one change per path per window, with the
                            // most consequential kind winning.
                            if let Some(existing) =
                                changes.iter_mut().find(|c| c.path == change.path)
                            {
                                existing.kind = merge_kind(existing.kind, change.kind);
                            } else {
                                changes.push(change);
                            }
                        }
                    }
                    if !changes.is_empty() {
                        // A disconnected receiver means the consumer is gone;
                        // the watcher is about to be dropped, so drop silently.
                        let _ = tx.send(changes);
                    }
                }
                Err(errors) => {
                    for error in errors {
                        tracing::warn!(%error, "filesystem watch error");
                    }
                }
            }
        })
        .map_err(|e| VfsError::Watch(e.to_string()))?;

        debouncer
            .watch(&root, RecursiveMode::Recursive)
            .map_err(|e| VfsError::Watch(e.to_string()))?;

        Ok(Self { _debouncer: debouncer, receiver: rx })
    }

    /// The channel changes are delivered on.
    ///
    /// Exposed so callers can `select!` over it alongside their own channels.
    pub fn receiver(&self) -> &Receiver<Vec<FileChange>> {
        &self.receiver
    }

    /// Take any changes that are ready, without blocking.
    pub fn try_recv(&self) -> Option<Vec<FileChange>> {
        self.receiver.try_recv().ok()
    }

    /// Block for the next batch of changes, up to `timeout`.
    pub fn recv_timeout(&self, timeout: Duration) -> Option<Vec<FileChange>> {
        self.receiver.recv_timeout(timeout).ok()
    }
}

fn classify(kind: &notify::EventKind) -> ChangeKind {
    use notify::EventKind;
    use notify::event::{CreateKind, ModifyKind, RenameMode};
    match kind {
        EventKind::Create(CreateKind::Any | CreateKind::File | CreateKind::Folder | CreateKind::Other) => {
            ChangeKind::Created
        }
        EventKind::Remove(_) => ChangeKind::Removed,
        EventKind::Modify(ModifyKind::Name(RenameMode::Any | RenameMode::To | RenameMode::From | RenameMode::Both | RenameMode::Other)) => {
            ChangeKind::Renamed
        }
        _ => ChangeKind::Modified,
    }
}

/// When several raw events collapse onto one path, keep the one that tells the
/// consumer the most.
///
/// Removal dominates: if a path was created and then removed inside one window,
/// the net effect the IDE must react to is that it is gone.
fn merge_kind(existing: ChangeKind, incoming: ChangeKind) -> ChangeKind {
    match (existing, incoming) {
        (_, ChangeKind::Removed) | (ChangeKind::Removed, _) => ChangeKind::Removed,
        (ChangeKind::Created, _) | (_, ChangeKind::Created) => ChangeKind::Created,
        (ChangeKind::Renamed, _) | (_, ChangeKind::Renamed) => ChangeKind::Renamed,
        _ => ChangeKind::Modified,
    }
}

/// Whether a path is one the IDE never wants to hear about.
fn is_uninteresting(path: &Path) -> bool {
    for component in path.components() {
        let Some(name) = component.as_os_str().to_str() else {
            continue;
        };
        if ALWAYS_IGNORED_DIRS.contains(&name) {
            return true;
        }
    }
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    // Editor and tooling scratch files, including our own atomic-write temps.
    name.contains(".nebula-tmp-")
        || name.ends_with('~')
        || name.ends_with(".swp")
        || name.ends_with(".swx")
        || name.starts_with(".#")
        || name == "4913" // vim's write-probe file
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    /// Drain events for up to `budget`, returning everything seen.
    ///
    /// Filesystem notification latency varies by platform and by CI load, so
    /// tests wait for a condition rather than sleeping a fixed amount.
    fn drain_until(
        watcher: &FileWatcher,
        budget: Duration,
        mut done: impl FnMut(&[FileChange]) -> bool,
    ) -> Vec<FileChange> {
        let deadline = std::time::Instant::now() + budget;
        let mut all = Vec::new();
        while std::time::Instant::now() < deadline {
            if let Some(batch) = watcher.recv_timeout(Duration::from_millis(100)) {
                all.extend(batch);
                if done(&all) {
                    break;
                }
            }
        }
        all
    }

    #[test]
    fn creating_a_file_produces_a_change() {
        let dir = TempDir::new().unwrap();
        let watcher = FileWatcher::with_debounce(dir.path(), Duration::from_millis(50)).unwrap();

        let target = dir.path().join("created.txt");
        fs::write(&target, b"content").unwrap();

        let changes = drain_until(&watcher, Duration::from_secs(10), |all| {
            all.iter().any(|c| c.path.ends_with("created.txt"))
        });
        assert!(
            changes.iter().any(|c| c.path.ends_with("created.txt")),
            "expected a change for created.txt, saw {changes:?}"
        );
    }

    #[test]
    fn ignored_directories_produce_no_events() {
        let dir = TempDir::new().unwrap();
        // Both directories must exist before the watcher starts: a recursive
        // watch registers per-directory, so a file written into a directory
        // created moments earlier can legitimately be missed.
        fs::create_dir_all(dir.path().join("target/debug")).unwrap();
        fs::create_dir_all(dir.path().join("src")).unwrap();
        let watcher = FileWatcher::with_debounce(dir.path(), Duration::from_millis(50)).unwrap();

        // Write into target/ (ignored) and then into src/ (watched). Seeing the
        // second without the first proves the filter works, without depending on
        // a fixed sleep.
        fs::write(dir.path().join("target/debug/out.bin"), b"build output").unwrap();
        fs::write(dir.path().join("src/real.rs"), b"fn main() {}").unwrap();

        let changes = drain_until(&watcher, Duration::from_secs(10), |all| {
            all.iter().any(|c| c.path.ends_with("real.rs"))
        });
        assert!(changes.iter().any(|c| c.path.ends_with("real.rs")));
        assert!(
            !changes.iter().any(|c| c.path.to_string_lossy().contains("target")),
            "build output must be filtered out, saw {changes:?}"
        );
    }

    #[test]
    fn atomic_write_temp_files_are_filtered() {
        assert!(is_uninteresting(Path::new("/p/.main.rs.nebula-tmp-1234")));
        assert!(is_uninteresting(Path::new("/p/main.rs~")));
        assert!(is_uninteresting(Path::new("/p/.main.rs.swp")));
        assert!(is_uninteresting(Path::new("/p/target/debug/x")));
        assert!(is_uninteresting(Path::new("/p/node_modules/x/index.js")));
        assert!(!is_uninteresting(Path::new("/p/src/main.rs")));
    }

    #[test]
    fn removal_dominates_when_kinds_collapse() {
        assert_eq!(merge_kind(ChangeKind::Created, ChangeKind::Removed), ChangeKind::Removed);
        assert_eq!(merge_kind(ChangeKind::Removed, ChangeKind::Modified), ChangeKind::Removed);
        assert_eq!(merge_kind(ChangeKind::Created, ChangeKind::Modified), ChangeKind::Created);
        assert_eq!(merge_kind(ChangeKind::Modified, ChangeKind::Modified), ChangeKind::Modified);
    }

    #[test]
    fn a_burst_of_writes_coalesces_per_path() {
        let dir = TempDir::new().unwrap();
        let watcher = FileWatcher::with_debounce(dir.path(), Duration::from_millis(200)).unwrap();

        let target = dir.path().join("hot.txt");
        for i in 0..50 {
            fs::write(&target, format!("write {i}")).unwrap();
        }

        let changes = drain_until(&watcher, Duration::from_secs(10), |all| {
            all.iter().any(|c| c.path.ends_with("hot.txt"))
        });
        let for_target = changes.iter().filter(|c| c.path.ends_with("hot.txt")).count();
        assert!(for_target >= 1, "expected at least one change, saw none");
        assert!(
            for_target < 50,
            "50 writes in one debounce window must not yield 50 changes, got {for_target}"
        );
    }
}
