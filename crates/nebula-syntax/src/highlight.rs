//! Syntax highlighting.
//!
//! Highlighting runs per **visible viewport**, not per document: the query
//! cursor is restricted to the byte range currently on screen, so scrolling
//! through a 200 000-line file costs the same as scrolling through a 200-line
//! one.

use nebula_core::{Range, TextBuffer};
use serde::{Deserialize, Serialize};
use streaming_iterator::StreamingIterator;
use tree_sitter::{QueryCursor, TextProvider};

use crate::Result;
use crate::tree::SyntaxTree;

/// A theme-facing highlight class.
///
/// Deliberately small and closed: a theme has to define a colour for every
/// variant, and the set of grammar capture names is open-ended and inconsistent
/// across languages. [`HighlightKind::from_capture`] does the normalising.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum HighlightKind {
    /// Language keywords: `fn`, `if`, `return`.
    Keyword,
    /// Function and method names.
    Function,
    /// Type names, structs, classes, traits.
    Type,
    /// Variables and parameters.
    Variable,
    /// Named constants and enum members.
    Constant,
    /// String and character literals.
    String,
    /// Numeric literals.
    Number,
    /// Boolean and null-like literals.
    Boolean,
    /// Comments and doc comments.
    Comment,
    /// Operators: `+`, `=>`, `::`.
    Operator,
    /// Brackets, commas, semicolons.
    Punctuation,
    /// Attributes, decorators, annotations.
    Attribute,
    /// Object/struct field and property names.
    Property,
    /// Module, namespace and package names.
    Namespace,
    /// Escape sequences inside strings.
    Escape,
    /// Anything the theme has no more specific colour for.
    Text,
}

impl HighlightKind {
    /// Map a tree-sitter capture name onto a highlight class.
    ///
    /// Capture names are dotted and hierarchical (`function.method.builtin`), so
    /// matching walks from most to least specific and falls back to the first
    /// segment. Grammars disagree on names — Rust says `type`, JavaScript says
    /// `constructor` — and this is the single place that disagreement is
    /// reconciled.
    pub fn from_capture(capture: &str) -> HighlightKind {
        // Exact matches first, for names whose prefix would mislead.
        match capture {
            "variable.parameter" => return HighlightKind::Variable,
            "constant.builtin" => return HighlightKind::Boolean,
            "string.escape" | "escape" => return HighlightKind::Escape,
            "punctuation.delimiter" | "punctuation.bracket" | "punctuation.special" => {
                return HighlightKind::Punctuation;
            }
            _ => {}
        }

        let root = capture.split('.').next().unwrap_or(capture);
        match root {
            "keyword" | "conditional" | "repeat" | "include" | "exception" | "storageclass"
            | "keyword_modifier" => HighlightKind::Keyword,
            "function" | "method" | "constructor" => HighlightKind::Function,
            "type" | "class" | "struct" | "interface" | "enum" | "typedef" => HighlightKind::Type,
            "variable" | "parameter" | "field_identifier" | "label" => HighlightKind::Variable,
            "constant" => HighlightKind::Constant,
            "string" | "character" | "char" => HighlightKind::String,
            "number" | "float" | "integer" => HighlightKind::Number,
            "boolean" => HighlightKind::Boolean,
            "comment" => HighlightKind::Comment,
            "operator" => HighlightKind::Operator,
            "punctuation" | "delimiter" | "bracket" => HighlightKind::Punctuation,
            "attribute" | "annotation" | "decorator" => HighlightKind::Attribute,
            "property" | "field" | "tag" => HighlightKind::Property,
            "namespace" | "module" | "package" => HighlightKind::Namespace,
            "escape" => HighlightKind::Escape,
            _ => HighlightKind::Text,
        }
    }

