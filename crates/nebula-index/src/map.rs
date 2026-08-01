//! Rendering the repo map.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use nebula_syntax::{Symbol, SymbolKind};
use serde::{Deserialize, Serialize};

use crate::graph::SymbolGraph;
use crate::{PageRankConfig, Result, tokens};

/// How to build and render a repo map.
#[derive(Debug, Clone)]
pub struct RepoMapOptions {
    /// Token budget for the rendered map.
    pub token_budget: usize,
    /// Files the user currently has open. These seed the personalised teleport
    /// distribution, and are always included in the output.
    pub focus_files: Vec<PathBuf>,
    /// Files to leave out entirely — normally the ones already quoted in full
    /// elsewhere in the prompt, since repeating them wastes budget.
    pub exclude_files: Vec<PathBuf>,
    /// Maximum symbols shown per file.
    pub max_symbols_per_file: usize,
    /// PageRank tuning.
    pub pagerank: PageRankConfig,
}

impl Default for RepoMapOptions {
    fn default() -> Self {
        Self {
            token_budget: 1024,
            focus_files: Vec::new(),
            exclude_files: Vec::new(),
            max_symbols_per_file: 12,
            pagerank: PageRankConfig::default(),
        }
    }
}

impl RepoMapOptions {
    /// Set the token budget.
    pub fn budget(mut self, tokens: usize) -> Self {
        self.token_budget = tokens;
        self
    }

    /// Add a focus file.
    pub fn focus(mut self, path: impl Into<PathBuf>) -> Self {
        self.focus_files.push(path.into());
        self
    }

    /// Exclude a file from the map.
    pub fn exclude(mut self, path: impl Into<PathBuf>) -> Self {
        self.exclude_files.push(path.into());
        self
    }
}

/// A file's entry in the ranked map.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RankedFile {
    /// Path relative to the project root.
    pub path: PathBuf,
    /// PageRank score.
    pub score: f64,
    /// The symbols selected for this file, most important first.
    pub symbols: Vec<Symbol>,
}

/// A ranked, renderable summary of a project.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RepoMap {
    /// Files, highest-ranked first.
    pub files: Vec<RankedFile>,
}

impl RepoMap {
    /// Build a map from a project.
    pub fn build(project: &nebula_vfs::Project, options: &RepoMapOptions) -> Result<Self> {
        let graph = SymbolGraph::build(project)?;
        Ok(Self::from_graph(&graph, options))
    }

    /// Build a map from an already-constructed graph.
    ///
    /// Separated from [`RepoMap::build`] so the graph can be cached across
    /// sessions and re-ranked cheaply whenever the user's focus files change —
    /// re-ranking is milliseconds, re-parsing the project is not.
    pub fn from_graph(graph: &SymbolGraph, options: &RepoMapOptions) -> Self {
        if graph.is_empty() {
            return Self::default();
        }

        let excluded: HashSet<&Path> = options.exclude_files.iter().map(PathBuf::as_path).collect();

        // Seed the teleport distribution on the focus files.
        let personalization: Vec<f64> = {
            let focus: HashSet<&Path> = options.focus_files.iter().map(PathBuf::as_path).collect();
            if focus.is_empty() {
                Vec::new()
            } else {
                graph
                    .files
                    .iter()
                    .map(|f| if focus.contains(f.path.as_path()) { 1.0 } else { 0.0 })
                    .collect()
            }
        };

        let ranks = graph.to_pagerank().rank(&personalization, &options.pagerank);

        let mut ranked: Vec<RankedFile> = graph
            .files
            .iter()
            .enumerate()
            .filter(|(_, file)| !excluded.contains(file.path.as_path()))
            .filter(|(_, file)| !file.definitions.is_empty())
            .map(|(index, file)| {
                let mut symbols = file.definitions.clone();
                symbols.sort_by(|a, b| {
                    // Types and functions before fields and variables, then by
                    // position so the output reads like the file does.
                    kind_priority(a.kind)
                        .cmp(&kind_priority(b.kind))
                        .then(a.range.start.cmp(&b.range.start))
                });
                symbols.truncate(options.max_symbols_per_file);

                RankedFile {
                    path: file.path.clone(),
                    score: ranks.get(index).copied().unwrap_or(0.0),
                    symbols,
                }
            })
            .collect();

        ranked.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
                // Ties broken by path so output is reproducible.
                .then(a.path.cmp(&b.path))
        });

        Self { files: ranked }
    }

    /// Number of files in the map.
    pub fn len(&self) -> usize {
        self.files.len()
    }

    /// Whether the map is empty.
    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    /// Render the map as text, fitting inside `options.token_budget`.
    ///
    /// Files are emitted in rank order and the loop stops when the next file
    /// would overflow the budget — so the budget is a hard ceiling, and what
    /// gets dropped is always the least important thing.
    pub fn render(&self, options: &RepoMapOptions) -> String {
        let mut out = String::new();
        let mut used = 0usize;

        for file in &self.files {
            let block = render_file(file);
            let cost = tokens::estimate(&block);

            if used + cost > options.token_budget {
                // Try a header-only entry, which is far cheaper — knowing a file
                // exists is worth something even without its symbols.
                let minimal = format!("{}\n", portable(&file.path));
                let minimal_cost = tokens::estimate(&minimal);
                if used + minimal_cost <= options.token_budget {
                    out.push_str(&minimal);
                    used += minimal_cost;
                    continue;
                }
                break;
            }
            out.push_str(&block);
            used += cost;
        }
        out
    }

    /// Render with the default options.
    pub fn render_default(&self) -> String {
        self.render(&RepoMapOptions::default())
    }
}

