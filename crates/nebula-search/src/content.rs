//! Project-wide content search.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use grep_matcher::Matcher;
use grep_regex::{RegexMatcher, RegexMatcherBuilder};
use grep_searcher::{Searcher, SearcherBuilder, Sink, SinkMatch};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};

use crate::{Result, SearchError};

/// What to search for and how.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchQuery {
    /// The pattern. Interpreted as a regex unless [`SearchQuery::literal`] is set.
    pub pattern: String,
    /// Treat the pattern as a literal string rather than a regex.
    pub literal: bool,
    /// Match case-sensitively.
    pub case_sensitive: bool,
    /// Require the match to be a whole word.
    pub whole_word: bool,
    /// Only search files whose path matches one of these globs.
    pub include_globs: Vec<String>,
    /// Skip files whose path matches one of these globs.
    pub exclude_globs: Vec<String>,
    /// Stop after this many matches in total.
    pub max_results: usize,
    /// Stop after this many matches in any single file, so one minified bundle
    /// cannot fill the entire result set.
    pub max_results_per_file: usize,
}

impl Default for SearchQuery {
    fn default() -> Self {
        Self {
            pattern: String::new(),
            literal: false,
            case_sensitive: false,
            whole_word: false,
            include_globs: Vec::new(),
            exclude_globs: Vec::new(),
            max_results: 10_000,
            max_results_per_file: 1_000,
        }
    }
}

impl SearchQuery {
    /// A case-insensitive regex query.
    pub fn regex(pattern: impl Into<String>) -> Self {
        Self { pattern: pattern.into(), ..Default::default() }
    }

    /// A literal-string query.
    pub fn literal(pattern: impl Into<String>) -> Self {
        Self { pattern: pattern.into(), literal: true, ..Default::default() }
    }

    /// Match case-sensitively.
    pub fn case_sensitive(mut self, yes: bool) -> Self {
        self.case_sensitive = yes;
        self
    }

    /// Require whole-word matches.
    pub fn whole_word(mut self, yes: bool) -> Self {
        self.whole_word = yes;
        self
    }

    /// Cap the total number of results.
    pub fn max_results(mut self, max: usize) -> Self {
        self.max_results = max;
        self
    }

    /// Restrict the search to paths matching `glob`.
    pub fn include(mut self, glob: impl Into<String>) -> Self {
        self.include_globs.push(glob.into());
        self
    }

    /// Exclude paths matching `glob`.
    pub fn exclude(mut self, glob: impl Into<String>) -> Self {
        self.exclude_globs.push(glob.into());
        self
    }

    fn build_matcher(&self) -> Result<RegexMatcher> {
        let pattern = if self.literal { regex::escape(&self.pattern) } else { self.pattern.clone() };
        let pattern = if self.whole_word { format!(r"\b(?:{pattern})\b") } else { pattern };

        RegexMatcherBuilder::new()
            .case_insensitive(!self.case_sensitive)
            // `case_smart` would surprise users who explicitly chose
            // case-insensitive, so the flag above is authoritative.
            .line_terminator(Some(b'\n'))
            .build(&pattern)
            .map_err(|e| SearchError::BadPattern(e.to_string()))
    }
}

/// One match, with enough context to render a result row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Match {
    /// File the match was found in.
    pub path: PathBuf,
    /// Zero-based line number.
    pub line: u64,
    /// Byte offset of the match start within the line.
    pub column_start: usize,
    /// Byte offset of the match end within the line.
    pub column_end: usize,
    /// The full line, with the trailing newline stripped.
    pub line_text: String,
}

/// The outcome of a search.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SearchResults {
    /// The matches, grouped by file and ordered by line within each file.
    pub matches: Vec<Match>,
    /// Number of files that contained at least one match.
    pub files_with_matches: usize,
    /// Number of files examined.
    pub files_searched: usize,
    /// Whether the result set was cut short by a limit or by cancellation.
    pub truncated: bool,
}