    /// A stable machine-readable name, used by theme files.
    pub const fn name(&self) -> &'static str {
        match self {
            HighlightKind::Keyword => "keyword",
            HighlightKind::Function => "function",
            HighlightKind::Type => "type",
            HighlightKind::Variable => "variable",
            HighlightKind::Constant => "constant",
            HighlightKind::String => "string",
            HighlightKind::Number => "number",
            HighlightKind::Boolean => "boolean",
            HighlightKind::Comment => "comment",
            HighlightKind::Operator => "operator",
            HighlightKind::Punctuation => "punctuation",
            HighlightKind::Attribute => "attribute",
            HighlightKind::Property => "property",
            HighlightKind::Namespace => "namespace",
            HighlightKind::Escape => "escape",
            HighlightKind::Text => "text",
        }
    }

    /// How specific this class is, used to break ties between two captures on
    /// the exact same node.
    ///
    /// Grammars open their highlights query with blanket rules — Python's very
    /// first pattern is `(identifier) @variable`, which matches every identifier
    /// in the file including function names. A later, narrower pattern then
    /// captures the same node as `@function`. tree-sitter emits both and does
    /// not order them usefully, so the catch-all classes are ranked below
    /// everything else and lose the tie.
    const fn specificity(&self) -> u8 {
        match self {
            HighlightKind::Text => 0,
            HighlightKind::Variable => 1,
            _ => 2,
        }
    }

    /// Every variant, so a theme can be validated for completeness.
    pub const ALL: &'static [HighlightKind] = &[
        HighlightKind::Keyword,
        HighlightKind::Function,
        HighlightKind::Type,
        HighlightKind::Variable,
        HighlightKind::Constant,
        HighlightKind::String,
        HighlightKind::Number,
        HighlightKind::Boolean,
        HighlightKind::Comment,
        HighlightKind::Operator,
        HighlightKind::Punctuation,
        HighlightKind::Attribute,
        HighlightKind::Property,
        HighlightKind::Namespace,
        HighlightKind::Escape,
        HighlightKind::Text,
    ];
}

/// A highlighted span, in char offsets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HighlightSpan {
    /// The characters this class applies to.
    pub range: Range,
    /// The class.
    pub kind: HighlightKind,
}

/// Runs highlight queries against a [`SyntaxTree`].
///
/// Holds a reusable [`QueryCursor`]: allocating one per frame would be a
/// per-keystroke allocation on the hot path.
#[derive(Default)]
pub struct Highlighter {
    cursor: QueryCursor,
}

impl std::fmt::Debug for Highlighter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Highlighter")
    }
}

impl Highlighter {
    /// A new highlighter.
    pub fn new() -> Self {
        Self::default()
    }

    /// Highlight the whole document.
    pub fn highlight(
        &mut self,
        tree: &SyntaxTree,
        buffer: &TextBuffer,
    ) -> Result<Vec<HighlightSpan>> {
        self.highlight_range(tree, buffer, Range::new(0, buffer.len_chars()))
    }

    /// Highlight only `range` — normally the visible viewport plus a margin.
    pub fn highlight_range(
        &mut self,
        tree: &SyntaxTree,
        buffer: &TextBuffer,
        range: Range,
    ) -> Result<Vec<HighlightSpan>> {
        let Some(query) = tree.grammar().highlights.as_ref() else {
            return Ok(Vec::new());
        };

        let range = buffer.clamp_range(range);
        let start_byte = buffer.char_to_byte(range.start)?;
        let end_byte = buffer.char_to_byte(range.end)?;
        self.cursor.set_byte_range(start_byte..end_byte);

        let provider = text_provider(buffer);
        let mut spans: Vec<HighlightSpan> = Vec::new();

        // The tree may have been edited without being re-parsed — that is how
        // the editor keeps a 150 ms re-parse off the keystroke path. In that
        // state tree-sitter has shifted node offsets by the edit's delta
        // without revisiting the nodes themselves, so a node can name a byte
        // past the end of the current text. Clamping keeps highlighting usable
        // against a stale tree instead of failing the frame; the offsets become
        // exact again at the next re-parse.
        let limit = buffer.len_bytes();
        let mut matches = self.cursor.matches(query, tree.root(), provider);
        while let Some(m) = matches.next() {
            for capture in m.captures {
                let capture_name = &query.capture_names()[capture.index as usize];
                let kind = HighlightKind::from_capture(capture_name);
                let node = capture.node;

                let start_byte = node.start_byte().min(limit);
                let end_byte = node.end_byte().min(limit);
                if start_byte >= end_byte {
                    continue;
                }

                spans.push(HighlightSpan {
                    range: Range {
                        start: buffer.byte_to_char(start_byte)?,
                        end: buffer.byte_to_char(end_byte)?,
                    },
                    kind,
                });
            }
        }

        Ok(resolve_overlaps(spans))
    }
}

