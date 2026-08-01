//! Incremental parse trees.

use std::sync::Arc;

use nebula_core::{Range, TextBuffer};
use tree_sitter::{InputEdit, Node, Parser, Point, Tree};

use crate::grammar::Grammar;
use crate::{Result, SyntaxError};

/// A parse tree kept in sync with a buffer.
///
/// The invariant: after [`SyntaxTree::edit`] followed by [`SyntaxTree::reparse`],
/// the tree matches the buffer exactly. Calling `reparse` without `edit` still
/// produces a correct tree — it just costs a full parse, which is what the
/// incremental path exists to avoid.
pub struct SyntaxTree {
    grammar: Arc<Grammar>,
    parser: Parser,
    tree: Tree,
    /// Buffer version this tree was parsed from, so callers can tell whether the
    /// tree they are holding is current.
    version: u64,
}

impl std::fmt::Debug for SyntaxTree {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SyntaxTree")
            .field("language", &self.grammar.language_id)
            .field("version", &self.version)
            .field("has_error", &self.has_error())
            .finish()
    }
}

impl SyntaxTree {
    /// Parse `buffer` from scratch.
    pub fn parse(grammar: Arc<Grammar>, buffer: &TextBuffer, version: u64) -> Result<Self> {
        let mut parser = Parser::new();
        parser.set_language(&grammar.language).map_err(|source| {
            SyntaxError::IncompatibleGrammar { language: grammar.language_id.clone(), source }
        })?;

        let tree = parse_buffer(&mut parser, buffer, None)
            .ok_or_else(|| SyntaxError::ParseFailed(grammar.language_id.clone()))?;

        Ok(Self { grammar, parser, tree, version })
    }

    /// The grammar this tree was parsed with.
    pub fn grammar(&self) -> &Arc<Grammar> {
        &self.grammar
    }

    /// The underlying tree-sitter tree.
    pub fn tree(&self) -> &Tree {
        &self.tree
    }

