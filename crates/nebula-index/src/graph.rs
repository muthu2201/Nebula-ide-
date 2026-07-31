//! Building the symbol graph from a project tree.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use nebula_syntax::{GrammarRegistry, Symbol, SyntaxTree};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};

use crate::Result;

/// One file in the graph.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileNode {
    /// Path relative to the project root.
    pub path: PathBuf,
    /// Language identifier.
    pub language: String,
    /// Definitions this file provides.
    pub definitions: Vec<Symbol>,
    /// Identifiers this file references, with occurrence counts.
    pub references: HashMap<String, usize>,
    /// Number of lines, used to break ranking ties towards substantive files.
    pub lines: usize,
}

/// The whole-project symbol graph.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SymbolGraph {
    /// Files, in a stable order that node indices refer to.
    pub files: Vec<FileNode>,
    /// Which files define each identifier: `name -> file indices`.
    definitions_by_name: HashMap<String, Vec<usize>>,
}

impl SymbolGraph {
    /// An empty graph.
    pub fn new() -> Self {
        Self::default()
    }

    /// Build a graph by parsing every supported file in `project`.
    ///
    /// Files are parsed in parallel; a file that fails to parse is skipped with
    /// a debug log rather than failing the build, because one unparseable file
    /// must not deprive the user of a repo map for the other four thousand.
    pub fn build(project: &nebula_vfs::Project) -> Result<Self> {
        let entries = project.files()?;

        let nodes: Vec<FileNode> = entries
            .par_iter()
            .filter_map(|entry| {
                let language = entry.language.as_deref()?;
                if !GrammarRegistry::supports(language) {
                    return None;
                }
                match Self::parse_file(&entry.path, &entry.relative, language) {
                    Ok(node) => Some(node),
                    Err(err) => {
                        tracing::debug!(
                            path = %entry.relative.display(),
                            %err,
                            "skipping file while building the symbol graph"
                        );
                        None
                    }
                }
            })
            .collect();

        Ok(Self::from_nodes(nodes))
    }

    /// Build a graph from already-parsed nodes.
    pub fn from_nodes(mut files: Vec<FileNode>) -> Self {
        // A stable order keeps node indices — and therefore ranking output —
        // reproducible across runs.
        files.sort_by(|a, b| a.path.cmp(&b.path));

        let mut definitions_by_name: HashMap<String, Vec<usize>> = HashMap::new();
        for (index, file) in files.iter().enumerate() {
            for symbol in &file.definitions {
                definitions_by_name.entry(symbol.name.clone()).or_default().push(index);
            }
        }
        Self { files, definitions_by_name }
    }

    /// Parse one file into a node.
    fn parse_file(absolute: &Path, relative: &Path, language: &str) -> Result<FileNode> {
        // Each worker gets its own registry rather than sharing one behind a
        // lock; compiling a grammar's queries once per thread is far cheaper
        // than serialising every parse through a mutex.
        thread_local! {
            static REGISTRY: GrammarRegistry = GrammarRegistry::new();
        }

        let bytes = nebula_vfs::read_bytes(absolute)?;
        if nebula_vfs::is_binary(&bytes) {
            return Ok(FileNode {
                path: relative.to_path_buf(),
                language: language.to_string(),
                definitions: Vec::new(),
                references: HashMap::new(),
                lines: 0,
            });
        }
        let buffer = nebula_core::TextBuffer::from_bytes(&bytes)
            .map_err(nebula_syntax::SyntaxError::Core)?;

        let (definitions, references) = REGISTRY.with(|registry| -> Result<_> {
            let grammar = registry.get(language)?;
            let tree = SyntaxTree::parse(grammar, &buffer, 0)?;
            let definitions = nebula_syntax::symbols::symbols(&tree, &buffer)?;
            let raw_references = nebula_syntax::symbols::references(&tree, &buffer)?;

            let mut references: HashMap<String, usize> = HashMap::new();
            for reference in raw_references {
                *references.entry(reference.name).or_insert(0) += 1;
            }
            Ok((definitions, references))
        })?;

        Ok(FileNode {
            path: relative.to_path_buf(),
            language: language.to_string(),
            definitions,
            references,
            lines: buffer.len_lines(),
        })
    }

    /// Number of files.
    pub fn len(&self) -> usize {
        self.files.len()
    }

    /// Whether the graph is empty.
    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    /// Find a file's node index by relative path.
    pub fn index_of(&self, path: &Path) -> Option<usize> {
        self.files.iter().position(|f| f.path == path)
    }