/// Flatten overlapping captures into a disjoint, sorted span list.
///
/// Captures from a highlights query are either nested (an identifier inside a
/// call expression) or disjoint, never partially overlapping — they are syntax
/// nodes. The rule is therefore:
///
/// * a nested span wins over its enclosing span, for its own extent only; the
///   enclosing span keeps the head and tail around it;
/// * two captures on the *identical* node are resolved by
///   [`HighlightKind::specificity`], because grammars capture the same node
///   under both a blanket rule and a narrow one.
///
/// Sorting encodes both rules — enclosing spans first, and on an exact tie the
/// less specific first — so the painting loop below can simply let the later
/// span win.
fn resolve_overlaps(mut spans: Vec<HighlightSpan>) -> Vec<HighlightSpan> {
    if spans.is_empty() {
        return spans;
    }
    spans.sort_by(|a, b| {
        a.range
            .start
            .cmp(&b.range.start)
            .then_with(|| b.range.len().cmp(&a.range.len()))
            .then_with(|| a.kind.specificity().cmp(&b.kind.specificity()))
    });

    let mut out: Vec<HighlightSpan> = Vec::with_capacity(spans.len());
    for span in spans {
        if span.range.is_empty() {
            continue;
        }
        // Everything already in `out` is sorted and disjoint, so only trailing
        // entries can overlap the incoming span. Pop them, keeping whatever
        // parts stick out on either side.
        let mut head: Option<HighlightSpan> = None;
        let mut tails: Vec<HighlightSpan> = Vec::new();
        while let Some(&prev) = out.last() {
            if prev.range.end <= span.range.start {
                break;
            }
            out.pop();
            if prev.range.start < span.range.start {
                head = Some(HighlightSpan {
                    range: Range { start: prev.range.start, end: span.range.start },
                    kind: prev.kind,
                });
            }
            if prev.range.end > span.range.end {
                tails.push(HighlightSpan {
                    range: Range { start: span.range.end, end: prev.range.end },
                    kind: prev.kind,
                });
            }
        }
        if let Some(head) = head {
            out.push(head);
        }
        out.push(span);
        tails.sort_by_key(|s| s.range.start);
        out.extend(tails);
    }
    out.retain(|s| !s.range.is_empty());
    out
}

/// A text provider that feeds rope chunks to tree-sitter's query engine.
///
/// Shared with the symbol extractor, which runs the same kind of query.
pub(crate) fn text_provider(buffer: &TextBuffer) -> RopeTextProvider<'_> {
    RopeTextProvider { buffer }
}

/// Feeds rope chunks to tree-sitter's query engine.
#[derive(Clone, Copy)]
pub(crate) struct RopeTextProvider<'a> {
    buffer: &'a TextBuffer,
}

impl<'a> TextProvider<&'a [u8]> for RopeTextProvider<'a> {
    type I = RopeChunks<'a>;

    fn text(&mut self, node: tree_sitter::Node<'_>) -> Self::I {
        RopeChunks {
            buffer: self.buffer,
            byte: node.start_byte(),
            end: node.end_byte().min(self.buffer.len_bytes()),
        }
    }
}

/// Iterator over the rope's chunks covering one node.
pub(crate) struct RopeChunks<'a> {
    buffer: &'a TextBuffer,
    byte: usize,
    end: usize,
}

impl<'a> Iterator for RopeChunks<'a> {
    type Item = &'a [u8];

    fn next(&mut self) -> Option<&'a [u8]> {
        if self.byte >= self.end {
            return None;
        }
        let (chunk, chunk_start) =
            nebula_core::rope_ext::chunk_at_byte(self.buffer.rope(), self.byte);
        if chunk.is_empty() {
            return None;
        }
        let from = self.byte - chunk_start;
        let to = (self.end - chunk_start).min(chunk.len());
        self.byte = chunk_start + to;
        Some(&chunk.as_bytes()[from..to])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn highlighting_survives_a_tree_edited_but_not_re_parsed() {
        // The editor paints from an edited-but-stale tree so that a re-parse
        // never sits between a keystroke and a frame. In that state tree-sitter
        // shifts node offsets without revisiting the nodes, so a node can name
        // a byte past the end of the text. Failing the frame over that would
        // make the whole deferred-parse design unusable.
        use nebula_core::{Document, Transaction};

        let grammar = crate::GrammarRegistry::new().get("rust").unwrap();
        let mut document = Document::from_str("fn main() { let value = 1; }\n");
        let mut tree = crate::SyntaxTree::parse(grammar, document.buffer(), 0).unwrap();

        // Delete most of the text and tell the tree about it, without parsing.
        let before = document.buffer().clone();
        document
            .set_selections(nebula_core::SelectionSet::single(nebula_core::Selection::new(4, 28)));
        document.delete_backward().unwrap();

        let change: Vec<Transaction> = document.last_change().to_vec();
        tree.edit(&before, document.buffer(), &change[0]).unwrap();

        let spans = Highlighter::new().highlight(&tree, document.buffer()).unwrap();

        let length = document.buffer().len_chars();
        for span in &spans {
            assert!(span.range.end <= length, "{span:?} runs past a {length}-character buffer");
            assert!(span.range.start < span.range.end, "{span:?} is empty or inverted");
        }
    }

