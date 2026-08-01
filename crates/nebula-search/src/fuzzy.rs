//! Fuzzy matching for the file and symbol pickers.
//!
//! Backed by `nucleo-matcher`, whose scoring is tuned for exactly this problem:
//! matches at word boundaries and after path separators score higher, so typing
//! `mnrs` ranks `src/main.rs` above `dominators.rs`.

use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher, Utf32Str};
use serde::{Deserialize, Serialize};

/// A scored fuzzy match.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FuzzyResult<T> {
    /// The matched item.
    pub item: T,
    /// Match score; higher is better. Only comparable within one query.
    pub score: u32,
    /// Char indices in the candidate string that the query matched, for
    /// highlighting the matched characters in the picker.
    pub indices: Vec<u32>,
}

/// A reusable fuzzy matcher.
///
/// `nucleo`'s `Matcher` holds sizeable scratch buffers, so it is constructed
/// once and reused across keystrokes rather than per query.
pub struct FuzzyMatcher {
    matcher: Matcher,
}

impl std::fmt::Debug for FuzzyMatcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("FuzzyMatcher")
    }
}

impl Default for FuzzyMatcher {
    fn default() -> Self {
        Self::new()
    }
}

impl FuzzyMatcher {
    /// A matcher tuned for general strings.
    pub fn new() -> Self {
        Self { matcher: Matcher::new(Config::DEFAULT) }
    }

    /// A matcher tuned for file paths.
    ///
    /// Path mode gives extra weight to characters after `/`, which is what makes
    /// `src/main.rs` beat `dominators.rs` for the query `mnrs`.
    pub fn for_paths() -> Self {
        Self { matcher: Matcher::new(Config::DEFAULT.match_paths()) }
    }

    /// Score `candidates` against `query`, returning matches best-first.
    ///
    /// An empty query matches everything with score 0, preserving input order —
    /// which is what a picker should show before the user types anything.
    pub fn match_list<T: Clone>(
        &mut self,
        query: &str,
        candidates: impl IntoIterator<Item = (String, T)>,
    ) -> Vec<FuzzyResult<T>> {
        let candidates: Vec<(String, T)> = candidates.into_iter().collect();

        if query.is_empty() {
            return candidates
                .into_iter()
                .map(|(_, item)| FuzzyResult { item, score: 0, indices: Vec::new() })
                .collect();
        }

        // `Smart` case matching is case-insensitive until the query contains an
        // uppercase character, which is the behaviour every picker converged on.
        let pattern = Pattern::parse(query, CaseMatching::Smart, Normalization::Smart);

        let mut buf = Vec::new();
        let mut indices = Vec::new();
        let mut results: Vec<FuzzyResult<T>> = Vec::new();

        for (text, item) in candidates {
            buf.clear();
            indices.clear();
            let haystack = Utf32Str::new(&text, &mut buf);
            if let Some(score) = pattern.indices(haystack, &mut self.matcher, &mut indices) {
                indices.sort_unstable();
                indices.dedup();
                results.push(FuzzyResult { item, score, indices: indices.clone() });
            }
        }

        // Sort by score descending. The sort is stable, so equal scores keep
        // the caller's input order and the picker does not jitter.
        results.sort_by_key(|result| std::cmp::Reverse(result.score));
        results
    }

    /// Score `candidates` and keep only the best `limit`.
    pub fn match_top<T: Clone>(
        &mut self,
        query: &str,
        candidates: impl IntoIterator<Item = (String, T)>,
        limit: usize,
    ) -> Vec<FuzzyResult<T>> {
        let mut results = self.match_list(query, candidates);
        results.truncate(limit);
        results
    }

