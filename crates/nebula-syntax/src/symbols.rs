//! Symbol extraction.
//!
//! Symbols come from each grammar's `tags` query — the same query GitHub uses
//! for code navigation. Using the grammar's own query rather than hand-written
//! node matching means a grammar update brings improved symbol coverage for
//! free, and it is the reason this module is a few hundred lines rather than a
//! few thousand.
//!
//! The output feeds three consumers: the outline view, the fuzzy symbol picker,
//! and the repo map that ranks files for AI context.

use nebula_core::{Range, TextBuffer};
use serde::{Deserialize, Serialize};
use streaming_iterator::StreamingIterator;
use tree_sitter::QueryCursor;

use crate::Result;
use crate::tree::SyntaxTree;

/// What kind of thing a symbol is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum SymbolKind {
    /// A free function.
    Function,
    /// A method on a type.
    Method,
    /// A class, struct, or record.
    Class,
    /// An interface or trait.
    Interface,
    /// An enum.
    Enum,
    /// A module, namespace or package.
    Module,
    /// A constant or static.
    Constant,
    /// A variable binding at file scope.
    Variable,
    /// A struct or class field.
    Field,
    /// A type alias.
    TypeAlias,
    /// A macro.
    Macro,
}

impl SymbolKind {
    /// Map a `tags` query capture name onto a kind.
    ///
    /// Capture names look like `definition.function`; grammars vary in which
    /// ones they emit, so unknown definitions degrade to [`SymbolKind::Variable`]
    /// rather than being dropped — a symbol with a slightly wrong icon is far
    /// more useful than a missing one.
    fn from_capture(capture: &str) -> Option<SymbolKind> {
        let suffix = capture.strip_prefix("definition.")?;
        Some(match suffix {
            "function" => SymbolKind::Function,
            "method" => SymbolKind::Method,
            "class" | "struct" => SymbolKind::Class,
            "interface" | "trait" => SymbolKind::Interface,
            "enum" => SymbolKind::Enum,
            "module" | "namespace" | "package" => SymbolKind::Module,
            "constant" => SymbolKind::Constant,
            "field" | "property" | "member" => SymbolKind::Field,
            "type" => SymbolKind::TypeAlias,
            "macro" => SymbolKind::Macro,
            "var" | "variable" | "let" => SymbolKind::Variable,
            _ => SymbolKind::Variable,
        })
    }

    /// A stable machine-readable name.
    pub const fn name(&self) -> &'static str {
        match self {
            SymbolKind::Function => "function",
            SymbolKind::Method => "method",
            SymbolKind::Class => "class",
            SymbolKind::Interface => "interface",
            SymbolKind::Enum => "enum",
            SymbolKind::Module => "module",
            SymbolKind::Constant => "constant",
            SymbolKind::Variable => "variable",
            SymbolKind::Field => "field",
            SymbolKind::TypeAlias => "type",
            SymbolKind::Macro => "macro",
        }
    }
}

/// A definition found in a document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Symbol {
    /// The identifier.
    pub name: String,
    /// What kind of definition it is.
    pub kind: SymbolKind,
    /// The full extent of the definition, including its body.
    pub range: Range,
    /// Just the name token — where "go to definition" should place the cursor.
    pub name_range: Range,
    /// Zero-based line the definition starts on.
    pub line: usize,
}

/// A reference to a symbol defined elsewhere.
///
/// The repo map uses these as the edges of its graph: a file that references
/// many of another file's definitions depends on it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reference {
    /// The referenced identifier.
    pub name: String,
    /// Where the reference appears.
    pub range: Range,
    /// Zero-based line.
    pub line: usize,
}