    #[test]
    fn highlighting_is_exact_again_after_the_deferred_re_parse() {
        use nebula_core::{Document, Transaction};

        let grammar = crate::GrammarRegistry::new().get("rust").unwrap();
        let mut document = Document::from_str("fn main() {}");
        let mut tree = crate::SyntaxTree::parse(grammar.clone(), document.buffer(), 0).unwrap();

        let before = document.buffer().clone();
        document.set_caret(12);
        document.insert_at_cursors("\nfn other() {}", false).unwrap();

        let change: Vec<Transaction> = document.last_change().to_vec();
        tree.edit(&before, document.buffer(), &change[0]).unwrap();
        tree.reparse_incremental(document.buffer(), document.version()).unwrap();

        let incremental = Highlighter::new().highlight(&tree, document.buffer()).unwrap();

        let fresh_tree =
            crate::SyntaxTree::parse(grammar, document.buffer(), document.version()).unwrap();
        let fresh = Highlighter::new().highlight(&fresh_tree, document.buffer()).unwrap();

        assert_eq!(incremental, fresh);
    }
    use crate::grammar::GrammarRegistry;
    use crate::tree::SyntaxTree;

    fn highlight_source(language: &str, source: &str) -> Vec<(String, HighlightKind)> {
        let registry = GrammarRegistry::new();
        let grammar = registry.get(language).unwrap();
        let buffer = TextBuffer::from_str(source);
        let tree = SyntaxTree::parse(grammar, &buffer, 0).unwrap();
        let spans = Highlighter::new().highlight(&tree, &buffer).unwrap();
        spans.into_iter().map(|s| (buffer.slice(s.range).unwrap(), s.kind)).collect()
    }

    #[test]
    fn capture_names_normalise_to_theme_classes() {
        assert_eq!(HighlightKind::from_capture("keyword"), HighlightKind::Keyword);
        assert_eq!(HighlightKind::from_capture("function.method"), HighlightKind::Function);
        assert_eq!(HighlightKind::from_capture("type.builtin"), HighlightKind::Type);
        assert_eq!(HighlightKind::from_capture("variable.parameter"), HighlightKind::Variable);
        assert_eq!(HighlightKind::from_capture("constant.builtin"), HighlightKind::Boolean);
        assert_eq!(HighlightKind::from_capture("punctuation.bracket"), HighlightKind::Punctuation);
        assert_eq!(HighlightKind::from_capture("something.unknown"), HighlightKind::Text);
    }

    #[test]
    fn rust_keywords_strings_and_comments_are_classified() {
        let spans = highlight_source("rust", "// note\nfn main() { let s = \"text\"; }");
        let kinds: Vec<_> = spans.iter().map(|(_, k)| *k).collect();

        assert!(kinds.contains(&HighlightKind::Comment), "{spans:?}");
        assert!(kinds.contains(&HighlightKind::Keyword), "{spans:?}");
        assert!(kinds.contains(&HighlightKind::String), "{spans:?}");
        assert!(
            spans.iter().any(|(text, kind)| text == "main" && *kind == HighlightKind::Function),
            "{spans:?}"
        );
    }

    #[test]
    fn python_definitions_are_classified() {
        let spans = highlight_source("python", "def greet(name):\n    return f\"hi {name}\"\n");
        let kinds: Vec<_> = spans.iter().map(|(_, k)| *k).collect();
        assert!(kinds.contains(&HighlightKind::Keyword), "{spans:?}");
        assert!(
            spans.iter().any(|(text, kind)| text == "greet" && *kind == HighlightKind::Function),
            "{spans:?}"
        );
    }

