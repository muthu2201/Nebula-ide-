//! The editor's state: which files are open, what is in them, and what the
//! parser currently thinks they mean.
//!
//! There is no window, no renderer and no event loop in this module. A
//! [`Workspace`] can be opened, edited, saved and closed entirely from a test,
//! which is what makes the end-to-end stress harness possible without a display
//! server.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use nebula_core::{Document, DocumentId};
use nebula_syntax::{GrammarRegistry, SyntaxTree};
use nebula_vfs::Project;

use crate::{IdeError, Result};

/// One open file.
#[derive(Debug)]
pub struct OpenFile {
    /// The text, cursors and history.
    pub document: Document,
    /// The parse tree, if the language has a grammar.
    ///
    /// `None` is a normal state — plain text, a log file, a language whose
    /// grammar is not bundled — and means the file is edited without
    /// highlighting rather than not opened.
    pub tree: Option<SyntaxTree>,
    /// The version the tree was parsed at, used to decide whether it is stale.
    tree_version: u64,
    /// When the document was last edited, for deciding whether typing has
    /// paused long enough to re-parse.
    last_edit: Option<std::time::Instant>,
}

impl OpenFile {
    /// Whether the parse tree matches the document's current text.
    pub fn tree_is_current(&self) -> bool {
        self.tree.is_none() || self.tree_version == self.document.version()
    }
}

/// The set of open files and the project they belong to.
#[derive(Debug)]
pub struct Workspace {
    /// The project root, if the editor was opened on a directory.
    project: Option<Project>,
    files: Vec<OpenFile>,
    by_id: HashMap<DocumentId, usize>,
    active: usize,
    grammars: GrammarRegistry,
}

impl Default for Workspace {
    fn default() -> Self {
        Self::new()
    }
}

impl Workspace {
    /// An empty workspace with one untitled document.
    ///
    /// The editor is never in a state with no document: "nothing is open" and
    /// "an empty scratch buffer is open" are the same thing to the user, and
    /// the second one has no special cases in the renderer.
    pub fn new() -> Self {
        let document = Document::new();
        let id = document.id();
        Self {
            project: None,
            files: vec![OpenFile { document, tree: None, tree_version: 0, last_edit: None }],
            by_id: HashMap::from([(id, 0)]),
            active: 0,
            grammars: GrammarRegistry::new(),
        }
    }

    /// A workspace rooted at a directory.
    pub fn open_project(root: impl AsRef<Path>) -> Result<Self> {
        let project = Project::open(root)?;
        let mut workspace = Self::new();
        workspace.project = Some(project);
        Ok(workspace)
    }

    /// The project, if there is one.
    pub fn project(&self) -> Option<&Project> {
        self.project.as_ref()
    }

    /// Every open file.
    pub fn files(&self) -> &[OpenFile] {
        &self.files
    }

    /// How many files are open.
    pub fn len(&self) -> usize {
        self.files.len()
    }

    /// Always false: a workspace always holds at least one document.
    pub fn is_empty(&self) -> bool {
        false
    }

    /// The focused file.
    pub fn active(&self) -> &OpenFile {
        &self.files[self.active]
    }

    /// The focused file, mutably.
    pub fn active_mut(&mut self) -> &mut OpenFile {
        &mut self.files[self.active]
    }

    /// The focused document.
    pub fn document(&self) -> &Document {
        &self.files[self.active].document
    }

    /// The focused document, mutably.
    pub fn document_mut(&mut self) -> &mut Document {
        &mut self.files[self.active].document
    }

    /// Which file is focused.
    pub fn active_index(&self) -> usize {
        self.active
    }

    /// Focus a file by index, ignoring an out-of-range one.
    pub fn focus(&mut self, index: usize) -> bool {
        if index < self.files.len() {
            self.active = index;
            true
        } else {
            false
        }
    }

    /// Focus the next file, wrapping.
    pub fn focus_next(&mut self) {
        self.active = (self.active + 1) % self.files.len();
    }