    /// Score a single candidate, returning `None` if it does not match.
    pub fn score_one(&mut self, query: &str, candidate: &str) -> Option<u32> {
        if query.is_empty() {
            return Some(0);
        }
        let pattern = Pattern::parse(query, CaseMatching::Smart, Normalization::Smart);
        let mut buf = Vec::new();
        pattern.score(Utf32Str::new(candidate, &mut buf), &mut self.matcher)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths() -> Vec<(String, usize)> {
        [
            "src/main.rs",
            "src/lib.rs",
            "src/dominators.rs",
            "crates/nebula-core/src/text.rs",
            "tests/integration_test.rs",
            "README.md",
        ]
        .iter()
        .enumerate()
        .map(|(i, p)| (p.to_string(), i))
        .collect()
    }

    fn matched_names(results: &[FuzzyResult<usize>]) -> Vec<String> {
        let all = paths();
        results.iter().map(|r| all[r.item].0.clone()).collect()
    }

    #[test]
    fn subsequence_queries_match() {
        let mut matcher = FuzzyMatcher::for_paths();
        let results = matcher.match_list("mnrs", paths());
        let names = matched_names(&results);
        assert!(names.contains(&"src/main.rs".to_string()), "{names:?}");
    }

    #[test]
    fn path_mode_ranks_filename_matches_first() {
        let mut matcher = FuzzyMatcher::for_paths();
        let results = matcher.match_list("main", paths());
        let names = matched_names(&results);
        assert_eq!(names.first().map(String::as_str), Some("src/main.rs"), "{names:?}");
    }

    #[test]
    fn results_are_ordered_best_first() {
        let mut matcher = FuzzyMatcher::for_paths();
        let results = matcher.match_list("rs", paths());
        for pair in results.windows(2) {
            assert!(pair[0].score >= pair[1].score, "scores must descend");
        }
    }

    #[test]
    fn non_matching_candidates_are_dropped() {
        let mut matcher = FuzzyMatcher::for_paths();
        let results = matcher.match_list("zzzzz", paths());
        assert!(results.is_empty());
    }

    #[test]
    fn an_empty_query_keeps_everything_in_input_order() {
        let mut matcher = FuzzyMatcher::for_paths();
        let results = matcher.match_list("", paths());
        assert_eq!(results.len(), paths().len());
        let order: Vec<usize> = results.iter().map(|r| r.item).collect();
        assert_eq!(order, (0..paths().len()).collect::<Vec<_>>());
    }

    #[test]
    fn smart_case_is_insensitive_until_the_query_has_uppercase() {
        let mut matcher = FuzzyMatcher::new();
        let candidates = vec![("README.md".to_string(), 0), ("readme.txt".to_string(), 1)];

        let lower = matcher.match_list("readme", candidates.clone());
        assert_eq!(lower.len(), 2, "a lowercase query matches both cases");

        let upper = matcher.match_list("README", candidates);
        assert_eq!(upper.len(), 1, "an uppercase query becomes case-sensitive");
        assert_eq!(upper[0].item, 0);
    }

    #[test]
    fn indices_point_at_the_matched_characters() {
        let mut matcher = FuzzyMatcher::new();
        let results = matcher.match_list("mn", vec![("main".to_string(), 0)]);
        assert_eq!(results.len(), 1);
        let indices = &results[0].indices;
        assert_eq!(indices.len(), 2);

        let chars: Vec<char> = "main".chars().collect();
        let matched: String = indices.iter().map(|i| chars[*i as usize]).collect();
        assert_eq!(matched, "mn", "indices must line up with the candidate's chars");
    }

    #[test]
    fn indices_are_sorted_and_unique() {
        let mut matcher = FuzzyMatcher::for_paths();
        let results = matcher.match_list("srcmain", paths());
        for result in &results {
            let mut sorted = result.indices.clone();
            sorted.sort_unstable();
            sorted.dedup();
            assert_eq!(result.indices, sorted, "the renderer relies on sorted unique indices");
        }
    }

    #[test]
    fn limit_keeps_only_the_best() {
        let mut matcher = FuzzyMatcher::for_paths();
        let all = matcher.match_list("rs", paths());
        let top = matcher.match_top("rs", paths(), 2);
        assert_eq!(top.len(), 2);
        assert_eq!(top[0].score, all[0].score);
    }

    #[test]
    fn scoring_a_single_candidate_agrees_with_the_list_path() {
        let mut matcher = FuzzyMatcher::new();
        let single = matcher.score_one("main", "src/main.rs").unwrap();
        let listed = matcher.match_list("main", vec![("src/main.rs".to_string(), 0)]);
        assert_eq!(single, listed[0].score);
        assert_eq!(matcher.score_one("zzz", "src/main.rs"), None);
    }

    #[test]
    fn unicode_candidates_do_not_break_index_mapping() {
        let mut matcher = FuzzyMatcher::new();
        let results = matcher.match_list("wörld", vec![("héllo wörld".to_string(), 0)]);
        assert_eq!(results.len(), 1);
        let chars: Vec<char> = "héllo wörld".chars().collect();
        for index in &results[0].indices {
            assert!((*index as usize) < chars.len(), "index escaped the candidate");
        }
    }
}