/// Extract every definition in the document.
///
/// Results are sorted by position, so the outline view can render them directly.
pub fn symbols(tree: &SyntaxTree, buffer: &TextBuffer) -> Result<Vec<Symbol>> {
    let Some(query) = tree.grammar().tags.as_ref() else {
        return Ok(Vec::new());
    };

    let mut cursor = QueryCursor::new();
    let provider = crate::highlight::text_provider(buffer);
    let mut out: Vec<Symbol> = Vec::new();

    let mut matches = cursor.matches(query, tree.root(), provider);
    while let Some(m) = matches.next() {
        // A tags match pairs a `@definition.*` node with a `@name` node. Both
        // must be present for the result to be useful.
        let mut definition: Option<(SymbolKind, tree_sitter::Node)> = None;
        let mut name_node: Option<tree_sitter::Node> = None;

        for capture in m.captures {
            let capture_name = &query.capture_names()[capture.index as usize];
            if *capture_name == "name" {
                name_node = Some(capture.node);
            } else if let Some(kind) = SymbolKind::from_capture(capture_name) {
                definition = Some((kind, capture.node));
            }
        }

        let (Some((kind, def_node)), Some(name_node)) = (definition, name_node) else {
            continue;
        };

        let name_range = Range {
            start: buffer.byte_to_char(name_node.start_byte())?,
            end: buffer.byte_to_char(name_node.end_byte())?,
        };
        let range = Range {
            start: buffer.byte_to_char(def_node.start_byte())?,
            end: buffer.byte_to_char(def_node.end_byte())?,
        };
        let name = buffer.slice(name_range)?;
        if name.is_empty() {
            continue;
        }

        out.push(Symbol {
            name,
            kind,
            range,
            name_range,
            line: buffer.offset_to_line(range.start)?,
        });
    }

    out.sort_by_key(|s| (s.range.start, s.range.end));
    out.dedup_by(|a, b| a.name == b.name && a.name_range == b.name_range);
    Ok(out)
}