    /// The root node.
    pub fn root(&self) -> Node<'_> {
        self.tree.root_node()
    }

    /// The buffer version this tree reflects.
    pub fn version(&self) -> u64 {
        self.version
    }

    /// Whether the tree contains a syntax error.
    ///
    /// Half-typed code is the normal state of a file being edited, so this is
    /// informational — an error node never prevents highlighting the rest.
    pub fn has_error(&self) -> bool {
        self.tree.root_node().has_error()
    }

    /// Tell tree-sitter where an applied transaction moved things, without
    /// re-parsing.
    ///
    /// This is the cheap half of an incremental update: it shifts the existing
    /// nodes' offsets so the tree still describes the new text everywhere the
    /// edit did not touch. Highlighting stays visually correct against it, which
    /// is what lets the expensive half be deferred.
    ///
    /// Leaves the tree marked stale. Nothing else here re-parses, so a caller
    /// that only ever calls this will highlight increasingly stale syntax —
    /// pair it with [`SyntaxTree::reparse_incremental`].
    pub fn edit(
        &mut self,
        buffer_before: &TextBuffer,
        buffer_after: &TextBuffer,
        transaction: &nebula_core::Transaction,
    ) -> Result<()> {
        for edit in transaction.edits() {
            let input_edit = to_input_edit(buffer_before, buffer_after, edit, transaction)?;
            self.tree.edit(&input_edit);
        }
        Ok(())
    }

    /// Re-parse, reusing everything the previous tree still describes.
    ///
    /// Must follow [`SyntaxTree::edit`] for every intervening change, or
    /// tree-sitter reuses subtrees that no longer match the text and the result
    /// is a silently wrong tree.
    ///
    /// ## Why this is not on the keystroke path
    ///
    /// "Incremental" bounds the work by how much of the *tree* changed, not by
    /// how much of the text did. A single character typed into a file with
    /// fifty thousand top-level items forces the root's child list to be
    /// rebuilt, which measures at roughly 150 ms on a 300 000-line file — an
    /// order of magnitude over the whole frame budget. So the editor paints
    /// from the edited-but-stale tree and calls this once typing pauses.
    pub fn reparse_incremental(&mut self, buffer: &TextBuffer, version: u64) -> Result<()> {
        self.tree = parse_buffer(&mut self.parser, buffer, Some(&self.tree))
            .ok_or_else(|| SyntaxError::ParseFailed(self.grammar.language_id.clone()))?;
        self.version = version;
        Ok(())
    }

    /// Apply a transaction and re-parse in one step.
    ///
    /// The two are separate operations with very different costs, so prefer
    /// [`SyntaxTree::edit`] plus a deferred [`SyntaxTree::reparse_incremental`]
    /// anywhere a frame is waiting. This is for callers that want the tree
    /// correct immediately and are not on the keystroke path.
    pub fn apply(
        &mut self,
        buffer_before: &TextBuffer,
        buffer_after: &TextBuffer,
        transaction: &nebula_core::Transaction,
        new_version: u64,
    ) -> Result<()> {
        self.edit(buffer_before, buffer_after, transaction)?;
        self.reparse_incremental(buffer_after, new_version)
    }

    /// Re-parse from scratch, discarding incremental state.
    ///
    /// Used after a reload or a language change, where nothing about the old
    /// tree is reusable.
    pub fn reparse(&mut self, buffer: &TextBuffer, version: u64) -> Result<()> {
        self.tree = parse_buffer(&mut self.parser, buffer, None)
            .ok_or_else(|| SyntaxError::ParseFailed(self.grammar.language_id.clone()))?;
        self.version = version;
        Ok(())
    }

    /// The smallest named node covering `range`.
    ///
    /// Named nodes skip anonymous tokens like `;` and `{`, which is what
    /// "expand selection" wants — selecting the semicolon is never useful.
    pub fn named_node_at(&self, buffer: &TextBuffer, range: Range) -> Result<Option<Range>> {
        let start = buffer.char_to_byte(range.start)?;
        let end = buffer.char_to_byte(range.end)?;
        let node = self.tree.root_node().named_descendant_for_byte_range(start, end);
        match node {
            None => Ok(None),
            Some(node) => Ok(Some(Range {
                start: buffer.byte_to_char(node.start_byte())?,
                end: buffer.byte_to_char(node.end_byte())?,
            })),
        }
    }

    /// Expand `range` to the smallest enclosing syntax node that is strictly
    /// larger, for "expand selection".
    pub fn expand_selection(&self, buffer: &TextBuffer, range: Range) -> Result<Option<Range>> {
        let start = buffer.char_to_byte(range.start)?;
        let end = buffer.char_to_byte(range.end)?;

        let mut node = match self.tree.root_node().named_descendant_for_byte_range(start, end) {
            Some(n) => n,
            None => return Ok(None),
        };

        // Walk up until the node covers strictly more than the current range.
        loop {
            let node_range = Range {
                start: buffer.byte_to_char(node.start_byte())?,
                end: buffer.byte_to_char(node.end_byte())?,
            };
            if node_range != range && node_range.start <= range.start && node_range.end >= range.end
            {
                return Ok(Some(node_range));
            }
            match node.parent() {
                Some(parent) => node = parent,
                None => return Ok(None),
            }
        }
    }

    /// The chain of named node kinds from the root down to `offset`.
    ///
    /// This is the "breadcrumb" the status bar shows, and it is also what the AI
    /// context builder uses to describe where the cursor is without shipping the
    /// whole file.
    pub fn node_path_at(&self, buffer: &TextBuffer, offset: usize) -> Result<Vec<String>> {
        let byte = buffer.char_to_byte(offset)?;
        let mut path = Vec::new();
        let mut node = self.tree.root_node().named_descendant_for_byte_range(byte, byte);
        while let Some(n) = node {
            path.push(n.kind().to_string());
            node = n.parent();
        }
        path.reverse();
        Ok(path)
    }

    /// An s-expression dump of the tree, for debugging and for tests.
    pub fn to_sexp(&self) -> String {
        self.tree.root_node().to_sexp()
    }
}

/// Parse a rope without materialising it as one contiguous string.
///
/// tree-sitter pulls text through a callback, so the rope's chunks are handed
/// over directly — a 200 MB file never becomes a 200 MB `String`.
fn parse_buffer(parser: &mut Parser, buffer: &TextBuffer, old_tree: Option<&Tree>) -> Option<Tree> {
    let rope = buffer.rope();
    parser.parse_with_options(
        &mut |byte_offset: usize, _position: Point| -> &[u8] {
            let (chunk, chunk_start) = nebula_core::rope_ext::chunk_at_byte(rope, byte_offset);
            if chunk.is_empty() {
                return &[];
            }
            &chunk.as_bytes()[byte_offset - chunk_start..]
        },
        old_tree,
        None,
    )
}