    #[test]
    fn spans_are_sorted_and_disjoint() {
        let registry = GrammarRegistry::new();
        let grammar = registry.get("rust").unwrap();
        let source = "fn compute(a: u32) -> u32 { a * 2 } // doubles\n";
        let buffer = TextBuffer::from_str(source);
        let tree = SyntaxTree::parse(grammar, &buffer, 0).unwrap();
        let spans = Highlighter::new().highlight(&tree, &buffer).unwrap();

        for pair in spans.windows(2) {
            assert!(
                pair[0].range.end <= pair[1].range.start,
                "overlapping spans reached the renderer: {:?} then {:?}",
                pair[0],
                pair[1]
            );
        }
        assert!(spans.iter().all(|s| !s.range.is_empty()));
    }

    #[test]
    fn viewport_highlighting_covers_only_the_requested_range() {
        let registry = GrammarRegistry::new();
        let grammar = registry.get("rust").unwrap();
        let source = "fn a() {}\n".repeat(5_000);
        let buffer = TextBuffer::from_str(&source);
        let tree = SyntaxTree::parse(grammar, &buffer, 0).unwrap();

        let mut highlighter = Highlighter::new();
        let window = Range::new(0, 100);
        let spans = highlighter.highlight_range(&tree, &buffer, window).unwrap();

        assert!(!spans.is_empty());
        // tree-sitter returns whole nodes that intersect the window, so allow a
        // node's worth of slack past the end.
        assert!(
            spans.iter().all(|s| s.range.start < window.end + 32),
            "spans must stay near the viewport"
        );
        let all = highlighter.highlight(&tree, &buffer).unwrap();
        assert!(all.len() > spans.len() * 10, "viewport query must do far less work");
    }

    #[test]
    fn highlighting_an_empty_buffer_yields_nothing() {
        let spans = highlight_source("rust", "");
        assert!(spans.is_empty());
    }

    #[test]
    fn multibyte_source_produces_correctly_bounded_spans() {
        let registry = GrammarRegistry::new();
        let grammar = registry.get("rust").unwrap();
        let source = "fn main() { let s = \"héllo 🌌 world\"; }";
        let buffer = TextBuffer::from_str(source);
        let tree = SyntaxTree::parse(grammar, &buffer, 0).unwrap();
        let spans = Highlighter::new().highlight(&tree, &buffer).unwrap();

        // Every span must be sliceable, which fails loudly if a byte offset
        // leaked through as a char offset.
        for span in &spans {
            buffer.slice(span.range).expect("span must be a valid char range");
        }
        let string_span = spans.iter().find(|s| s.kind == HighlightKind::String).unwrap();
        assert_eq!(buffer.slice(string_span.range).unwrap(), "\"héllo 🌌 world\"");
    }

    #[test]
    fn overlap_resolution_prefers_the_more_specific_span() {
        let spans = vec![
            HighlightSpan { range: Range::new(0, 10), kind: HighlightKind::Variable },
            HighlightSpan { range: Range::new(3, 6), kind: HighlightKind::Function },
        ];
        let resolved = resolve_overlaps(spans);
        // The nested span wins its own extent; the enclosing span keeps the
        // head and the tail around it.
        assert_eq!(resolved.len(), 3);
        assert_eq!(
            resolved[0],
            HighlightSpan { range: Range::new(0, 3), kind: HighlightKind::Variable }
        );
        assert_eq!(
            resolved[1],
            HighlightSpan { range: Range::new(3, 6), kind: HighlightKind::Function }
        );
        assert_eq!(
            resolved[2],
            HighlightSpan { range: Range::new(6, 10), kind: HighlightKind::Variable }
        );
    }

    #[test]
    fn identical_ranges_are_resolved_by_specificity() {
        // Exactly the Python case: `(identifier) @variable` and
        // `(function_definition name: (identifier) @function)` capture the same
        // node, in an order tree-sitter does not guarantee.
        for spans in [
            vec![
                HighlightSpan { range: Range::new(4, 9), kind: HighlightKind::Variable },
                HighlightSpan { range: Range::new(4, 9), kind: HighlightKind::Function },
            ],
            vec![
                HighlightSpan { range: Range::new(4, 9), kind: HighlightKind::Function },
                HighlightSpan { range: Range::new(4, 9), kind: HighlightKind::Variable },
            ],
        ] {
            let resolved = resolve_overlaps(spans);
            assert_eq!(resolved.len(), 1);
            assert_eq!(resolved[0].kind, HighlightKind::Function, "blanket capture must lose");
        }
    }

    #[test]
    fn every_kind_has_a_stable_name() {
        let mut names: Vec<&str> = HighlightKind::ALL.iter().map(|k| k.name()).collect();
        let count = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), count, "theme keys must be unique");
    }
}