    /// Focus a file by document id.
    pub fn focus_id(&mut self, id: DocumentId) -> bool {
        match self.by_id.get(&id) {
            Some(&index) => {
                self.active = index;
                true
            }
            None => false,
        }
    }

    /// Open a file from disk, focusing it.
    ///
    /// Opening a file that is already open focuses the existing buffer rather
    /// than loading a second copy — two views of one file that can diverge is a
    /// data-loss bug, not a feature.
    pub fn open(&mut self, path: impl AsRef<Path>) -> Result<DocumentId> {
        let path = path.as_ref();
        let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());

        if let Some(index) = self.files.iter().position(|file| {
            file.document
                .path()
                .map(|p| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf()) == canonical)
                .unwrap_or(false)
        }) {
            self.active = index;
            return Ok(self.files[index].document.id());
        }

        let bytes = std::fs::read(path)
            .map_err(|error| IdeError::Open { path: path.to_path_buf(), source: error })?;
        let document = Document::from_bytes(canonical, &bytes)?;

        Ok(self.adopt(document))
    }

    /// Open an in-memory document, focusing it.
    pub fn open_document(&mut self, document: Document) -> DocumentId {
        self.adopt(document)
    }

    /// Add a document to the workspace and focus it.
    fn adopt(&mut self, document: Document) -> DocumentId {
        let id = document.id();
        let tree = self.parse(&document);
        let tree_version = document.version();

        // The initial untitled scratch buffer is replaced rather than kept
        // alongside: an unwanted empty tab on every launch is clutter.
        if self.files.len() == 1
            && self.files[0].document.path().is_none()
            && !self.files[0].document.is_modified()
            && self.files[0].document.buffer().is_empty()
        {
            self.by_id.remove(&self.files[0].document.id());
            self.files[0] = OpenFile { document, tree, tree_version, last_edit: None };
            self.by_id.insert(id, 0);
            self.active = 0;
            return id;
        }

        self.files.push(OpenFile { document, tree, tree_version, last_edit: None });
        self.active = self.files.len() - 1;
        self.by_id.insert(id, self.active);
        id
    }

    /// Parse a document, if a grammar for its language is bundled.
    fn parse(&mut self, document: &Document) -> Option<SyntaxTree> {
        let language = document.language()?.to_string();
        let grammar = self.grammars.get(&language).ok()?;
        match SyntaxTree::parse(grammar, document.buffer(), document.version()) {
            Ok(tree) => Some(tree),
            Err(error) => {
                // A grammar that fails on a real file is a bug worth recording,
                // but not a reason to refuse to open the file.
                tracing::warn!(%language, %error, "could not parse; opening without highlighting");
                None
            }
        }
    }

    /// Re-parse the focused file if its tree is out of date.
    ///
    /// Incremental where [`Workspace::note_edit`] kept the tree in step with the
    /// text, and a full re-parse otherwise.
    pub fn refresh_syntax(&mut self) -> Result<()> {
        let index = self.active;
        if self.files[index].tree_is_current() {
            return Ok(());
        }

        let version = self.files[index].document.version();
        let file = &mut self.files[index];
        let Some(tree) = file.tree.as_mut() else { return Ok(()) };

        // `tree_version == u64::MAX` is the marker for "the incremental state
        // was lost", where reusing the old tree would produce a wrong one.
        let result = if file.tree_version == u64::MAX {
            tree.reparse(file.document.buffer(), version)
        } else {
            tree.reparse_incremental(file.document.buffer(), version)
        };

        result?;
        file.tree_version = version;
        Ok(())
    }

    /// Re-parse the focused file only if typing has paused for `idle`.
    ///
    /// Returns whether a re-parse happened.
    ///
    /// ## Why the delay exists
    ///
    /// Even an incremental re-parse is bounded by how much of the *tree*
    /// changed, not the text: one character typed into a file with fifty
    /// thousand top-level items rebuilds the root's child list, which measures
    /// at around 150 ms on a 300 000-line file. Doing that between the keystroke
    /// and the frame would blow the entire budget twenty times over.
    ///
    /// So the editor paints from the edited-but-stale tree, whose offsets
    /// [`Workspace::note_edit`] has already shifted, and re-parses in the first
    /// gap in typing. On a normal file the re-parse is sub-millisecond and the
    /// delay is invisible; on a huge one it is the difference between an editor
    /// that responds and one that does not.
    pub fn refresh_syntax_if_idle(&mut self, idle: std::time::Duration) -> Result<bool> {
        let file = &self.files[self.active];
        if file.tree_is_current() {
            return Ok(false);
        }
        if file.last_edit.is_some_and(|at| at.elapsed() < idle) {
            return Ok(false);
        }

        self.refresh_syntax()?;
        Ok(true)
    }

    /// Tell the focused file's parse tree about an edit, so the next re-parse
    /// is incremental rather than a full reparse.
    pub fn note_edit(
        &mut self,
        before: &nebula_core::TextBuffer,
        transactions: &[nebula_core::Transaction],
    ) {
        let index = self.active;
        let file = &mut self.files[index];
        file.last_edit = Some(std::time::Instant::now());

        let Some(tree) = file.tree.as_mut() else { return };
        if transactions.is_empty() {
            return;
        }

        // A grouped edit is several transactions, each in the coordinates that
        // were current when it ran. tree-sitter has to be told about the extent
        // of the whole change, and the outer buffers bracket it: replaying the
        // sequence against intermediate buffers would mean reconstructing each
        // one, which is what this avoids.
        let combined = nebula_core::Transaction::from_edits(
            transactions.iter().flat_map(|t| t.edits().iter().cloned()),
        );

        // Only the offset shift, never a re-parse: this runs between the
        // keystroke and the frame.
        let applied = match combined {
            Ok(combined) => tree.edit(before, file.document.buffer(), &combined),
            Err(error) => Err(nebula_syntax::SyntaxError::from(error)),
        };

        if let Err(error) = applied {
            // Not fatal: the marker makes the next refresh do a full re-parse,
            // which is slower but always correct.
            tracing::debug!(%error, "incremental edit rejected; falling back to a full reparse");
            file.tree_version = u64::MAX;
        }
    }

    /// Save the focused document.
    pub fn save(&mut self) -> Result<PathBuf> {
        let document = &mut self.files[self.active].document;
        let Some(path) = document.path().map(Path::to_path_buf) else {
            return Err(IdeError::NoPath);
        };

        // Write to a sibling temporary file and rename over the target, so an
        // interrupted save cannot leave a half-written source file behind.
        let bytes = document.to_bytes();
        write_atomically(&path, &bytes)
            .map_err(|error| IdeError::Save { path: path.clone(), source: error })?;

        document.mark_saved();
        Ok(path)
    }

    /// Save the focused document to a new path.
    pub fn save_as(&mut self, path: impl Into<PathBuf>) -> Result<PathBuf> {
        let path = path.into();
        let language = nebula_core::document::detect_language(&path);
        {
            let document = &mut self.files[self.active].document;
            document.meta_mut().path = Some(path.clone());
            document.meta_mut().language = language;
        }

        let saved = self.save()?;

        // The language may have changed with the extension, so the parse tree
        // has to be rebuilt rather than reparsed.
        let index = self.active;
        let document = &self.files[index].document;
        let tree_version = document.version();
        let tree = {
            let document = std::mem::replace(&mut self.files[index].document, Document::new());
            let tree = self.parse(&document);
            self.files[index].document = document;
            tree
        };
        self.files[index].tree = tree;
        self.files[index].tree_version = tree_version;

        Ok(saved)
    }

    /// Close the focused file.
    ///
    /// Refuses to discard unsaved changes unless `force` is set: losing an
    /// hour's work to a mistyped shortcut is the worst thing an editor can do.
    pub fn close(&mut self, force: bool) -> Result<()> {
        if !force && self.files[self.active].document.is_modified() {
            return Err(IdeError::UnsavedChanges);
        }

        if self.files.len() == 1 {
            // Closing the last file leaves an empty scratch buffer rather than
            // an empty workspace.
            let document = Document::new();
            let id = document.id();
            self.by_id.clear();
            self.by_id.insert(id, 0);
            self.files[0] = OpenFile { document, tree: None, tree_version: 0, last_edit: None };
            self.active = 0;
            return Ok(());
        }

        self.files.remove(self.active);
        if self.active >= self.files.len() {
            self.active = self.files.len() - 1;
        }
        self.reindex();
        Ok(())
    }

    /// Whether any open file has unsaved changes.
    pub fn has_unsaved_changes(&self) -> bool {
        self.files.iter().any(|file| file.document.is_modified())
    }

    /// The paths of every file with unsaved changes.
    pub fn unsaved(&self) -> Vec<PathBuf> {
        self.files
            .iter()
            .filter(|file| file.document.is_modified())
            .filter_map(|file| file.document.path().map(Path::to_path_buf))
            .collect()
    }

    /// Rebuild the id → index map after the file list changes.
    fn reindex(&mut self) {
        self.by_id.clear();
        for (index, file) in self.files.iter().enumerate() {
            self.by_id.insert(file.document.id(), index);
        }
    }

    /// Which languages have a bundled grammar.
    pub fn supported_languages() -> Vec<&'static str> {
        GrammarRegistry::available_languages()
    }
}