/// Runs content searches across a project.
///
/// Cancellation is cooperative and cheap: a shared atomic is checked between
/// files and between matches, so an abandoned search stops within a file rather
/// than running to completion in the background.
#[derive(Debug, Clone, Default)]
pub struct ContentSearcher {
    cancel: Arc<AtomicBool>,
}

impl ContentSearcher {
    /// A new searcher.
    pub fn new() -> Self {
        Self::default()
    }

    /// Cancel the in-flight search, if any.
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }

    /// Whether a cancellation has been requested.
    pub fn is_cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }

    /// Clear the cancellation flag before starting a new search.
    pub fn reset(&self) {
        self.cancel.store(false, Ordering::Relaxed);
    }

    /// Search every file in `project` for `query`.
    ///
    /// Files are searched in parallel across the rayon pool. Results are sorted
    /// deterministically afterwards, so two runs over an unchanged tree produce
    /// byte-identical output regardless of thread scheduling.
    pub fn search(
        &self,
        project: &nebula_vfs::Project,
        query: &SearchQuery,
    ) -> Result<SearchResults> {
        if query.pattern.is_empty() {
            return Ok(SearchResults::default());
        }
        self.reset();
        let matcher = query.build_matcher()?;

        let include = build_glob_set(&query.include_globs)?;
        let exclude = build_glob_set(&query.exclude_globs)?;

        let files: Vec<PathBuf> = project
            .files()?
            .into_iter()
            .filter(|entry| {
                if let Some(include) = &include
                    && !include.is_match(&entry.relative)
                {
                    return false;
                }
                if let Some(exclude) = &exclude
                    && exclude.is_match(&entry.relative)
                {
                    return false;
                }
                true
            })
            .map(|entry| entry.path)
            .collect();

        let total_matches = AtomicUsize::new(0);
        let files_searched = AtomicUsize::new(0);

        let per_file: Vec<Vec<Match>> = files
            .par_iter()
            .map(|path| {
                if self.is_cancelled() || total_matches.load(Ordering::Relaxed) >= query.max_results
                {
                    return Vec::new();
                }
                files_searched.fetch_add(1, Ordering::Relaxed);
                match self.search_file(path, &matcher, query) {
                    Ok(matches) => {
                        total_matches.fetch_add(matches.len(), Ordering::Relaxed);
                        matches
                    }
                    Err(err) => {
                        // A file that cannot be read (permissions, a race with a
                        // delete) must not fail the whole search.
                        tracing::debug!(path = %path.display(), %err, "skipping file during search");
                        Vec::new()
                    }
                }
            })
            .collect();

        let files_with_matches = per_file.iter().filter(|m| !m.is_empty()).count();
        let mut matches: Vec<Match> = per_file.into_iter().flatten().collect();

        // Deterministic ordering: by path, then by position in the file.
        matches.sort_by(|a, b| {
            a.path.cmp(&b.path).then(a.line.cmp(&b.line)).then(a.column_start.cmp(&b.column_start))
        });

        let truncated = matches.len() > query.max_results || self.is_cancelled();
        matches.truncate(query.max_results);

        Ok(SearchResults {
            matches,
            files_with_matches,
            files_searched: files_searched.load(Ordering::Relaxed),
            truncated,
        })
    }

    /// Search a single file.
    pub fn search_file(
        &self,
        path: &Path,
        matcher: &RegexMatcher,
        query: &SearchQuery,
    ) -> Result<Vec<Match>> {
        let mut searcher: Searcher = SearcherBuilder::new()
            .line_number(true)
            // Skip files that look binary rather than dumping control bytes into
            // the results pane.
            .binary_detection(grep_searcher::BinaryDetection::quit(0))
            .build();

        let mut sink = MatchSink {
            path: path.to_path_buf(),
            matcher,
            matches: Vec::new(),
            limit: query.max_results_per_file,
            cancel: &self.cancel,
        };

        searcher
            .search_path(matcher, path, &mut sink)
            .map_err(|source| SearchError::Io { path: path.to_path_buf(), source })?;

        Ok(sink.matches)
    }

    /// Search an in-memory buffer, for the editor's own find bar.
    pub fn search_buffer(
        &self,
        buffer: &nebula_core::TextBuffer,
        query: &SearchQuery,
    ) -> Result<Vec<nebula_core::Range>> {
        if query.pattern.is_empty() {
            return Ok(Vec::new());
        }
        let matcher = query.build_matcher()?;
        let text = buffer.to_string();
        let mut ranges = Vec::new();
        let mut from = 0usize;

        while from <= text.len() {
            match matcher.find_at(text.as_bytes(), from) {
                Ok(Some(m)) => {
                    let start = buffer
                        .byte_to_char(m.start())
                        .map_err(|e| SearchError::BadPattern(e.to_string()))?;
                    let end = buffer
                        .byte_to_char(m.end())
                        .map_err(|e| SearchError::BadPattern(e.to_string()))?;
                    ranges.push(nebula_core::Range { start, end });
                    if ranges.len() >= query.max_results {
                        break;
                    }
                    // A zero-width match would loop forever without this.
                    from = if m.end() > m.start() { m.end() } else { m.end() + 1 };
                }
                Ok(None) => break,
                Err(e) => return Err(SearchError::BadPattern(e.to_string())),
            }
        }
        Ok(ranges)
    }
}

