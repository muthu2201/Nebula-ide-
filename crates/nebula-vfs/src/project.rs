//! Project trees: gitignore-aware traversal rooted at a directory.

use std::path::{Path, PathBuf};

use ignore::{DirEntry, WalkBuilder};

use crate::{Result, VfsError};

/// Directories that are skipped even when no ignore file mentions them.
///
/// These are build outputs and dependency caches: including them makes the file
/// picker useless and the repo map meaningless, and every one of them is
/// reproducible from the source that is indexed.
pub const ALWAYS_IGNORED_DIRS: &[&str] = &[
    ".git",
    ".hg",
    ".svn",
    ".jj",
    "target",
    "node_modules",
    ".venv",
    "venv",
    "__pycache__",
    ".mypy_cache",
    ".pytest_cache",
    ".ruff_cache",
    "dist",
    "build",
    ".next",
    ".nuxt",
    ".gradle",
    ".idea",
    ".vscode",
    ".nebula/cache",
];

/// How to walk a project tree.
#[derive(Debug, Clone)]
pub struct WalkOptions {
    /// Honour `.gitignore`, `.ignore` and `.git/info/exclude`.
    pub respect_gitignore: bool,
    /// Include dotfiles.
    pub include_hidden: bool,
    /// Follow symbolic links. Off by default: a link loop is a hang, and a link
    /// out of the tree is an information leak.
    pub follow_links: bool,
    /// Maximum directory depth, or `None` for unlimited.
    pub max_depth: Option<usize>,
    /// Stop after this many entries. Guards against a pathological tree
    /// stalling the UI.
    pub max_entries: usize,
    /// Skip files larger than this.
    pub max_file_size: Option<u64>,
}

impl Default for WalkOptions {
    fn default() -> Self {
        Self {
            respect_gitignore: true,
            include_hidden: false,
            follow_links: false,
            max_depth: None,
            max_entries: 500_000,
            max_file_size: Some(crate::MAX_FILE_SIZE),
        }
    }
}

/// One entry in a project tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectEntry {
    /// Absolute path.
    pub path: PathBuf,
    /// Path relative to the project root — what the UI and the repo map key on.
    pub relative: PathBuf,
    /// Whether this is a directory.
    pub is_dir: bool,
    /// File size in bytes; zero for directories.
    pub size: u64,
    /// Detected language identifier, if any.
    pub language: Option<String>,
}

/// A project rooted at a directory on disk.
#[derive(Debug, Clone)]
pub struct Project {
    root: PathBuf,
    options: WalkOptions,
}

impl Project {
    /// Open a project at `root`.
    ///
    /// The root is canonicalised immediately: every containment check afterwards
    /// compares against the resolved path, so a symlinked project directory
    /// works correctly instead of failing every check.
    pub fn open(root: impl AsRef<Path>) -> Result<Self> {
        let root = root.as_ref();
        let canonical = std::fs::canonicalize(root)
            .map_err(|e| VfsError::Io { path: root.to_path_buf(), source: e })?;
        if !canonical.is_dir() {
            return Err(VfsError::NotAFile { path: canonical });
        }
        Ok(Self { root: canonical, options: WalkOptions::default() })
    }

    /// Open with custom walk options.
    pub fn open_with(root: impl AsRef<Path>, options: WalkOptions) -> Result<Self> {
        Ok(Self { options, ..Self::open(root)? })
    }

    /// The canonical project root.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The walk options in force.
    pub fn options(&self) -> &WalkOptions {
        &self.options
    }

    /// Resolve a path against the root, rejecting anything that escapes it.
    pub fn resolve(&self, path: impl AsRef<Path>) -> Result<PathBuf> {
        crate::contained_path(&self.root, path.as_ref())
    }

    /// Whether `path` lies inside the project.
    pub fn contains(&self, path: impl AsRef<Path>) -> bool {
        self.resolve(path).is_ok()
    }

    /// Walk the project, returning every file (not directory) that survives the
    /// ignore rules.
    pub fn files(&self) -> Result<Vec<ProjectEntry>> {
        Ok(self.walk()?.into_iter().filter(|e| !e.is_dir).collect())
    }

