//! # nebula-vfs
//!
//! Filesystem access for the IDE. Three responsibilities:
//!
//! * **Reading and writing files** safely — atomic writes so a crash mid-save
//!   cannot truncate a user's source file, and a size ceiling so opening a
//!   500 MB log does not exhaust memory.
//! * **Walking a project** the way developers expect — honouring `.gitignore`,
//!   skipping `target/` and `node_modules/`, never following symlinks out of
//!   the project root.
//! * **Watching for changes** with debouncing, so a `cargo build` touching ten
//!   thousand files produces a handful of events rather than a flood.
//!
//! Every path that crosses this boundary is canonicalised and checked against
//! the project root. That check is not decorative: it is the same containment
//! guarantee the agent tool layer depends on, so a traversal bug here becomes a
//! sandbox escape there.

// `deny` rather than `forbid`: this crate contains exactly one `unsafe` call,
// the memory map in `unsafe_mmap`, which opts in explicitly and documents why.
// Every other module in the crate is still covered.
#![deny(unsafe_code)]
#![warn(missing_docs)]

pub mod project;
pub mod watcher;

use std::fs;
use std::io::Write;
use std::path::{Component, Path, PathBuf};

pub use project::{Project, ProjectEntry, WalkOptions};
pub use watcher::{ChangeKind, FileChange, FileWatcher};

/// Largest file the editor will open, in bytes.
///
/// Files above this are refused rather than loaded: the rope handles large
/// documents well, but a multi-gigabyte file is virtually never something a
/// user meant to open in an editor, and the memory cost is unrecoverable.
pub const MAX_FILE_SIZE: u64 = 256 * 1024 * 1024;

/// Files at or above this size are read through a memory map rather than into
/// a heap buffer.
pub const MMAP_THRESHOLD: u64 = 1024 * 1024;

/// Errors from the filesystem layer.
#[derive(Debug, thiserror::Error)]
pub enum VfsError {
    /// Underlying I/O failure.
    #[error("io error at {path}: {source}")]
    Io {
        /// The path being operated on.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: std::io::Error,
    },

    /// The file exceeds [`MAX_FILE_SIZE`].
    #[error("file {path} is {size} bytes, above the {max} byte limit")]
    TooLarge {
        /// The offending path.
        path: PathBuf,
        /// Its size.
        size: u64,
        /// The configured ceiling.
        max: u64,
    },

    /// A path resolved outside the project root.
    #[error("path {path} escapes the project root {root}")]
    OutsideRoot {
        /// The path that escaped.
        path: PathBuf,
        /// The root it escaped from.
        root: PathBuf,
    },

    /// The path is not a regular file.
    #[error("{path} is not a regular file")]
    NotAFile {
        /// The offending path.
        path: PathBuf,
    },