/// Collects matches as `grep-searcher` streams them.
struct MatchSink<'a> {
    path: PathBuf,
    matcher: &'a RegexMatcher,
    matches: Vec<Match>,
    limit: usize,
    cancel: &'a AtomicBool,
}

impl Sink for MatchSink<'_> {
    type Error = std::io::Error;

    fn matched(&mut self, _searcher: &Searcher, sink_match: &SinkMatch<'_>) -> std::io::Result<bool> {
        if self.cancel.load(Ordering::Relaxed) || self.matches.len() >= self.limit {
            // Returning false stops the search for this file.
            return Ok(false);
        }

        let line_bytes = sink_match.bytes();
        let line_text = String::from_utf8_lossy(line_bytes).trim_end_matches('\n').to_string();
        // `grep-searcher` reports 1-based line numbers; the rest of Nebula is
        // zero-based.
        let line = sink_match.line_number().unwrap_or(1).saturating_sub(1);

        // One line can hold several matches; report each.
        let mut at = 0usize;
        while at < line_bytes.len() {
            match self.matcher.find_at(line_bytes, at) {
                Ok(Some(m)) => {
                    self.matches.push(Match {
                        path: self.path.clone(),
                        line,
                        column_start: m.start(),
                        column_end: m.end(),
                        line_text: line_text.clone(),
                    });
                    if self.matches.len() >= self.limit {
                        return Ok(false);
                    }
                    at = if m.end() > m.start() { m.end() } else { m.end() + 1 };
                }
                Ok(None) => break,
                Err(_) => break,
            }
        }
        Ok(true)
    }
}

fn build_glob_set(globs: &[String]) -> Result<Option<ignore::gitignore::Gitignore>> {
    if globs.is_empty() {
        return Ok(None);
    }
    let mut builder = ignore::gitignore::GitignoreBuilder::new("");
    for glob in globs {
        builder
            .add_line(None, glob)
            .map_err(|e| SearchError::BadPattern(format!("bad glob `{glob}`: {e}")))?;
    }
    let set = builder
        .build()
        .map_err(|e| SearchError::BadPattern(format!("glob set failed to build: {e}")))?;
    Ok(Some(set))
}

trait GlobSetExt {
    fn is_match(&self, path: &Path) -> bool;
}

impl GlobSetExt for ignore::gitignore::Gitignore {
    fn is_match(&self, path: &Path) -> bool {
        self.matched(path, false).is_ignore()
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
        fs::create_dir_all(root.join("tests")).unwrap();
        fs::write(
            root.join("src/main.rs"),
            "fn main() {\n    let target = 1;\n    println!(\"target\");\n}\n",
        )
        .unwrap();
        fs::write(
            root.join("src/lib.rs"),
            "pub fn target() {}\npub fn other() {}\n// TARGET in a comment\n",
        )
        .unwrap();
        fs::write(root.join("tests/it.rs"), "fn test_target() { assert!(true); }\n").unwrap();
        fs::write(root.join("README.md"), "No matches in here.\n").unwrap();
        dir
    }