/// A path as the repo map should print it: separated by `/` on every platform.
///
/// The map is a document, not a filesystem operation. It goes into a prompt, a
/// cache key and a diff, and a rendering that says `src\service.rs` on Windows
/// and `src/service.rs` everywhere else makes all three differ by operating
/// system for no reason anyone reading it would want. Forward slashes are also
/// what every other tool that prints repository paths uses.
///
/// The `PathBuf` itself keeps native separators — it is a real path and gets
/// used as one. Only the printing is normalised.
pub(crate) fn portable(path: &std::path::Path) -> String {
    path.components().map(|c| c.as_os_str().to_string_lossy()).collect::<Vec<_>>().join("/")
}

/// Render one file's block.
///
/// The `⋮` elision marker is the convention aider established and models handle
/// it well: it signals "code omitted here" without spending tokens explaining
/// that.
fn render_file(file: &RankedFile) -> String {
    let mut out = String::with_capacity(64 + file.symbols.len() * 48);
    out.push_str(&portable(&file.path));
    out.push_str(":\n");

    let mut last_line: Option<usize> = None;
    for symbol in &file.symbols {
        // A gap between symbols means code was skipped.
        if last_line.is_some_and(|last| symbol.line > last + 1) {
            out.push_str("⋮\n");
        }
        out.push('│');
        out.push_str(&format!("{} {}\n", symbol.kind.name(), symbol.name));
        last_line = Some(symbol.line);
    }
    out.push('\n');
    out
}