    /// Which files define `name`.
    pub fn definers_of(&self, name: &str) -> &[usize] {
        self.definitions_by_name.get(name).map(Vec::as_slice).unwrap_or(&[])
    }

    /// Total number of definitions across the project.
    pub fn definition_count(&self) -> usize {
        self.files.iter().map(|f| f.definitions.len()).sum()
    }

    /// Build the PageRank graph: an edge from each referencing file to each
    /// file that defines the referenced symbol.
    ///
    /// Two weighting decisions matter here:
    ///
    /// * A reference is worth `sqrt(count)` rather than `count`, so a file that
    ///   calls `log()` two hundred times does not out-vote a file that uses a
    ///   domain type once in a way that actually indicates coupling.
    /// * A symbol defined in many files (`new`, `main`, `get`) has its weight
    ///   divided across those definers, which is the same idea as inverse
    ///   document frequency: a name that is everywhere tells you nothing.
    pub fn to_pagerank(&self) -> crate::PageRank {
        let mut graph = crate::PageRank::new(self.files.len());

        for (from, file) in self.files.iter().enumerate() {
            for (name, count) in &file.references {
                let definers = self.definers_of(name);
                if definers.is_empty() {
                    continue;
                }
                // Common names carry proportionally less signal.
                let spread = definers.len() as f64;
                let weight = (*count as f64).sqrt() / spread;

                for &to in definers {
                    // A file referencing its own definitions says nothing about
                    // inter-file importance.
                    if to != from {
                        graph.add_edge(from, to, weight);
                    }
                }
            }
        }
        graph
    }

    /// Serialise the graph, for caching between sessions.
    pub fn to_bytes(&self) -> Result<Vec<u8>> {
        bincode::serde::encode_to_vec(self, bincode::config::standard())
            .map_err(|e| crate::IndexError::Serialization(e.to_string()))
    }

    /// Restore a cached graph.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        let (graph, _) = bincode::serde::decode_from_slice(bytes, bincode::config::standard())
            .map_err(|e| crate::IndexError::Serialization(e.to_string()))?;
        Ok(graph)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    /// A small project with a deliberate dependency structure:
    /// `core.rs` defines what everything else uses.
    fn fixture() -> TempDir {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("src")).unwrap();

        fs::write(
            root.join("src/core.rs"),
            "pub struct Engine {}\n\
             pub fn start_engine() -> Engine { Engine {} }\n\
             pub fn stop_engine(e: Engine) {}\n",
        )
        .unwrap();

        fs::write(
            root.join("src/service.rs"),
            "pub fn run() {\n    let e = start_engine();\n    stop_engine(e);\n}\n\
             pub fn restart() {\n    let e = start_engine();\n    stop_engine(e);\n}\n",
        )
        .unwrap();

        fs::write(
            root.join("src/cli.rs"),
            "pub fn main_entry() {\n    run();\n    restart();\n}\n",
        )
        .unwrap();