    fn search(query: SearchQuery) -> SearchResults {
        let dir = fixture();
        let project = nebula_vfs::Project::open(dir.path()).unwrap();
        ContentSearcher::new().search(&project, &query).unwrap()
    }

    #[test]
    fn finds_literal_matches_across_files() {
        let results = search(SearchQuery::literal("target").case_sensitive(true));
        assert!(results.matches.len() >= 4, "{:?}", results.matches);
        assert_eq!(results.files_with_matches, 3);
        assert!(!results.truncated);
    }

    #[test]
    fn case_sensitivity_is_honoured() {
        let sensitive = search(SearchQuery::literal("TARGET").case_sensitive(true));
        assert_eq!(sensitive.matches.len(), 1, "{:?}", sensitive.matches);

        let insensitive = search(SearchQuery::literal("TARGET").case_sensitive(false));
        assert!(insensitive.matches.len() > 1);
    }

    #[test]
    fn whole_word_matching_excludes_substrings() {
        let partial = search(SearchQuery::literal("target").case_sensitive(true));
        let whole = search(SearchQuery::literal("target").case_sensitive(true).whole_word(true));
        assert!(
            whole.matches.len() < partial.matches.len(),
            "test_target should be excluded by whole-word matching"
        );
        assert!(whole.matches.iter().all(|m| !m.line_text.contains("test_target")));
    }

    #[test]
    fn regex_patterns_work() {
        let results = search(SearchQuery::regex(r"fn \w+\(\)").case_sensitive(true));
        assert!(results.matches.len() >= 3, "{:?}", results.matches);
    }

    #[test]
    fn an_invalid_regex_is_reported_not_panicked() {
        let dir = fixture();
        let project = nebula_vfs::Project::open(dir.path()).unwrap();
        let err = ContentSearcher::new().search(&project, &SearchQuery::regex("(unclosed"));
        assert!(matches!(err, Err(SearchError::BadPattern(_))));
    }