/// Ordering for which symbol kinds are most worth showing.
fn kind_priority(kind: SymbolKind) -> u8 {
    match kind {
        SymbolKind::Class | SymbolKind::Interface | SymbolKind::Enum => 0,
        SymbolKind::Function | SymbolKind::Method => 1,
        SymbolKind::TypeAlias | SymbolKind::Module => 2,
        SymbolKind::Constant | SymbolKind::Macro => 3,
        SymbolKind::Field | SymbolKind::Variable => 4,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn fixture() -> TempDir {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("src")).unwrap();

        fs::write(
            root.join("src/core.rs"),
            "pub struct Engine {}\npub fn start_engine() -> Engine { Engine {} }\npub fn stop_engine(e: Engine) {}\n",
        )
        .unwrap();
        fs::write(
            root.join("src/service.rs"),
            "pub fn run() { start_engine(); }\npub fn restart() { start_engine(); stop_engine(); }\n",
        )
        .unwrap();
        fs::write(root.join("src/cli.rs"), "pub fn main_entry() { run(); }\n").unwrap();
        fs::write(root.join("src/lonely.rs"), "pub fn unused_helper() {}\n").unwrap();
        dir
    }

    fn build(options: &RepoMapOptions) -> RepoMap {
        let dir = fixture();
        let project = nebula_vfs::Project::open(dir.path()).unwrap();
        RepoMap::build(&project, options).unwrap()
    }

    #[test]
    fn files_are_ordered_by_rank() {
        let map = build(&RepoMapOptions::default());
        let scores: Vec<f64> = map.files.iter().map(|f| f.score).collect();
        for pair in scores.windows(2) {
            assert!(pair[0] >= pair[1], "map must be ordered by descending rank: {scores:?}");
        }
    }

    #[test]
    fn the_most_depended_on_file_comes_first() {
        let map = build(&RepoMapOptions::default());
        assert_eq!(
            map.files[0].path,
            PathBuf::from("src/core.rs"),
            "got order {:?}",
            map.files.iter().map(|f| &f.path).collect::<Vec<_>>()
        );
    }

    #[test]
    fn focus_files_bias_the_ranking() {
        let unfocused = build(&RepoMapOptions::default());
        let focused = build(&RepoMapOptions::default().focus("src/lonely.rs"));

        let rank_of = |map: &RepoMap, path: &str| {
            map.files.iter().position(|f| f.path == Path::new(path)).unwrap()
        };
        assert!(
            rank_of(&focused, "src/lonely.rs") < rank_of(&unfocused, "src/lonely.rs"),
            "focusing a file must raise it up the map"
        );
    }

    #[test]
    fn excluded_files_are_omitted() {
        let map = build(&RepoMapOptions::default().exclude("src/core.rs"));
        assert!(
            !map.files.iter().any(|f| f.path == Path::new("src/core.rs")),
            "an excluded file leaked into the map"
        );
    }

    #[test]
    fn rendering_stays_within_the_token_budget() {
        for budget in [10usize, 50, 200, 1000] {
            let options = RepoMapOptions::default().budget(budget);
            let map = build(&options);
            let rendered = map.render(&options);
            let estimated = tokens::estimate(&rendered);
            assert!(
                estimated <= budget,
                "budget {budget} exceeded: rendered {estimated} tokens\n{rendered}"
            );
        }
    }

    #[test]
    fn a_tighter_budget_produces_a_shorter_map() {
        let map = build(&RepoMapOptions::default());
        let wide = map.render(&RepoMapOptions::default().budget(2000));
        let narrow = map.render(&RepoMapOptions::default().budget(60));
        assert!(narrow.len() < wide.len());
        assert!(!narrow.is_empty(), "even a tight budget should show something");
    }

    #[test]
    fn the_highest_ranked_file_survives_a_tight_budget() {
        let map = build(&RepoMapOptions::default());
        let narrow = map.render(&RepoMapOptions::default().budget(40));
        assert!(
            narrow.contains("src/core.rs"),
            "the most important file must be the last thing dropped:\n{narrow}"
        );
    }

    #[test]
    fn rendered_output_names_files_and_symbols() {
        let map = build(&RepoMapOptions::default());
        let rendered = map.render(&RepoMapOptions::default().budget(4000));
        assert!(rendered.contains("src/core.rs"));
        assert!(rendered.contains("start_engine"), "{rendered}");
        assert!(rendered.contains("Engine"), "{rendered}");
    }

    #[test]
    fn files_without_definitions_are_left_out() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("empty.rs"), "// only a comment\n").unwrap();
        fs::write(dir.path().join("real.rs"), "pub fn thing() {}\n").unwrap();

        let project = nebula_vfs::Project::open(dir.path()).unwrap();
        let map = RepoMap::build(&project, &RepoMapOptions::default()).unwrap();
        assert_eq!(map.len(), 1);
        assert_eq!(map.files[0].path, PathBuf::from("real.rs"));
    }

    #[test]
    fn symbols_per_file_are_capped() {
        let dir = TempDir::new().unwrap();
        let source: String = (0..50).map(|i| format!("pub fn f{i}() {{}}\n")).collect();
        fs::write(dir.path().join("many.rs"), source).unwrap();

        let project = nebula_vfs::Project::open(dir.path()).unwrap();
        let options = RepoMapOptions { max_symbols_per_file: 5, ..Default::default() };
        let map = RepoMap::build(&project, &options).unwrap();
        assert_eq!(map.files[0].symbols.len(), 5);
    }

    #[test]
    fn types_are_listed_before_functions() {
        let dir = TempDir::new().unwrap();
        fs::write(
            dir.path().join("mixed.rs"),
            "pub fn helper() {}\npub struct Important {}\npub fn another() {}\n",
        )
        .unwrap();
        let project = nebula_vfs::Project::open(dir.path()).unwrap();
        let map = RepoMap::build(&project, &RepoMapOptions::default()).unwrap();

        assert_eq!(map.files[0].symbols[0].name, "Important", "types lead the summary");
    }

    #[test]
    fn an_empty_project_renders_an_empty_map() {
        let dir = TempDir::new().unwrap();
        let project = nebula_vfs::Project::open(dir.path()).unwrap();
        let map = RepoMap::build(&project, &RepoMapOptions::default()).unwrap();
        assert!(map.is_empty());
        assert!(map.render_default().is_empty());
    }

    #[test]
    fn a_zero_budget_renders_nothing_rather_than_panicking() {
        let map = build(&RepoMapOptions::default());
        assert!(map.render(&RepoMapOptions::default().budget(0)).is_empty());
    }

    #[test]
    fn maps_are_reproducible() {
        let dir = fixture();
        let project = nebula_vfs::Project::open(dir.path()).unwrap();
        let options = RepoMapOptions::default();
        let first = RepoMap::build(&project, &options).unwrap();
        let second = RepoMap::build(&project, &options).unwrap();
        assert_eq!(first.render(&options), second.render(&options));
    }

    #[test]
    fn reranking_a_cached_graph_matches_a_fresh_build() {
        let dir = fixture();
        let project = nebula_vfs::Project::open(dir.path()).unwrap();
        let graph = SymbolGraph::build(&project).unwrap();

        let options = RepoMapOptions::default().focus("src/service.rs");
        let from_cache = RepoMap::from_graph(&graph, &options);
        let fresh = RepoMap::build(&project, &options).unwrap();
        assert_eq!(from_cache.render(&options), fresh.render(&options));
    }
}
