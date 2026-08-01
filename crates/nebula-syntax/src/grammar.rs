//! The grammar registry.
//!
//! Grammars are compiled C, so loading one is cheap but compiling its queries
//! is not — a highlights query is a few hundred patterns that tree-sitter must
//! parse and index. Queries are therefore compiled **once per language, lazily**,
//! and shared behind an `Arc` by every document using that language.

use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::RwLock;
use tree_sitter::{Language, Query};

use crate::{Result, SyntaxError};

/// A loaded grammar with its compiled queries.
pub struct Grammar {
    /// Language identifier (`rust`, `python`, ...).
    pub language_id: String,
    /// The tree-sitter language.
    pub language: Language,
    /// Compiled highlights query, if the grammar ships one.
    pub highlights: Option<Query>,
    /// Compiled tags query, used for symbol extraction.
    pub tags: Option<Query>,
    /// Compiled injections query, for embedded languages.
    pub injections: Option<Query>,
    /// Bracket pairs, used by structural navigation and auto-indent.
    pub block_delimiters: &'static [(&'static str, &'static str)],
    /// Line comment prefix, if the language has one.
    pub line_comment: Option<&'static str>,
}

impl std::fmt::Debug for Grammar {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Grammar")
            .field("language_id", &self.language_id)
            .field("has_highlights", &self.highlights.is_some())
            .field("has_tags", &self.tags.is_some())
            .field("has_injections", &self.injections.is_some())
            .finish()
    }
}

/// The static definition of a language, before its queries are compiled.
struct GrammarSpec {
    language_id: &'static str,
    language: fn() -> Language,
    highlights: Option<&'static str>,
    tags: Option<&'static str>,
    injections: Option<&'static str>,
    block_delimiters: &'static [(&'static str, &'static str)],
    line_comment: Option<&'static str>,
}

const CURLY_BLOCKS: &[(&str, &str)] = &[("{", "}"), ("(", ")"), ("[", "]")];
const BRACKET_ONLY: &[(&str, &str)] = &[("(", ")"), ("[", "]"), ("{", "}")];

/// Every grammar compiled into the binary.
fn specs() -> Vec<GrammarSpec> {
    vec![
        GrammarSpec {
            language_id: "rust",
            language: || tree_sitter_rust::LANGUAGE.into(),
            highlights: Some(tree_sitter_rust::HIGHLIGHTS_QUERY),
            tags: Some(tree_sitter_rust::TAGS_QUERY),
            injections: Some(tree_sitter_rust::INJECTIONS_QUERY),
            block_delimiters: CURLY_BLOCKS,
            line_comment: Some("//"),
        },
        GrammarSpec {
            language_id: "python",
            language: || tree_sitter_python::LANGUAGE.into(),
            highlights: Some(tree_sitter_python::HIGHLIGHTS_QUERY),
            tags: Some(tree_sitter_python::TAGS_QUERY),
            injections: None,
            block_delimiters: BRACKET_ONLY,
            line_comment: Some("#"),
        },
        GrammarSpec {
            language_id: "javascript",
            language: || tree_sitter_javascript::LANGUAGE.into(),
            highlights: Some(tree_sitter_javascript::HIGHLIGHT_QUERY),
            tags: Some(tree_sitter_javascript::TAGS_QUERY),
            injections: Some(tree_sitter_javascript::INJECTIONS_QUERY),
            block_delimiters: CURLY_BLOCKS,
            line_comment: Some("//"),
        },
        GrammarSpec {
            language_id: "typescript",
            language: || tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            highlights: Some(tree_sitter_typescript::HIGHLIGHTS_QUERY),
            tags: Some(tree_sitter_typescript::TAGS_QUERY),
            injections: None,
            block_delimiters: CURLY_BLOCKS,
            line_comment: Some("//"),
        },
        GrammarSpec {
            language_id: "typescriptreact",
            language: || tree_sitter_typescript::LANGUAGE_TSX.into(),
            highlights: Some(tree_sitter_typescript::HIGHLIGHTS_QUERY),
            tags: Some(tree_sitter_typescript::TAGS_QUERY),
            injections: None,
            block_delimiters: CURLY_BLOCKS,
            line_comment: Some("//"),
        },
        GrammarSpec {
            language_id: "go",
            language: || tree_sitter_go::LANGUAGE.into(),
            highlights: Some(tree_sitter_go::HIGHLIGHTS_QUERY),
            tags: Some(tree_sitter_go::TAGS_QUERY),
            injections: None,
            block_delimiters: CURLY_BLOCKS,
            line_comment: Some("//"),
        },
        GrammarSpec {
            language_id: "c",
            language: || tree_sitter_c::LANGUAGE.into(),
            highlights: Some(tree_sitter_c::HIGHLIGHT_QUERY),
            tags: Some(tree_sitter_c::TAGS_QUERY),
            injections: None,
            block_delimiters: CURLY_BLOCKS,
            line_comment: Some("//"),
        },
        GrammarSpec {
            language_id: "json",
            language: || tree_sitter_json::LANGUAGE.into(),
            highlights: Some(tree_sitter_json::HIGHLIGHTS_QUERY),
            tags: None,
            injections: None,
            block_delimiters: BRACKET_ONLY,
            line_comment: None,
        },
        GrammarSpec {
            language_id: "toml",
            language: || tree_sitter_toml_ng::LANGUAGE.into(),
            highlights: Some(tree_sitter_toml_ng::HIGHLIGHTS_QUERY),
            tags: None,
            injections: None,
            block_delimiters: BRACKET_ONLY,
            line_comment: Some("#"),
        },
    ]
}