/// Extract every reference (call site, type use) in the document.
pub fn references(tree: &SyntaxTree, buffer: &TextBuffer) -> Result<Vec<Reference>> {
    let Some(query) = tree.grammar().tags.as_ref() else {
        return Ok(Vec::new());
    };

    let mut cursor = QueryCursor::new();
    let provider = crate::highlight::text_provider(buffer);
    let mut out: Vec<Reference> = Vec::new();

    let mut matches = cursor.matches(query, tree.root(), provider);
    while let Some(m) = matches.next() {
        let is_reference = m.captures.iter().any(|c| {
            query.capture_names()[c.index as usize].starts_with("reference.")
        });
        if !is_reference {
            continue;
        }
        for capture in m.captures {
            if query.capture_names()[capture.index as usize] != "name" {
                continue;
            }
            let range = Range {
                start: buffer.byte_to_char(capture.node.start_byte())?,
                end: buffer.byte_to_char(capture.node.end_byte())?,
            };
            let name = buffer.slice(range)?;
            if name.is_empty() {
                continue;
            }
            out.push(Reference { name, range, line: buffer.offset_to_line(range.start)? });
        }
    }

    out.sort_by_key(|r| r.range.start);
    out.dedup();
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grammar::GrammarRegistry;

    fn extract(language: &str, source: &str) -> Vec<Symbol> {
        let registry = GrammarRegistry::new();
        let grammar = registry.get(language).unwrap();
        let buffer = TextBuffer::from_str(source);
        let tree = SyntaxTree::parse(grammar, &buffer, 0).unwrap();
        symbols(&tree, &buffer).unwrap()
    }

    fn names(symbols: &[Symbol]) -> Vec<&str> {
        symbols.iter().map(|s| s.name.as_str()).collect()
    }

    #[test]
    fn rust_definitions_are_extracted() {
        let source = r#"
pub struct Config {
    pub retries: u32,
}

pub trait Handler {
    fn handle(&self);
}

impl Handler for Config {
    fn handle(&self) {}
}

pub fn build(path: &str) -> Config {
    Config { retries: 3 }
}

pub enum Mode { Fast, Slow }
"#;
        let found = extract("rust", source);
        let found_names = names(&found);

        assert!(found_names.contains(&"Config"), "{found_names:?}");
        assert!(found_names.contains(&"Handler"), "{found_names:?}");
        assert!(found_names.contains(&"build"), "{found_names:?}");
        assert!(found_names.contains(&"Mode"), "{found_names:?}");

        let config = found.iter().find(|s| s.name == "Config").unwrap();
        assert_eq!(config.kind, SymbolKind::Class);
        let build = found.iter().find(|s| s.name == "build").unwrap();
        assert_eq!(build.kind, SymbolKind::Function);
    }

    #[test]
    fn python_definitions_are_extracted() {
        let source = "class Service:\n    def start(self):\n        pass\n\ndef helper():\n    pass\n";
        let found = extract("python", source);
        let found_names = names(&found);
        assert!(found_names.contains(&"Service"), "{found_names:?}");
        assert!(found_names.contains(&"start"), "{found_names:?}");
        assert!(found_names.contains(&"helper"), "{found_names:?}");
    }

    #[test]
    fn go_definitions_are_extracted() {
        let source = "package main\n\ntype Server struct{}\n\nfunc (s *Server) Run() {}\n\nfunc main() {}\n";
        let found = extract("go", source);
        let found_names = names(&found);
        assert!(found_names.contains(&"Server"), "{found_names:?}");
        assert!(found_names.contains(&"main"), "{found_names:?}");
    }

    #[test]
    fn typescript_definitions_are_extracted() {
        let source = "export class Repo {\n  find(id: string) { return id; }\n}\nexport function make(): Repo { return new Repo(); }\n";
        let found = extract("typescript", source);
        let found_names = names(&found);
        assert!(found_names.contains(&"Repo"), "{found_names:?}");
        assert!(found_names.contains(&"make"), "{found_names:?}");
    }

    #[test]
    fn name_range_points_at_the_identifier_only() {
        let source = "pub fn interesting_name() {}\n";
        let found = extract("rust", source);
        let symbol = found.iter().find(|s| s.name == "interesting_name").unwrap();

        let buffer = TextBuffer::from_str(source);
        assert_eq!(buffer.slice(symbol.name_range).unwrap(), "interesting_name");
        assert!(
            symbol.range.len() > symbol.name_range.len(),
            "the definition range should cover the whole item"
        );
    }

    #[test]
    fn line_numbers_are_recorded_for_the_outline_view() {
        let source = "fn first() {}\nfn second() {}\nfn third() {}\n";
        let found = extract("rust", source);
        let second = found.iter().find(|s| s.name == "second").unwrap();
        assert_eq!(second.line, 1);
    }

    #[test]
    fn symbols_are_returned_in_document_order() {
        let source = "fn zebra() {}\nfn apple() {}\nfn mango() {}\n";
        let found = extract("rust", source);
        let ordered: Vec<_> = found.iter().map(|s| s.range.start).collect();
        let mut sorted = ordered.clone();
        sorted.sort_unstable();
        assert_eq!(ordered, sorted);
    }

    #[test]
    fn languages_without_a_tags_query_yield_no_symbols() {
        assert!(extract("json", "{\"a\": 1}").is_empty());
        assert!(extract("toml", "[table]\nkey = 1\n").is_empty());
    }

    #[test]
    fn references_are_extracted_for_the_repo_map() {
        let registry = GrammarRegistry::new();
        let grammar = registry.get("rust").unwrap();
        let source = "fn main() {\n    helper();\n    other::thing();\n}\n";
        let buffer = TextBuffer::from_str(source);
        let tree = SyntaxTree::parse(grammar, &buffer, 0).unwrap();

        let refs = references(&tree, &buffer).unwrap();
        let ref_names: Vec<&str> = refs.iter().map(|r| r.name.as_str()).collect();
        assert!(ref_names.contains(&"helper"), "{ref_names:?}");
    }

    #[test]
    fn broken_source_still_yields_the_symbols_it_can() {
        // A file mid-edit must still populate the outline.
        let source = "fn complete() {}\nfn broken( {\n";
        let found = extract("rust", source);
        assert!(names(&found).contains(&"complete"), "{:?}", names(&found));
    }
}