    /// Walk the project, returning files and directories.
    pub fn walk(&self) -> Result<Vec<ProjectEntry>> {
        let mut builder = WalkBuilder::new(&self.root);
        builder
            .hidden(!self.options.include_hidden)
            .git_ignore(self.options.respect_gitignore)
            .git_global(self.options.respect_gitignore)
            .git_exclude(self.options.respect_gitignore)
            .ignore(self.options.respect_gitignore)
            .parents(self.options.respect_gitignore)
            // Honour `.gitignore` even when the directory is not a git
            // repository. The crate default is to require a `.git` dir, which
            // would silently index build output in a worktree, a jj repo, or a
            // plain directory that still ships a meaningful `.gitignore`.
            .require_git(false)
            .follow_links(self.options.follow_links)
            .max_depth(self.options.max_depth)
            // Errors on individual entries (a permission-denied directory, a
            // file deleted mid-walk) must not abort the whole walk.
            .filter_entry(|entry| !is_always_ignored(entry));

        let mut entries = Vec::new();
        for result in builder.build() {
            if entries.len() >= self.options.max_entries {
                tracing::warn!(
                    root = %self.root.display(),
                    limit = self.options.max_entries,
                    "project walk hit the entry limit; results are truncated"
                );
                break;
            }
            let entry = match result {
                Ok(entry) => entry,
                Err(err) => {
                    tracing::debug!(%err, "skipping unreadable entry during project walk");
                    continue;
                }
            };
            // The root itself is not an entry of its own tree.
            if entry.depth() == 0 {
                continue;
            }
            let path = entry.path();
            let Ok(relative) = path.strip_prefix(&self.root) else {
                continue;
            };
            let is_dir = entry.file_type().is_some_and(|t| t.is_dir());
            let size = if is_dir {
                0
            } else {
                match entry.metadata() {
                    Ok(m) => m.len(),
                    Err(_) => continue,
                }
            };
            if !is_dir
                && let Some(max) = self.options.max_file_size
                && size > max
            {
                continue;
            }
            entries.push(ProjectEntry {
                path: path.to_path_buf(),
                relative: relative.to_path_buf(),
                is_dir,
                size,
                language: if is_dir { None } else { nebula_core::document::detect_language(path) },
            });
        }
        entries.sort_by(|a, b| a.relative.cmp(&b.relative));
        Ok(entries)
    }

    /// Files whose detected language matches `language`.
    pub fn files_with_language(&self, language: &str) -> Result<Vec<ProjectEntry>> {
        Ok(self.files()?.into_iter().filter(|e| e.language.as_deref() == Some(language)).collect())
    }

    /// Read a project-relative file.
    pub fn read(&self, relative: impl AsRef<Path>) -> Result<Vec<u8>> {
        crate::read_bytes(self.resolve(relative)?)
    }

    /// Write a project-relative file atomically.
    pub fn write(&self, relative: impl AsRef<Path>, bytes: &[u8]) -> Result<()> {
        crate::write_atomic(self.resolve(relative)?, bytes)
    }

    /// The directory Nebula keeps per-project state in (`<root>/.nebula`).
    pub fn state_dir(&self) -> PathBuf {
        self.root.join(".nebula")
    }
}