/// Translate one [`nebula_core::Edit`] into tree-sitter's `InputEdit`.
///
/// tree-sitter wants byte offsets and row/column points for three positions:
/// where the edit starts, where the old text ended, and where the new text
/// ends. The first two come from the pre-edit buffer, the third from the
/// post-edit buffer.
fn to_input_edit(
    before: &TextBuffer,
    after: &TextBuffer,
    edit: &nebula_core::Edit,
    transaction: &nebula_core::Transaction,
) -> Result<InputEdit> {
    let start_byte = before.char_to_byte(edit.range.start)?;
    let old_end_byte = before.char_to_byte(edit.range.end)?;

    let start_position = point_of(before, edit.range.start)?;
    let old_end_position = point_of(before, edit.range.end)?;

    // Where this edit's replacement text ends in the post-edit buffer.
    let new_end_char = transaction.map_offset(edit.range.start, false) + edit.text.chars().count();
    let new_end_char = new_end_char.min(after.len_chars());
    let new_end_byte = after.char_to_byte(new_end_char)?;
    let new_end_position = point_of(after, new_end_char)?;

    Ok(InputEdit {
        start_byte,
        old_end_byte,
        new_end_byte,
        start_position,
        old_end_position,
        new_end_position,
    })
}

/// tree-sitter `Point`s are row plus **byte** column, not char column.
fn point_of(buffer: &TextBuffer, offset: usize) -> Result<Point> {
    let position = buffer.offset_to_position(offset)?;
    let line_start = buffer.line_start(position.line)?;
    let column = buffer.char_to_byte(offset)? - buffer.char_to_byte(line_start)?;
    Ok(Point::new(position.line, column))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grammar::GrammarRegistry;
    use nebula_core::{Edit, Transaction};

    fn rust_tree(source: &str) -> (SyntaxTree, TextBuffer) {
        let registry = GrammarRegistry::new();
        let grammar = registry.get("rust").unwrap();
        let buffer = TextBuffer::from_str(source);
        let tree = SyntaxTree::parse(grammar, &buffer, 0).unwrap();
        (tree, buffer)
    }

    #[test]
    fn parses_valid_rust_without_errors() {
        let (tree, _) = rust_tree("fn main() { println!(\"hi\"); }");
        assert!(!tree.has_error());
        assert_eq!(tree.root().kind(), "source_file");
    }

    #[test]
    fn half_typed_code_still_produces_a_tree() {
        // The normal state of a file mid-keystroke: it must still highlight.
        let (tree, _) = rust_tree("fn main() { let x = ");
        assert!(tree.has_error());
        assert_eq!(tree.root().kind(), "source_file", "an error node is not a failure to parse");
    }

    #[test]
    fn incremental_edit_matches_a_full_reparse() {
        // The load-bearing correctness property of this module.
        let source = "fn main() {\n    let value = 1;\n}\n";
        let (mut tree, before) = rust_tree(source);

        let mut after = before.clone();
        let transaction = Transaction::single(Edit::insert(source.find("1").unwrap(), "compute("));
        transaction.apply(&mut after).unwrap();
        tree.apply(&before, &after, &transaction, 1).unwrap();

        let registry = GrammarRegistry::new();
        let fresh = SyntaxTree::parse(registry.get("rust").unwrap(), &after, 1).unwrap();
        assert_eq!(
            tree.to_sexp(),
            fresh.to_sexp(),
            "incremental result must be identical to a full parse"
        );
    }

    #[test]
    fn incremental_edit_across_multiple_cursors_matches_full_reparse() {
        let source = "fn a() {}\nfn b() {}\nfn c() {}\n";
        let (mut tree, before) = rust_tree(source);

        let mut after = before.clone();
        let transaction = Transaction::from_edits([
            Edit::insert(3, "aa"),
            Edit::insert(13, "bb"),
            Edit::insert(23, "cc"),
        ])
        .unwrap();
        transaction.apply(&mut after).unwrap();
        tree.apply(&before, &after, &transaction, 1).unwrap();

        let registry = GrammarRegistry::new();
        let fresh = SyntaxTree::parse(registry.get("rust").unwrap(), &after, 1).unwrap();
        assert_eq!(tree.to_sexp(), fresh.to_sexp());
    }

    #[test]
    fn incremental_deletion_matches_full_reparse() {
        let source = "fn main() {\n    let unused = 42;\n    let used = 1;\n}\n";
        let (mut tree, before) = rust_tree(source);

        let start = source.find("    let unused").unwrap();
        let end = source.find("    let used").unwrap();
        let mut after = before.clone();
        let transaction = Transaction::single(Edit::delete(Range::new(start, end)));
        transaction.apply(&mut after).unwrap();
        tree.apply(&before, &after, &transaction, 1).unwrap();

        let registry = GrammarRegistry::new();
        let fresh = SyntaxTree::parse(registry.get("rust").unwrap(), &after, 1).unwrap();
        assert_eq!(tree.to_sexp(), fresh.to_sexp());
        assert!(!tree.has_error());
    }

    #[test]
    fn multibyte_text_keeps_incremental_and_full_parses_in_agreement() {
        // Byte/char confusion in the InputEdit shows up here and nowhere else.
        let source = "fn main() {\n    let s = \"héllo 🌌\";\n}\n";
        let (mut tree, before) = rust_tree(source);

        let insert_at = before.byte_to_char(source.find("🌌").unwrap()).unwrap();
        let mut after = before.clone();
        let transaction = Transaction::single(Edit::insert(insert_at, "🚀 and "));
        transaction.apply(&mut after).unwrap();
        tree.apply(&before, &after, &transaction, 1).unwrap();

        let registry = GrammarRegistry::new();
        let fresh = SyntaxTree::parse(registry.get("rust").unwrap(), &after, 1).unwrap();
        assert_eq!(tree.to_sexp(), fresh.to_sexp());
    }

    #[test]
    fn version_tracks_the_buffer() {
        let (mut tree, buffer) = rust_tree("fn main() {}");
        assert_eq!(tree.version(), 0);
        tree.reparse(&buffer, 7).unwrap();
        assert_eq!(tree.version(), 7);
    }

    #[test]
    fn expand_selection_walks_up_the_tree() {
        let source = "fn main() {\n    let value = compute(1, 2);\n}\n";
        let (tree, buffer) = rust_tree(source);

        // Start on the identifier `compute`.
        let start = source.find("compute").unwrap();
        let mut range = Range::new(start, start + "compute".len());

        let mut sizes = vec![range.len()];
        for _ in 0..4 {
            match tree.expand_selection(&buffer, range).unwrap() {
                Some(next) => {
                    assert!(next.len() > range.len(), "expansion must grow the selection");
                    range = next;
                    sizes.push(range.len());
                }
                None => break,
            }
        }
        assert!(sizes.len() >= 3, "expected several expansion steps, got {sizes:?}");
        assert!(range.len() >= source.len() - 2, "expansion eventually reaches the whole file");
    }

    #[test]
    fn node_path_describes_the_cursor_context() {
        let source = "fn outer() {\n    let x = 1;\n}\n";
        let (tree, buffer) = rust_tree(source);
        let offset = source.find("x = 1").unwrap();
        let path = tree.node_path_at(&buffer, offset).unwrap();

        assert_eq!(path.first().map(String::as_str), Some("source_file"));
        assert!(
            path.iter().any(|k| k == "function_item"),
            "path should record the enclosing function: {path:?}"
        );
    }

    #[test]
    fn parses_every_supported_language() {
        let registry = GrammarRegistry::new();
        let samples: &[(&str, &str)] = &[
            ("rust", "fn main() { let x: u32 = 1; }"),
            ("python", "def f(a):\n    return a + 1\n"),
            ("javascript", "const f = (a) => a + 1;"),
            ("typescript", "const f = (a: number): number => a + 1;"),
            ("typescriptreact", "const C = () => <div className=\"x\">hi</div>;"),
            ("go", "package main\nfunc main() { println(\"hi\") }\n"),
            ("c", "int main(void) { return 0; }"),
            ("json", "{\"key\": [1, 2, true, null]}"),
            ("toml", "[package]\nname = \"x\"\nversion = \"0.1.0\"\n"),
        ];
        for (language, source) in samples {
            let grammar = registry.get(language).unwrap();
            let buffer = TextBuffer::from_str(source);
            let tree = SyntaxTree::parse(grammar, &buffer, 0).unwrap();
            assert!(!tree.has_error(), "`{language}` failed to parse a valid sample: {source}");
        }
    }

    #[test]
    fn large_files_parse_through_the_chunk_callback() {
        // Big enough that the rope holds many chunks, proving the callback
        // stitches them correctly rather than reading only the first.
        let source = "fn f() { let x = 1; }\n".repeat(20_000);
        let (tree, _) = rust_tree(&source);
        assert!(!tree.has_error());
        assert_eq!(tree.root().named_child_count(), 20_000);
    }
}