/// Write a file so that a crash mid-write cannot destroy the old contents.
fn write_atomically(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let directory = path.parent().unwrap_or(Path::new("."));
    let mut temporary = tempfile::NamedTempFile::new_in(directory)?;
    std::io::Write::write_all(&mut temporary, bytes)?;
    // fsync before the rename: on a crash, a rename that lands before the data
    // does leaves a file full of zeroes.
    temporary.as_file().sync_all()?;

    // Preserve the original's permissions — writing through a temp file
    // otherwise silently turns an executable script into a non-executable one.
    #[cfg(unix)]
    if let Ok(metadata) = std::fs::metadata(path) {
        use std::os::unix::fs::PermissionsExt;
        let mode = metadata.permissions().mode();
        temporary.as_file().set_permissions(std::fs::Permissions::from_mode(mode))?;
    }

    temporary.persist(path).map_err(|error| error.error)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn project_with(files: &[(&str, &str)]) -> TempDir {
        let dir = TempDir::new().unwrap();
        for (name, contents) in files {
            let path = dir.path().join(name);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(path, contents).unwrap();
        }
        dir
    }

    #[test]
    fn a_new_workspace_has_one_empty_scratch_buffer() {
        let workspace = Workspace::new();
        assert_eq!(workspace.len(), 1);
        assert!(workspace.document().buffer().is_empty());
        assert!(workspace.document().path().is_none());
    }

    #[test]
    fn opening_a_file_reads_it_and_focuses_it() {
        let dir = project_with(&[("main.rs", "fn main() {}\n")]);
        let mut workspace = Workspace::new();
        workspace.open(dir.path().join("main.rs")).unwrap();

        assert_eq!(workspace.document().text(), "fn main() {}\n");
        assert_eq!(workspace.document().language(), Some("rust"));
    }

    #[test]
    fn the_empty_scratch_buffer_is_replaced_rather_than_left_behind() {
        let dir = project_with(&[("a.rs", "fn a() {}")]);
        let mut workspace = Workspace::new();
        workspace.open(dir.path().join("a.rs")).unwrap();
        assert_eq!(workspace.len(), 1, "an unwanted empty tab was kept");
    }

    #[test]
    fn a_second_file_opens_alongside_the_first() {
        let dir = project_with(&[("a.rs", "fn a() {}"), ("b.rs", "fn b() {}")]);
        let mut workspace = Workspace::new();
        workspace.open(dir.path().join("a.rs")).unwrap();
        workspace.open(dir.path().join("b.rs")).unwrap();

        assert_eq!(workspace.len(), 2);
        assert_eq!(workspace.document().text(), "fn b() {}");
    }

    #[test]
    fn opening_the_same_file_twice_focuses_the_existing_buffer() {
        // Two independent buffers over one file is how edits get silently lost.
        let dir = project_with(&[("a.rs", "fn a() {}")]);
        let mut workspace = Workspace::new();

        let first = workspace.open(dir.path().join("a.rs")).unwrap();
        workspace.document_mut().set_caret(0);
        workspace.document_mut().insert_at_cursors("// ", false).unwrap();

        let second = workspace.open(dir.path().join("a.rs")).unwrap();
        assert_eq!(first, second);
        assert_eq!(workspace.len(), 1);
        assert!(workspace.document().text().starts_with("// "), "the edit was lost");
    }

    #[test]
    fn opening_a_missing_file_names_the_path() {
        let mut workspace = Workspace::new();
        let error = workspace.open("/nonexistent/nowhere.rs").unwrap_err();
        assert!(error.to_string().contains("nowhere.rs"), "{error}");
    }

    #[test]
    fn a_source_file_gets_a_parse_tree() {
        let dir = project_with(&[("main.rs", "fn main() { let x = 1; }")]);
        let mut workspace = Workspace::new();
        workspace.open(dir.path().join("main.rs")).unwrap();

        let tree = workspace.active().tree.as_ref().expect("no parse tree");
        assert!(!tree.has_error());
    }

    #[test]
    fn a_plain_text_file_opens_without_a_parse_tree() {
        let dir = project_with(&[("notes.txt", "just some words")]);
        let mut workspace = Workspace::new();
        workspace.open(dir.path().join("notes.txt")).unwrap();

        assert!(workspace.active().tree.is_none());
        assert_eq!(workspace.document().text(), "just some words");
    }

    #[test]
    fn a_file_with_syntax_errors_still_opens() {
        let dir = project_with(&[("broken.rs", "fn main( { this is not rust")]);
        let mut workspace = Workspace::new();
        workspace.open(dir.path().join("broken.rs")).unwrap();

        let tree = workspace.active().tree.as_ref().unwrap();
        assert!(tree.has_error(), "the parser should notice");
        assert!(workspace.document().text().contains("not rust"), "but the file still opens");
    }

    #[test]
    fn editing_marks_the_tree_stale_and_refreshing_brings_it_back() {
        let dir = project_with(&[("main.rs", "fn main() {}")]);
        let mut workspace = Workspace::new();
        workspace.open(dir.path().join("main.rs")).unwrap();
        assert!(workspace.active().tree_is_current());

        workspace.document_mut().set_caret(11);
        workspace.document_mut().insert_at_cursors(" let x = 1;", false).unwrap();
        assert!(!workspace.active().tree_is_current());

        workspace.refresh_syntax().unwrap();
        assert!(workspace.active().tree_is_current());
        assert!(!workspace.active().tree.as_ref().unwrap().has_error());
    }

    #[test]
    fn saving_writes_the_file_and_clears_the_modified_flag() {
        let dir = project_with(&[("main.rs", "fn main() {}")]);
        let path = dir.path().join("main.rs");

        let mut workspace = Workspace::new();
        workspace.open(&path).unwrap();
        workspace.document_mut().set_caret(0);
        workspace.document_mut().insert_at_cursors("// header\n", false).unwrap();
        assert!(workspace.document().is_modified());

        workspace.save().unwrap();
        assert!(!workspace.document().is_modified());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "// header\nfn main() {}");
    }

    #[test]
    fn saving_an_untitled_document_is_refused_rather_than_guessed_at() {
        let mut workspace = Workspace::new();
        workspace.document_mut().insert_at_cursors("scratch", false).unwrap();
        assert!(matches!(workspace.save(), Err(IdeError::NoPath)));
    }

    #[test]
    fn save_as_writes_a_new_file_and_re_detects_the_language() {
        let dir = TempDir::new().unwrap();
        let mut workspace = Workspace::new();
        workspace.document_mut().insert_at_cursors("fn main() {}", false).unwrap();

        let path = dir.path().join("new.rs");
        workspace.save_as(&path).unwrap();

        assert_eq!(std::fs::read_to_string(&path).unwrap(), "fn main() {}");
        assert_eq!(workspace.document().language(), Some("rust"));
        assert!(workspace.active().tree.is_some(), "the new language got no parse tree");
    }

    #[test]
    fn saving_preserves_the_files_executable_bit() {
        // Saving through a temporary file is how a shell script silently stops
        // being runnable.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            let dir = project_with(&[("run.sh", "#!/bin/sh\necho hi\n")]);
            let path = dir.path().join("run.sh");
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();

            let mut workspace = Workspace::new();
            workspace.open(&path).unwrap();
            let end = workspace.document().buffer().len_chars();
            workspace.document_mut().set_caret(end);
            workspace.document_mut().insert_at_cursors("echo bye\n", false).unwrap();
            workspace.save().unwrap();

            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o755, "the executable bit was lost");
        }
    }

    #[test]
    fn saving_leaves_no_temporary_files_behind() {
        let dir = project_with(&[("main.rs", "fn main() {}")]);
        let mut workspace = Workspace::new();
        workspace.open(dir.path().join("main.rs")).unwrap();
        workspace.document_mut().set_caret(0);
        workspace.document_mut().insert_at_cursors("//\n", false).unwrap();
        workspace.save().unwrap();

        let entries: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(entries, vec!["main.rs".to_string()], "{entries:?}");
    }

    #[test]
    fn closing_a_modified_file_is_refused_by_default() {
        let dir = project_with(&[("main.rs", "fn main() {}")]);
        let mut workspace = Workspace::new();
        workspace.open(dir.path().join("main.rs")).unwrap();
        workspace.document_mut().set_caret(0);
        workspace.document_mut().insert_at_cursors("x", false).unwrap();

        assert!(matches!(workspace.close(false), Err(IdeError::UnsavedChanges)));
        assert_eq!(workspace.len(), 1, "the file was closed anyway");

        workspace.close(true).unwrap();
        assert!(workspace.document().buffer().is_empty());
    }

    #[test]
    fn closing_the_last_file_leaves_a_scratch_buffer_not_an_empty_workspace() {
        let dir = project_with(&[("main.rs", "fn main() {}")]);
        let mut workspace = Workspace::new();
        workspace.open(dir.path().join("main.rs")).unwrap();
        workspace.close(false).unwrap();

        assert_eq!(workspace.len(), 1);
        assert!(workspace.document().path().is_none());
        assert!(!workspace.is_empty());
    }

    #[test]
    fn closing_one_of_several_files_focuses_a_remaining_one() {
        let dir = project_with(&[("a.rs", "fn a(){}"), ("b.rs", "fn b(){}"), ("c.rs", "fn c(){}")]);
        let mut workspace = Workspace::new();
        for name in ["a.rs", "b.rs", "c.rs"] {
            workspace.open(dir.path().join(name)).unwrap();
        }
        assert_eq!(workspace.len(), 3);

        workspace.close(false).unwrap();
        assert_eq!(workspace.len(), 2);
        assert!(workspace.document().text().contains("fn b"));
    }

    #[test]
    fn focus_cycles_through_every_open_file() {
        let dir = project_with(&[("a.rs", "fn a(){}"), ("b.rs", "fn b(){}")]);
        let mut workspace = Workspace::new();
        workspace.open(dir.path().join("a.rs")).unwrap();
        workspace.open(dir.path().join("b.rs")).unwrap();

        assert_eq!(workspace.active_index(), 1);
        workspace.focus_next();
        assert_eq!(workspace.active_index(), 0);
        workspace.focus_next();
        assert_eq!(workspace.active_index(), 1);
    }

    #[test]
    fn a_file_can_be_focused_by_its_document_id() {
        let dir = project_with(&[("a.rs", "fn a(){}"), ("b.rs", "fn b(){}")]);
        let mut workspace = Workspace::new();
        let first = workspace.open(dir.path().join("a.rs")).unwrap();
        workspace.open(dir.path().join("b.rs")).unwrap();

        assert!(workspace.focus_id(first));
        assert!(workspace.document().text().contains("fn a"));
        assert!(!workspace.focus_id(DocumentId::next()), "an unknown id must not focus anything");
    }

    #[test]
    fn focusing_out_of_range_is_refused_rather_than_panicking() {
        let mut workspace = Workspace::new();
        assert!(!workspace.focus(99));
        assert_eq!(workspace.active_index(), 0);
    }

    #[test]
    fn unsaved_changes_are_reported_across_every_open_file() {
        let dir = project_with(&[("a.rs", "fn a(){}"), ("b.rs", "fn b(){}")]);
        let mut workspace = Workspace::new();
        workspace.open(dir.path().join("a.rs")).unwrap();
        workspace.open(dir.path().join("b.rs")).unwrap();
        assert!(!workspace.has_unsaved_changes());

        workspace.focus(0);
        workspace.document_mut().set_caret(0);
        workspace.document_mut().insert_at_cursors("x", false).unwrap();

        assert!(workspace.has_unsaved_changes());
        let unsaved = workspace.unsaved();
        assert_eq!(unsaved.len(), 1);
        assert!(unsaved[0].ends_with("a.rs"));
    }

    #[test]
    fn a_project_workspace_knows_its_root() {
        let dir = project_with(&[("src/main.rs", "fn main() {}")]);
        let workspace = Workspace::open_project(dir.path()).unwrap();
        assert!(workspace.project().is_some());
        assert_eq!(workspace.project().unwrap().files().unwrap().len(), 1);
    }

    #[test]
    fn crlf_files_keep_their_line_endings_through_a_save() {
        // Silently rewriting every line ending turns a one-line change into a
        // whole-file diff, which is how a reviewer stops reading a pull request.
        let dir = project_with(&[("win.rs", "fn main() {\r\n}\r\n")]);
        let path = dir.path().join("win.rs");

        let mut workspace = Workspace::new();
        workspace.open(&path).unwrap();
        workspace.document_mut().set_caret(11);
        workspace.document_mut().insert_at_cursors(" ", false).unwrap();
        workspace.save().unwrap();

        let written = std::fs::read(&path).unwrap();
        assert!(written.windows(2).any(|w| w == b"\r\n"), "CRLF was rewritten to LF");
    }

    #[test]
    fn every_bundled_language_can_be_opened() {
        // A grammar that is registered but does not load is a crash waiting for
        // whoever opens that file type.
        let dir = TempDir::new().unwrap();
        for language in Workspace::supported_languages() {
            let extension = match language {
                "rust" => "rs",
                "python" => "py",
                "javascript" => "js",
                "typescript" => "ts",
                "typescriptreact" => "tsx",
                "go" => "go",
                "c" => "c",
                "cpp" => "cpp",
                "json" => "json",
                "toml" => "toml",
                "markdown" => "md",
                "html" => "html",
                "css" => "css",
                "bash" => "sh",
                "yaml" => "yaml",
                other => panic!("no extension mapped for the `{other}` grammar"),
            };
            let path = dir.path().join(format!("sample.{extension}"));
            std::fs::write(&path, "x").unwrap();

            let mut workspace = Workspace::new();
            workspace.open(&path).unwrap();
            assert_eq!(
                workspace.document().language(),
                Some(language),
                "opening a .{extension} file did not select the {language} grammar"
            );
            assert!(workspace.active().tree.is_some(), "{language} produced no parse tree");
        }
    }
}