fn is_always_ignored(entry: &DirEntry) -> bool {
    if entry.depth() == 0 {
        return false;
    }
    let Some(name) = entry.file_name().to_str() else {
        return false;
    };
    let is_dir = entry.file_type().is_some_and(|t| t.is_dir());
    is_dir && ALWAYS_IGNORED_DIRS.iter().any(|d| *d == name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    /// Build a small but realistic project on disk.
    fn fixture() -> TempDir {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::create_dir_all(root.join("target/debug")).unwrap();
        fs::create_dir_all(root.join("node_modules/pkg")).unwrap();
        fs::create_dir_all(root.join("generated")).unwrap();

        fs::write(root.join("Cargo.toml"), b"[package]\nname = \"x\"\n").unwrap();
        fs::write(root.join("src/main.rs"), b"fn main() {}\n").unwrap();
        fs::write(root.join("src/util.py"), b"def f(): pass\n").unwrap();
        fs::write(root.join("target/debug/artifact.bin"), b"\0\0\0").unwrap();
        fs::write(root.join("node_modules/pkg/index.js"), b"module.exports={}\n").unwrap();
        fs::write(root.join("generated/out.txt"), b"generated\n").unwrap();
        fs::write(root.join(".gitignore"), b"generated/\n").unwrap();
        fs::write(root.join(".hidden"), b"hidden\n").unwrap();
        dir
    }

    #[test]
    fn walk_skips_build_output_and_dependency_dirs() {
        let dir = fixture();
        let project = Project::open(dir.path()).unwrap();
        let files: Vec<String> =
            project.files().unwrap().iter().map(|e| e.relative.display().to_string()).collect();

        assert!(files.contains(&"src/main.rs".to_string()));
        assert!(files.contains(&"Cargo.toml".to_string()));
        assert!(
            !files.iter().any(|f| f.starts_with("target")),
            "target/ must never be walked: {files:?}"
        );
        assert!(!files.iter().any(|f| f.starts_with("node_modules")));
    }

    #[test]
    fn walk_honours_gitignore() {
        let dir = fixture();
        let project = Project::open(dir.path()).unwrap();
        let files: Vec<String> =
            project.files().unwrap().iter().map(|e| e.relative.display().to_string()).collect();
        assert!(
            !files.iter().any(|f| f.starts_with("generated")),
            "gitignored paths must be skipped: {files:?}"
        );
    }

    #[test]
    fn hidden_files_are_excluded_by_default_and_included_on_request() {
        let dir = fixture();

        let default = Project::open(dir.path()).unwrap();
        let names: Vec<_> =
            default.files().unwrap().iter().map(|e| e.relative.display().to_string()).collect();
        assert!(!names.contains(&".hidden".to_string()));

        let opts = WalkOptions { include_hidden: true, ..Default::default() };
        let with_hidden = Project::open_with(dir.path(), opts).unwrap();
        let names: Vec<_> =
            with_hidden.files().unwrap().iter().map(|e| e.relative.display().to_string()).collect();
        assert!(names.contains(&".hidden".to_string()));
    }

    #[test]
    fn languages_are_detected_during_the_walk() {
        let dir = fixture();
        let project = Project::open(dir.path()).unwrap();
        assert_eq!(project.files_with_language("rust").unwrap().len(), 1);
        assert_eq!(project.files_with_language("python").unwrap().len(), 1);
        assert_eq!(project.files_with_language("go").unwrap().len(), 0);
    }

    #[test]
    fn entries_are_returned_in_stable_sorted_order() {
        let dir = fixture();
        let project = Project::open(dir.path()).unwrap();
        let first = project.files().unwrap();
        let second = project.files().unwrap();
        assert_eq!(first, second, "walk order must be deterministic");
        let mut sorted = first.clone();
        sorted.sort_by(|a, b| a.relative.cmp(&b.relative));
        assert_eq!(first, sorted);
    }

    #[test]
    fn max_depth_limits_recursion() {
        let dir = fixture();
        let opts = WalkOptions { max_depth: Some(1), ..Default::default() };
        let project = Project::open_with(dir.path(), opts).unwrap();
        let files = project.files().unwrap();
        assert!(files.iter().all(|e| e.relative.components().count() == 1));
    }

    #[test]
    fn oversize_files_are_filtered_out() {
        let dir = fixture();
        fs::write(dir.path().join("src/big.rs"), vec![b'x'; 4096]).unwrap();
        let opts = WalkOptions { max_file_size: Some(1024), ..Default::default() };
        let project = Project::open_with(dir.path(), opts).unwrap();
        let files: Vec<String> =
            project.files().unwrap().iter().map(|e| e.relative.display().to_string()).collect();
        assert!(!files.contains(&"src/big.rs".to_string()));
    }

    #[test]
    fn project_read_and_write_go_through_containment() {
        let dir = fixture();
        let project = Project::open(dir.path()).unwrap();
        project.write("src/new.rs", b"fn new() {}\n").unwrap();
        assert_eq!(project.read("src/new.rs").unwrap(), b"fn new() {}\n");
        assert!(project.read("../outside.txt").is_err());
        assert!(project.write("../outside.txt", b"nope").is_err());
    }

    #[test]
    fn opening_a_file_as_a_project_is_an_error() {
        let dir = fixture();
        assert!(Project::open(dir.path().join("Cargo.toml")).is_err());
    }

    #[test]
    fn entry_sizes_are_reported() {
        let dir = fixture();
        let project = Project::open(dir.path()).unwrap();
        let main = project
            .files()
            .unwrap()
            .into_iter()
            .find(|e| e.relative == Path::new("src/main.rs"))
            .unwrap();
        assert_eq!(main.size, "fn main() {}\n".len() as u64);
        assert!(!main.is_dir);
    }
}