    /// Decoding the file's bytes as text failed.
    #[error(transparent)]
    Core(#[from] nebula_core::CoreError),

    /// The watcher backend failed.
    #[error("watch error: {0}")]
    Watch(String),
}

/// Convenience result alias.
pub type Result<T, E = VfsError> = std::result::Result<T, E>;

fn io_err(path: impl Into<PathBuf>, source: std::io::Error) -> VfsError {
    VfsError::Io { path: path.into(), source }
}

/// Read a file's bytes, refusing anything over [`MAX_FILE_SIZE`].
///
/// Files over [`MMAP_THRESHOLD`] are memory-mapped and copied out, which avoids
/// the read-syscall loop for large files. The copy is deliberate: holding the
/// map alive would mean the editor observes concurrent external writes as
/// changing bytes under the rope.
pub fn read_bytes(path: impl AsRef<Path>) -> Result<Vec<u8>> {
    let path = path.as_ref();
    let meta = fs::metadata(path).map_err(|e| io_err(path, e))?;
    if !meta.is_file() {
        return Err(VfsError::NotAFile { path: path.to_path_buf() });
    }
    let size = meta.len();
    if size > MAX_FILE_SIZE {
        return Err(VfsError::TooLarge { path: path.to_path_buf(), size, max: MAX_FILE_SIZE });
    }

    if size >= MMAP_THRESHOLD {
        let file = fs::File::open(path).map_err(|e| io_err(path, e))?;
        // SAFETY-adjacent note: `memmap2` is the only unsafe in this dependency
        // graph and it is confined here. The map is read-only, is copied out
        // immediately, and is dropped before this function returns.
        let mmap = unsafe_mmap(&file).map_err(|e| io_err(path, e))?;
        Ok(mmap.to_vec())
    } else {
        fs::read(path).map_err(|e| io_err(path, e))
    }
}

// `#![forbid(unsafe_code)]` applies to this crate's own code; `memmap2::Mmap::map`
// is unsafe because the caller must guarantee the file is not mutated while
// mapped. We copy the contents out immediately and never hand the map to a
// caller, which is the narrowest possible use. This helper is the single place
// the allowance is granted.
#[allow(unsafe_code)]
fn unsafe_mmap(file: &fs::File) -> std::io::Result<memmap2::Mmap> {
    unsafe { memmap2::Mmap::map(file) }
}

/// Read a file into a [`nebula_core::Document`], detecting encoding and language.
pub fn read_document(path: impl AsRef<Path>) -> Result<nebula_core::Document> {
    let path = path.as_ref();
    let bytes = read_bytes(path)?;
    Ok(nebula_core::Document::from_bytes(path, &bytes)?)
}

/// Write `bytes` to `path` atomically.
///
/// The content goes to a temporary file in the same directory, is flushed and
/// synced, and is then renamed over the target. A crash at any point leaves
/// either the old file or the new one — never a truncated hybrid. Same-directory
/// placement matters: `rename` is only atomic within a filesystem.
pub fn write_atomic(path: impl AsRef<Path>, bytes: &[u8]) -> Result<()> {
    let path = path.as_ref();
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(dir).map_err(|e| io_err(dir, e))?;

    let file_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("file");
    let tmp_path = dir.join(format!(".{file_name}.nebula-tmp-{}", std::process::id()));

    // Scope the handle so it is closed before the rename — required on Windows.
    {
        let mut tmp = fs::File::create(&tmp_path).map_err(|e| io_err(&tmp_path, e))?;
        tmp.write_all(bytes).map_err(|e| io_err(&tmp_path, e))?;
        tmp.flush().map_err(|e| io_err(&tmp_path, e))?;
        tmp.sync_all().map_err(|e| io_err(&tmp_path, e))?;
    }

    // Preserve the original file's permissions, which `File::create` would
    // otherwise reset to the process umask default.
    if let Ok(meta) = fs::metadata(path) {
        let _ = fs::set_permissions(&tmp_path, meta.permissions());
    }

    if let Err(e) = fs::rename(&tmp_path, path) {
        let _ = fs::remove_file(&tmp_path);
        return Err(io_err(path, e));
    }
    Ok(())
}

/// Write a document back to its own path, atomically.
pub fn write_document(doc: &nebula_core::Document) -> Result<()> {
    let path = doc
        .path()
        .ok_or_else(|| VfsError::NotAFile { path: PathBuf::from("<unsaved>") })?;
    write_atomic(path, &doc.to_bytes())
}

/// Lexically normalise a path: resolve `.` and `..` without touching the disk.
///
/// Unlike [`std::fs::canonicalize`] this works for paths that do not exist yet,
/// which is what a containment check needs — the whole point is to reject a
/// path *before* creating it.
pub fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                // Popping past the root is a no-op, matching kernel behaviour.
                if !out.as_os_str().is_empty() && out.file_name().is_some() {
                    out.pop();
                }
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Resolve `path` against `root` and verify the result stays inside it.
///
/// Both the lexical form and, when the path exists, the fully canonicalised form
/// are checked. Checking only the lexical form would miss a symlink pointing
/// out of the tree; checking only the canonical form would let a non-existent
/// path through, which is exactly the case a "create file" tool call hits.
pub fn contained_path(root: &Path, path: &Path) -> Result<PathBuf> {
    let root_norm = if root.is_absolute() {
        fs::canonicalize(root).unwrap_or_else(|_| normalize(root))
    } else {
        normalize(root)
    };

    let joined = if path.is_absolute() { path.to_path_buf() } else { root_norm.join(path) };
    let lexical = normalize(&joined);

    if !lexical.starts_with(&root_norm) {
        return Err(VfsError::OutsideRoot { path: path.to_path_buf(), root: root_norm });
    }

    // If it exists, the real resolved path must also be inside — this is what
    // catches a symlink aimed at /etc.
    if lexical.exists() {
        let canonical = fs::canonicalize(&lexical).map_err(|e| io_err(&lexical, e))?;
        if !canonical.starts_with(&root_norm) {
            return Err(VfsError::OutsideRoot { path: path.to_path_buf(), root: root_norm });
        }
        return Ok(canonical);
    }

    // Does not exist yet: verify the nearest existing ancestor is inside, so a
    // symlinked parent directory cannot be used to escape on create.
    let mut ancestor = lexical.parent();
    while let Some(dir) = ancestor {
        if dir.exists() {
            let canonical_dir = fs::canonicalize(dir).map_err(|e| io_err(dir, e))?;
            if !canonical_dir.starts_with(&root_norm) {
                return Err(VfsError::OutsideRoot { path: path.to_path_buf(), root: root_norm });
            }
            break;
        }
        ancestor = dir.parent();
    }

    Ok(lexical)
}

/// Whether a file looks like binary content.
///
/// The heuristic is the one `git` uses: a NUL byte in the first 8 KiB. It is
/// cheap and has almost no false positives on real source files, which matters
/// because the cost of getting it wrong is rendering a binary as mojibake.
pub fn is_binary(bytes: &[u8]) -> bool {
    let window = &bytes[..bytes.len().min(8192)];
    window.contains(&0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn atomic_write_then_read_round_trips() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("sub/dir/file.txt");
        write_atomic(&path, b"hello vfs").unwrap();
        assert_eq!(read_bytes(&path).unwrap(), b"hello vfs");
    }

    #[test]
    fn atomic_write_leaves_no_temp_files_behind() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("f.txt");
        write_atomic(&path, b"one").unwrap();
        write_atomic(&path, b"two").unwrap();
        let entries: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
            .collect();
        assert_eq!(entries, vec!["f.txt"]);
        assert_eq!(read_bytes(&path).unwrap(), b"two");
    }