        fs::write(root.join("src/unrelated.rs"), "pub fn lonely_helper() {}\n").unwrap();
        dir
    }

    fn build() -> SymbolGraph {
        let dir = fixture();
        let project = nebula_vfs::Project::open(dir.path()).unwrap();
        SymbolGraph::build(&project).unwrap()
    }

    #[test]
    fn every_source_file_becomes_a_node() {
        let graph = build();
        assert_eq!(graph.len(), 4);
        let paths: Vec<String> = graph.files.iter().map(|f| f.path.display().to_string()).collect();
        assert!(paths.contains(&"src/core.rs".to_string()), "{paths:?}");
    }

    #[test]
    fn definitions_are_collected_per_file() {
        let graph = build();
        let core_index = graph.index_of(Path::new("src/core.rs")).unwrap();
        let names: Vec<&str> =
            graph.files[core_index].definitions.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"Engine"), "{names:?}");
        assert!(names.contains(&"start_engine"), "{names:?}");
    }

    #[test]
    fn references_are_counted() {
        let graph = build();
        let service = graph.index_of(Path::new("src/service.rs")).unwrap();
        let references = &graph.files[service].references;
        assert_eq!(
            references.get("start_engine").copied(),
            Some(2),
            "start_engine is called twice: {references:?}"
        );
    }

    #[test]
    fn definitions_are_indexed_by_name() {
        let graph = build();
        let core = graph.index_of(Path::new("src/core.rs")).unwrap();
        assert_eq!(graph.definers_of("start_engine"), &[core]);
        assert!(graph.definers_of("nonexistent_symbol").is_empty());
    }

    #[test]
    fn the_most_depended_on_file_ranks_highest() {
        let graph = build();
        let ranks = graph.to_pagerank().rank_uniform();

        let core = graph.index_of(Path::new("src/core.rs")).unwrap();
        let unrelated = graph.index_of(Path::new("src/unrelated.rs")).unwrap();

        assert!(
            ranks[core] > ranks[unrelated],
            "core.rs ({:.4}) should outrank unrelated.rs ({:.4})",
            ranks[core],
            ranks[unrelated]
        );
    }

    #[test]
    fn self_references_do_not_create_edges() {
        let dir = TempDir::new().unwrap();
        fs::write(
            dir.path().join("solo.rs"),
            "fn helper() {}\nfn caller() { helper(); helper(); }\n",
        )
        .unwrap();
        let project = nebula_vfs::Project::open(dir.path()).unwrap();
        let graph = SymbolGraph::build(&project).unwrap();

        assert_eq!(
            graph.to_pagerank().edge_count(),
            0,
            "a file calling its own functions is not a dependency"
        );
    }

    #[test]
    fn common_names_are_down_weighted() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        // `new` is defined in three files; `unique_thing` in one.
        for name in ["a", "b", "c"] {
            fs::write(root.join(format!("{name}.rs")), "pub fn new() {}\n").unwrap();
        }
        fs::write(root.join("d.rs"), "pub fn unique_thing() {}\n").unwrap();
        fs::write(root.join("user.rs"), "fn use_it() { new(); unique_thing(); }\n").unwrap();

        let project = nebula_vfs::Project::open(root).unwrap();
        let graph = SymbolGraph::build(&project).unwrap();
        let ranks = graph.to_pagerank().rank_uniform();

        let unique = graph.index_of(Path::new("d.rs")).unwrap();
        let common = graph.index_of(Path::new("a.rs")).unwrap();
        assert!(
            ranks[unique] > ranks[common],
            "a uniquely-named definition ({:.4}) should outrank one of three `new`s ({:.4})",
            ranks[unique],
            ranks[common]
        );
    }

    #[test]
    fn unsupported_languages_are_skipped_without_error() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("code.rs"), "fn f() {}\n").unwrap();
        fs::write(dir.path().join("notes.xyz"), "not a language we parse\n").unwrap();
        fs::write(dir.path().join("image.png"), [0x89u8, 0x50, 0x4E, 0x47, 0x00, 0x01]).unwrap();

        let project = nebula_vfs::Project::open(dir.path()).unwrap();
        let graph = SymbolGraph::build(&project).unwrap();
        assert_eq!(graph.len(), 1);
    }

    #[test]
    fn an_empty_project_produces_an_empty_graph() {
        let dir = TempDir::new().unwrap();
        let project = nebula_vfs::Project::open(dir.path()).unwrap();
        let graph = SymbolGraph::build(&project).unwrap();
        assert!(graph.is_empty());
        assert!(graph.to_pagerank().rank_uniform().is_empty());
    }

    #[test]
    fn graph_construction_is_deterministic() {
        let dir = fixture();
        let project = nebula_vfs::Project::open(dir.path()).unwrap();
        let first = SymbolGraph::build(&project).unwrap();
        let second = SymbolGraph::build(&project).unwrap();
        assert_eq!(
            first.files, second.files,
            "parallel parsing must still produce a stable node order"
        );
    }

    #[test]
    fn the_graph_round_trips_through_its_cache_format() {
        let graph = build();
        let bytes = graph.to_bytes().unwrap();
        let restored = SymbolGraph::from_bytes(&bytes).unwrap();
        assert_eq!(restored.files, graph.files);
        assert_eq!(restored.definers_of("start_engine"), graph.definers_of("start_engine"));
    }

    #[test]
    fn cross_language_projects_are_handled() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("a.rs"), "pub fn shared_name() {}\n").unwrap();
        fs::write(dir.path().join("b.py"), "def other():\n    shared_name()\n").unwrap();
        fs::write(dir.path().join("c.go"), "package main\nfunc main() {}\n").unwrap();

        let project = nebula_vfs::Project::open(dir.path()).unwrap();
        let graph = SymbolGraph::build(&project).unwrap();
        assert_eq!(graph.len(), 3);
        assert!(graph.definition_count() >= 3);
    }
}