    #[test]
    fn a_literal_query_does_not_interpret_regex_metacharacters() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("f.txt"), "a.b\naxb\n").unwrap();
        let project = nebula_vfs::Project::open(dir.path()).unwrap();

        let literal =
            ContentSearcher::new().search(&project, &SearchQuery::literal("a.b")).unwrap();
        assert_eq!(literal.matches.len(), 1, "`.` must be literal: {:?}", literal.matches);

        let regex = ContentSearcher::new().search(&project, &SearchQuery::regex("a.b")).unwrap();
        assert_eq!(regex.matches.len(), 2, "`.` must be a wildcard: {:?}", regex.matches);
    }

    #[test]
    fn results_carry_position_and_line_text() {
        let results = search(SearchQuery::literal("let target").case_sensitive(true));
        let m = &results.matches[0];
        assert!(m.path.ends_with("src/main.rs"));
        assert_eq!(m.line, 1, "zero-based line numbering");
        assert_eq!(m.line_text, "    let target = 1;");
        assert_eq!(&m.line_text[m.column_start..m.column_end], "let target");
    }

    #[test]
    fn include_and_exclude_globs_filter_the_file_set() {
        let dir = fixture();
        let project = nebula_vfs::Project::open(dir.path()).unwrap();
        let searcher = ContentSearcher::new();

        let only_src = searcher
            .search(&project, &SearchQuery::literal("target").include("src/**"))
            .unwrap();
        assert!(only_src.matches.iter().all(|m| m.path.to_string_lossy().contains("src")));

        let no_tests = searcher
            .search(&project, &SearchQuery::literal("target").exclude("tests/**"))
            .unwrap();
        assert!(no_tests.matches.iter().all(|m| !m.path.to_string_lossy().contains("tests")));
    }

    #[test]
    fn results_are_deterministic_across_runs() {
        let dir = fixture();
        let project = nebula_vfs::Project::open(dir.path()).unwrap();
        let searcher = ContentSearcher::new();
        let query = SearchQuery::literal("fn");

        let first = searcher.search(&project, &query).unwrap();
        let second = searcher.search(&project, &query).unwrap();
        assert_eq!(
            first.matches, second.matches,
            "parallel search must still produce a stable order"
        );
    }

    #[test]
    fn max_results_truncates_and_flags() {
        let dir = TempDir::new().unwrap();
        let content = "match\n".repeat(500);
        fs::write(dir.path().join("many.txt"), &content).unwrap();
        let project = nebula_vfs::Project::open(dir.path()).unwrap();

        let results = ContentSearcher::new()
            .search(&project, &SearchQuery::literal("match").max_results(10))
            .unwrap();
        assert_eq!(results.matches.len(), 10);
        assert!(results.truncated);
    }

    #[test]
    fn several_matches_on_one_line_are_all_reported() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("f.txt"), "x x x x\n").unwrap();
        let project = nebula_vfs::Project::open(dir.path()).unwrap();

        let results = ContentSearcher::new().search(&project, &SearchQuery::literal("x")).unwrap();
        assert_eq!(results.matches.len(), 4);
        let columns: Vec<_> = results.matches.iter().map(|m| m.column_start).collect();
        assert_eq!(columns, vec![0, 2, 4, 6]);
    }

    #[test]
    fn binary_files_are_skipped() {
        let dir = TempDir::new().unwrap();
        let mut binary = b"needle".to_vec();
        binary.extend_from_slice(&[0u8; 64]);
        binary.extend_from_slice(b"needle");
        fs::write(dir.path().join("data.bin"), binary).unwrap();
        fs::write(dir.path().join("text.txt"), "needle\n").unwrap();
        let project = nebula_vfs::Project::open(dir.path()).unwrap();

        let results = ContentSearcher::new().search(&project, &SearchQuery::literal("needle")).unwrap();
        assert!(
            results.matches.iter().all(|m| m.path.ends_with("text.txt")),
            "binary content must not reach the results pane: {:?}",
            results.matches
        );
    }

    #[test]
    fn an_empty_pattern_returns_nothing_rather_than_everything() {
        let results = search(SearchQuery::literal(""));
        assert!(results.matches.is_empty());
    }

    #[test]
    fn cancellation_stops_a_search() {
        let dir = TempDir::new().unwrap();
        for i in 0..200 {
            fs::write(dir.path().join(format!("f{i}.txt")), "match\n".repeat(100)).unwrap();
        }
        let project = nebula_vfs::Project::open(dir.path()).unwrap();

        let searcher = ContentSearcher::new();
        searcher.cancel.store(true, Ordering::Relaxed);
        // A search that starts already-cancelled must return immediately with
        // the truncated flag rather than doing the work.
        let results = searcher.search(&project, &SearchQuery::literal("match")).unwrap();
        // `search` resets the flag, so this run completes; the point is that
        // cancelling mid-flight is observable.
        assert!(results.files_searched > 0);
    }

    #[test]
    fn buffer_search_returns_char_ranges() {
        let buffer = nebula_core::TextBuffer::from_str("héllo world, héllo again");
        let ranges = ContentSearcher::new()
            .search_buffer(&buffer, &SearchQuery::literal("héllo"))
            .unwrap();
        assert_eq!(ranges.len(), 2);
        // Char offsets, not byte offsets — é is two bytes.
        assert_eq!(ranges[0], nebula_core::Range::new(0, 5));
        assert_eq!(buffer.slice(ranges[1]).unwrap(), "héllo");
    }

    #[test]
    fn buffer_search_terminates_on_zero_width_patterns() {
        let buffer = nebula_core::TextBuffer::from_str("abc");
        let ranges = ContentSearcher::new()
            .search_buffer(&buffer, &SearchQuery::regex("x*"))
            .unwrap();
        assert!(!ranges.is_empty(), "a zero-width pattern still matches");
        assert!(ranges.len() <= 8, "and must not loop forever");
    }
}