    #[test]
    fn atomic_write_overwrites_existing_content_entirely() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("f.txt");
        write_atomic(&path, b"a much longer original content").unwrap();
        write_atomic(&path, b"short").unwrap();
        assert_eq!(read_bytes(&path).unwrap(), b"short", "no trailing bytes survive");
    }

    #[test]
    fn large_files_are_read_through_the_mmap_path() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("big.txt");
        let content = vec![b'x'; (MMAP_THRESHOLD as usize) + 4096];
        write_atomic(&path, &content).unwrap();
        assert_eq!(read_bytes(&path).unwrap(), content);
    }

    #[test]
    fn reading_a_directory_is_an_error_not_a_panic() {
        let dir = TempDir::new().unwrap();
        assert!(matches!(read_bytes(dir.path()), Err(VfsError::NotAFile { .. })));
    }

    #[test]
    fn documents_round_trip_through_disk() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("main.rs");
        write_atomic(&path, b"fn main() {}\n").unwrap();

        let mut doc = read_document(&path).unwrap();
        assert_eq!(doc.language(), Some("rust"));
        doc.set_caret(doc.buffer().len_chars());
        doc.insert_at_cursors("// edited\n", false).unwrap();
        write_document(&doc).unwrap();

        assert_eq!(read_bytes(&path).unwrap(), b"fn main() {}\n// edited\n");
    }

    #[test]
    fn normalize_resolves_dot_segments_without_disk_access() {
        assert_eq!(normalize(Path::new("/a/b/../c/./d")), PathBuf::from("/a/c/d"));
        assert_eq!(normalize(Path::new("a/../../b")), PathBuf::from("b"));
        assert_eq!(normalize(Path::new("/../..")), PathBuf::from("/"));
    }

    #[test]
    fn containment_accepts_paths_inside_the_root() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/lib.rs"), b"").unwrap();

        let resolved = contained_path(root, Path::new("src/lib.rs")).unwrap();
        assert!(resolved.ends_with("src/lib.rs"));
    }

    #[test]
    fn containment_rejects_dot_dot_traversal() {
        let dir = TempDir::new().unwrap();
        let err = contained_path(dir.path(), Path::new("../../../etc/passwd"));
        assert!(matches!(err, Err(VfsError::OutsideRoot { .. })));
    }

    #[test]
    fn containment_rejects_absolute_paths_outside_the_root() {
        let dir = TempDir::new().unwrap();
        let err = contained_path(dir.path(), Path::new("/etc/passwd"));
        assert!(matches!(err, Err(VfsError::OutsideRoot { .. })));
    }

    #[test]
    fn containment_allows_creating_a_file_that_does_not_exist_yet() {
        let dir = TempDir::new().unwrap();
        let resolved = contained_path(dir.path(), Path::new("new/nested/file.txt")).unwrap();
        assert!(resolved.starts_with(fs::canonicalize(dir.path()).unwrap()));
    }

    #[cfg(unix)]
    #[test]
    fn containment_rejects_a_symlink_pointing_out_of_the_root() {
        let dir = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        fs::write(outside.path().join("secret.txt"), b"secret").unwrap();
        std::os::unix::fs::symlink(outside.path().join("secret.txt"), dir.path().join("link.txt"))
            .unwrap();

        let err = contained_path(dir.path(), Path::new("link.txt"));
        assert!(
            matches!(err, Err(VfsError::OutsideRoot { .. })),
            "a symlink out of the tree must not be followed"
        );
    }

    #[cfg(unix)]
    #[test]
    fn containment_rejects_creation_under_a_symlinked_directory() {
        let dir = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        std::os::unix::fs::symlink(outside.path(), dir.path().join("escape")).unwrap();

        let err = contained_path(dir.path(), Path::new("escape/new-file.txt"));
        assert!(matches!(err, Err(VfsError::OutsideRoot { .. })));
    }

    #[test]
    fn binary_detection_keys_on_nul_bytes() {
        assert!(!is_binary(b"plain source text\n"));
        assert!(is_binary(b"ELF\0\0\0"));
        // A NUL past the sniff window is not looked at, matching git.
        let mut late = vec![b'a'; 9000];
        late.push(0);
        assert!(!is_binary(&late));
    }
}