/// Lazily-compiled, shared grammars.
#[derive(Default)]
pub struct GrammarRegistry {
    loaded: RwLock<HashMap<String, Arc<Grammar>>>,
}

impl GrammarRegistry {
    /// A registry with nothing compiled yet.
    pub fn new() -> Self {
        Self::default()
    }

    /// Every language this build knows how to parse.
    pub fn available_languages() -> Vec<&'static str> {
        let mut ids: Vec<&'static str> = specs().iter().map(|s| s.language_id).collect();
        ids.sort_unstable();
        ids
    }

    /// Whether a language has a grammar in this build.
    pub fn supports(language_id: &str) -> bool {
        specs().iter().any(|s| s.language_id == language_id)
    }

    /// Get a grammar, compiling its queries on first use.
    pub fn get(&self, language_id: &str) -> Result<Arc<Grammar>> {
        if let Some(grammar) = self.loaded.read().get(language_id) {
            return Ok(Arc::clone(grammar));
        }

        let grammar = Arc::new(self.compile(language_id)?);
        self.loaded.write().insert(language_id.to_string(), Arc::clone(&grammar));
        Ok(grammar)
    }

    fn compile(&self, language_id: &str) -> Result<Grammar> {
        let specs = specs();
        let spec = specs
            .iter()
            .find(|s| s.language_id == language_id)
            .ok_or_else(|| SyntaxError::UnknownLanguage(language_id.to_string()))?;

        let language = (spec.language)();

        // A grammar whose ABI does not match the runtime fails here rather than
        // producing a garbage tree later.
        let mut probe = tree_sitter::Parser::new();
        probe.set_language(&language).map_err(|source| SyntaxError::IncompatibleGrammar {
            language: language_id.to_string(),
            source,
        })?;

        let compile_query = |source: Option<&'static str>| -> Result<Option<Query>> {
            match source {
                None => Ok(None),
                Some(src) => Query::new(&language, src).map(Some).map_err(|source| {
                    SyntaxError::BadQuery { language: language_id.to_string(), source }
                }),
            }
        };

        // The TypeScript grammar is a superset of JavaScript's, and its query
        // files carry only the TypeScript-specific patterns. Both the highlights
        // and the tags query therefore have to be concatenated with the
        // JavaScript ones, or `.ts` files lose string highlighting and produce
        // no class or function symbols at all.
        let is_typescript = matches!(language_id, "typescript" | "typescriptreact");
        let combine = |ts: Option<&'static str>, js: &'static str| -> Result<Option<Query>> {
            let source = format!("{}\n{}", js, ts.unwrap_or(""));
            Query::new(&language, &source).map(Some).map_err(|source| SyntaxError::BadQuery {
                language: language_id.to_string(),
                source,
            })
        };

        let highlights = if is_typescript {
            combine(spec.highlights, tree_sitter_javascript::HIGHLIGHT_QUERY)?
        } else {
            compile_query(spec.highlights)?
        };
        let tags = if is_typescript {
            combine(spec.tags, tree_sitter_javascript::TAGS_QUERY)?
        } else {
            compile_query(spec.tags)?
        };

        // Compile the remaining query before moving `language` into the struct;
        // the closures above borrow it.
        let injections = compile_query(spec.injections)?;

        Ok(Grammar {
            language_id: language_id.to_string(),
            language,
            highlights,
            tags,
            injections,
            block_delimiters: spec.block_delimiters,
            line_comment: spec.line_comment,
        })
    }
}

impl std::fmt::Debug for GrammarRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GrammarRegistry")
            .field("compiled", &self.loaded.read().keys().collect::<Vec<_>>())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_declared_grammar_actually_loads_and_compiles() {
        // This is the test that catches a tree-sitter ABI bump: a grammar crate
        // that no longer matches the runtime fails here, not at runtime in a
        // user's editor.
        let registry = GrammarRegistry::new();
        for language_id in GrammarRegistry::available_languages() {
            let grammar = registry
                .get(language_id)
                .unwrap_or_else(|e| panic!("grammar `{language_id}` failed to load: {e}"));
            assert_eq!(grammar.language_id, language_id);
            assert!(grammar.highlights.is_some(), "`{language_id}` must ship a highlights query");
        }
    }

    #[test]
    fn grammars_are_cached_and_shared() {
        let registry = GrammarRegistry::new();
        let first = registry.get("rust").unwrap();
        let second = registry.get("rust").unwrap();
        assert!(Arc::ptr_eq(&first, &second), "compiling queries twice would be wasteful");
    }

    #[test]
    fn unknown_languages_are_reported_not_guessed() {
        let registry = GrammarRegistry::new();
        assert!(matches!(registry.get("brainfuck"), Err(SyntaxError::UnknownLanguage(_))));
        assert!(!GrammarRegistry::supports("brainfuck"));
        assert!(GrammarRegistry::supports("rust"));
    }

    #[test]
    fn tags_queries_are_present_for_languages_with_symbols() {
        let registry = GrammarRegistry::new();
        for language_id in ["rust", "python", "javascript", "typescript", "go", "c"] {
            assert!(
                registry.get(language_id).unwrap().tags.is_some(),
                "`{language_id}` needs a tags query for the repo map"
            );
        }
    }

    #[test]
    fn typescript_inherits_the_javascript_highlight_patterns() {
        let registry = GrammarRegistry::new();
        let ts = registry.get("typescript").unwrap();
        let js = registry.get("javascript").unwrap();
        let ts_patterns = ts.highlights.as_ref().unwrap().pattern_count();
        let js_patterns = js.highlights.as_ref().unwrap().pattern_count();
        assert!(
            ts_patterns > js_patterns,
            "TS query ({ts_patterns}) should extend the JS query ({js_patterns})"
        );
    }
}
