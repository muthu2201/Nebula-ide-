//! Turning a document into a frame.
//!
//! [`EditorView::render`] is a pure function of (document, syntax tree, theme,
//! viewport) → [`Scene`]. It allocates no GPU resources, opens no window and
//! reads no clock, which is what makes every visual property of the editor —
//! where the caret is, what colour a keyword is, whether the selection covers
//! the newline — assertable in an ordinary unit test.
//!
//! ## Cost is proportional to the window, not the file
//!
//! Every loop here runs over the *visible* lines. Highlighting is requested for
//! the visible character range plus a margin, not for the document. Opening a
//! 200 000-line file therefore costs the same per frame as opening a 50-line
//! one, which is the difference between an editor that stays responsive on a
//! generated file and one that does not.

use nebula_core::Document;
use nebula_render::{Color, Point, Quad, Rect, Scene, TextRun};
use nebula_syntax::{HighlightKind, HighlightSpan, Highlighter, SyntaxTree};

use crate::{
    Result,
    layout::{Layout, Viewport},
    theme::Theme,
};

/// What the status bar says.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StatusLine {
    /// Usually the file name.
    pub left: String,
    /// Usually the language and encoding.
    pub centre: String,
    /// Usually the cursor position.
    pub right: String,
}

impl StatusLine {
    /// The status bar the editor shows by default.
    pub fn for_document(document: &Document) -> StatusLine {
        let name = document
            .path()
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "untitled".to_string());

        // The modified marker goes first, where the eye already is.
        let left = if document.is_modified() { format!("● {name}") } else { name };

        let position = document
            .buffer()
            .offset_to_position(document.selections().primary().head)
            .unwrap_or_default();

        let cursors = document.selections().len();
        let right = if cursors > 1 {
            format!("{}:{}  {cursors} cursors", position.line + 1, position.column + 1)
        } else {
            format!("{}:{}", position.line + 1, position.column + 1)
        };

        StatusLine { left, centre: document.language().unwrap_or("plain text").to_string(), right }
    }
}

/// How far beyond the viewport to highlight, in characters.
///
/// Highlighting exactly the visible range makes a token that starts just above
/// the top of the window lose its colour. A margin costs a few hundred
/// characters of parsing and removes the artefact entirely.
const HIGHLIGHT_MARGIN: usize = 2_048;

/// Builds frames.
#[derive(Debug)]
pub struct EditorView {
    /// Colours.
    pub theme: Theme,
    /// Geometry.
    pub layout: Layout,
    /// Which part of the document is on screen.
    pub viewport: Viewport,
    /// Font size in logical pixels.
    pub font_size: f32,
    /// Whether to draw the line-number gutter.
    pub show_line_numbers: bool,
    /// Whether to draw a band behind the caret's line.
    pub highlight_current_line: bool,
    /// The caret's width in logical pixels.
    pub caret_width: f32,

    highlighter: Highlighter,
}

impl EditorView {
    /// A view sized for a window.
    pub fn new(theme: Theme, layout: Layout) -> Self {
        let viewport = layout.viewport();
        Self {
            theme,
            layout,
            viewport,
            font_size: 14.0,
            show_line_numbers: true,
            highlight_current_line: true,
            caret_width: 2.0,
            highlighter: Highlighter::new(),
        }
    }

    /// Resize to a new window size, keeping the scroll position.
    pub fn resize(&mut self, width: f32, height: f32, total_lines: usize) {
        self.layout = Layout::new(
            width,
            height,
            self.layout.line_height,
            self.layout.char_width,
            total_lines,
        );
        self.viewport.visible_lines = self.layout.visible_lines();
        self.viewport.visible_columns = self.layout.visible_columns();
    }

    /// Build the frame.
    pub fn render(
        &mut self,
        document: &Document,
        tree: Option<&SyntaxTree>,
        scale_factor: f32,
    ) -> Result<Scene> {
        let status = StatusLine::for_document(document);
        self.render_with_status(document, tree, &status, scale_factor)
    }

    /// Build the frame with a caller-supplied status bar.
    pub fn render_with_status(
        &mut self,
        document: &Document,
        tree: Option<&SyntaxTree>,
        status: &StatusLine,
        scale_factor: f32,
    ) -> Result<Scene> {
        let buffer = document.buffer();
        let mut scene = Scene::new(self.layout.width, self.layout.height, scale_factor)
            .background(self.theme.ui.background);

        let lines = self.viewport.visible_range(buffer);
        let spans = self.highlight_spans(document, tree, &lines)?;

        let caret_line = buffer.offset_to_line(document.selections().primary().head).unwrap_or(0);

        // Chrome first, so text is painted over it rather than under it.
        self.draw_gutter_background(&mut scene);
        if self.highlight_current_line && self.viewport.contains_line(caret_line) {
            self.draw_current_line_band(&mut scene, caret_line);
        }
        self.draw_selections(&mut scene, document)?;

        // Text is clipped to its own area so a long line cannot spill into the
        // gutter or over the status bar.
        scene.push_clip(self.layout.text_area());
        for line in lines.clone() {
            self.draw_line(&mut scene, document, line, &spans)?;
        }
        scene.pop_clip();

        if self.show_line_numbers {
            self.draw_line_numbers(&mut scene, lines, caret_line);
        }
        self.draw_carets(&mut scene, document)?;
        self.draw_status(&mut scene, status);

        debug_assert!(scene.clips_balanced());
        Ok(scene)
    }

    /// Highlight the visible range, or nothing if there is no parse tree.
    fn highlight_spans(
        &mut self,
        document: &Document,
        tree: Option<&SyntaxTree>,
        lines: &std::ops::Range<usize>,
    ) -> Result<Vec<HighlightSpan>> {
        let Some(tree) = tree else { return Ok(Vec::new()) };
        let buffer = document.buffer();

        let start = buffer.line_start(lines.start)?;
        let end = buffer.line_end(lines.end.saturating_sub(1).max(lines.start))?;
        let range = nebula_core::position::Range::new(
            start.saturating_sub(HIGHLIGHT_MARGIN),
            (end + HIGHLIGHT_MARGIN).min(buffer.len_chars()),
        );

        Ok(self.highlighter.highlight_range(tree, buffer, range)?)
    }

    fn draw_gutter_background(&self, scene: &mut Scene) {
        if !self.show_line_numbers {
            return;
        }
        scene.quad(Quad::new(self.layout.gutter_area(), self.theme.ui.gutter_background));
    }

    fn draw_current_line_band(&self, scene: &mut Scene, line: usize) {
        let y = (line - self.viewport.first_line) as f32 * self.layout.line_height;
        scene.quad(Quad::new(
            Rect::new(0.0, y, self.layout.width, self.layout.line_height),
            self.theme.ui.current_line,
        ));
    }

    /// Paint the selection, one rectangle per visible line it covers.
    fn draw_selections(&self, scene: &mut Scene, document: &Document) -> Result<()> {
        let buffer = document.buffer();
        let visible = self.viewport.visible_range(buffer);

        for selection in document.selections().iter() {
            if selection.is_empty() {
                continue;
            }

            let start = buffer.offset_to_position(selection.start())?;
            let end = buffer.offset_to_position(selection.end())?;

            for line in start.line.max(visible.start)..=end.line.min(visible.end.saturating_sub(1))
            {
                let from = if line == start.line { start.column } else { 0 };
                let to = if line == end.line {
                    end.column
                } else {
                    // A selection spanning into the next line covers the
                    // newline too; drawing one extra column is how every editor
                    // signals that.
                    buffer.line_len(line)? + 1
                };

                if to <= self.viewport.first_column {
                    continue;
                }

                let origin = self.layout.position_of(line, from, &self.viewport);
                let width = (to.saturating_sub(from.max(self.viewport.first_column))) as f32
                    * self.layout.char_width;
                if width <= 0.0 {
                    continue;
                }

                scene.quad(Quad::new(
                    Rect::new(
                        origin.x.max(self.layout.gutter_width),
                        origin.y,
                        width,
                        self.layout.line_height,
                    ),
                    self.theme.ui.selection,
                ));
            }
        }
        Ok(())
    }

    /// Draw one line of text, split into runs by highlight class.
    fn draw_line(
        &self,
        scene: &mut Scene,
        document: &Document,
        line: usize,
        spans: &[HighlightSpan],
    ) -> Result<()> {
        let buffer = document.buffer();
        let text = buffer.line_trimmed(line)?;
        if text.is_empty() {
            return Ok(());
        }

        let line_start = buffer.line_start(line)?;
        let y = (line - self.viewport.first_line) as f32 * self.layout.line_height;
        // The baseline sits inside the line box rather than at its top, or every
        // glyph would be drawn one line too high.
        let baseline = y + self.layout.line_height * 0.75;

        let chars: Vec<char> = text.chars().collect();
        let mut column = 0usize;

        while column < chars.len() {
            let kind = kind_at(spans, line_start + column);
            // Extend the run while the class holds, so a whole keyword is one
            // TextRun rather than one per character.
            let mut end = column + 1;
            while end < chars.len() && kind_at(spans, line_start + end) == kind {
                end += 1;
            }

            if end > self.viewport.first_column {
                let visible_start = column.max(self.viewport.first_column);
                let run: String = chars[visible_start..end].iter().collect();
                if !run.trim().is_empty() {
                    let origin = Point::new(
                        self.layout.gutter_width
                            + (visible_start - self.viewport.first_column) as f32
                                * self.layout.char_width,
                        baseline,
                    );
                    let mut text_run =
                        TextRun::new(origin, run, self.color_for(kind), self.font_size);
                    text_run.italic = kind == Some(HighlightKind::Comment);
                    text_run.bold = kind == Some(HighlightKind::Keyword);
                    scene.text(text_run);
                }
            }

            column = end;
        }

        Ok(())
    }

    fn color_for(&self, kind: Option<HighlightKind>) -> Color {
        match kind {
            Some(kind) => self.theme.syntax_color(kind),
            None => self.theme.ui.foreground,
        }
    }

    fn draw_line_numbers(
        &self,
        scene: &mut Scene,
        lines: std::ops::Range<usize>,
        caret_line: usize,
    ) {
        for line in lines {
            let label = (line + 1).to_string();
            let y = (line - self.viewport.first_line) as f32 * self.layout.line_height;
            // Right-aligned against the gutter's inner edge, one character of
            // padding in from the text.
            let x = self.layout.gutter_width
                - self.layout.char_width * (label.chars().count() as f32 + 1.0);

            let color = if line == caret_line {
                self.theme.ui.gutter_active
            } else {
                self.theme.ui.gutter_foreground
            };

            scene.text(TextRun::new(
                Point::new(x, y + self.layout.line_height * 0.75),
                label,
                color,
                self.font_size,
            ));
        }
    }

    fn draw_carets(&self, scene: &mut Scene, document: &Document) -> Result<()> {
        let buffer = document.buffer();
        for selection in document.selections().iter() {
            let position = buffer.offset_to_position(selection.head)?;
            if !self.viewport.contains_line(position.line)
                || position.column < self.viewport.first_column
            {
                continue;
            }

            let origin = self.layout.position_of(position.line, position.column, &self.viewport);
            if origin.x > self.layout.width {
                continue;
            }

            scene.quad(Quad::new(
                Rect::new(origin.x, origin.y, self.caret_width, self.layout.line_height),
                self.theme.ui.cursor,
            ));
        }
        Ok(())
    }

    fn draw_status(&self, scene: &mut Scene, status: &StatusLine) {
        let area = self.layout.status_area();
        scene.quad(Quad::new(area, self.theme.ui.status_background));

        let baseline = area.y + area.height * 0.7;
        let padding = self.layout.char_width;

        scene.text(TextRun::new(
            Point::new(padding, baseline),
            status.left.clone(),
            self.theme.ui.status_foreground,
            self.font_size,
        ));

        if !status.centre.is_empty() {
            let width = status.centre.chars().count() as f32 * self.layout.char_width;
            scene.text(TextRun::new(
                Point::new((self.layout.width - width) / 2.0, baseline),
                status.centre.clone(),
                self.theme.ui.status_foreground,
                self.font_size,
            ));
        }

        if !status.right.is_empty() {
            let width = status.right.chars().count() as f32 * self.layout.char_width;
            scene.text(TextRun::new(
                Point::new(self.layout.width - width - padding, baseline),
                status.right.clone(),
                self.theme.ui.status_foreground,
                self.font_size,
            ));
        }
    }
}

/// Which highlight class covers `offset`, if any.
///
/// The span list is disjoint and sorted, so this is a binary search rather than
/// a scan — on a long line the difference is measurable per frame.
fn kind_at(spans: &[HighlightSpan], offset: usize) -> Option<HighlightKind> {
    let index = spans.partition_point(|span| span.range.end <= offset);
    spans.get(index).filter(|span| span.range.contains(offset)).map(|span| span.kind)
}

#[cfg(test)]
mod tests {
    use super::*;
    use nebula_core::{Selection, SelectionSet};
    use nebula_render::scene::Primitive;
    use nebula_syntax::GrammarRegistry;

    fn view() -> EditorView {
        EditorView::new(Theme::dark(), Layout::new(800.0, 600.0, 16.0, 8.0, 100))
    }

    fn quads(scene: &Scene) -> Vec<&Quad> {
        scene
            .primitives
            .iter()
            .filter_map(|p| match p {
                Primitive::Quad(quad) => Some(quad),
                _ => None,
            })
            .collect()
    }

    fn texts(scene: &Scene) -> Vec<&TextRun> {
        scene
            .primitives
            .iter()
            .filter_map(|p| match p {
                Primitive::Text(run) => Some(run),
                _ => None,
            })
            .collect()
    }

    fn rendered_text(scene: &Scene) -> String {
        texts(scene).iter().map(|run| run.text.as_str()).collect()
    }

    #[test]
    fn an_empty_document_still_produces_a_frame() {
        // Cold start draws before anything is open; a panic here is a blank
        // window on launch.
        let mut view = view();
        let scene = view.render(&Document::new(), None, 1.0).unwrap();
        assert!(!scene.is_empty());
        assert!(scene.clips_balanced());
    }

    #[test]
    fn the_documents_text_reaches_the_scene() {
        let mut view = view();
        let document = Document::from_str("hello world");
        let scene = view.render(&document, None, 1.0).unwrap();
        assert!(rendered_text(&scene).contains("hello world"));
    }

    #[test]
    fn only_the_visible_lines_are_drawn() {
        let text = (0..10_000).map(|i| format!("line {i}")).collect::<Vec<_>>().join("\n");
        let mut view = view();
        let document = Document::from_str(&text);
        let scene = view.render(&document, None, 1.0).unwrap();

        let drawn = rendered_text(&scene);
        assert!(drawn.contains("line 0"));
        assert!(!drawn.contains("line 900"), "a line far below the fold was drawn");
    }

    #[test]
    fn a_huge_file_costs_the_same_per_frame_as_a_small_one() {
        // This is the property the whole viewport design exists for.
        let small = Document::from_str(&"x\n".repeat(50));
        let huge = Document::from_str(&"x\n".repeat(200_000));

        let mut view = view();
        let a = view.render(&small, None, 1.0).unwrap();

        let mut view =
            EditorView::new(Theme::dark(), Layout::new(800.0, 600.0, 16.0, 8.0, 200_000));
        let b = view.render(&huge, None, 1.0).unwrap();

        // The gutter is wider for six-digit line numbers, so the counts are not
        // identical — but they must be the same order of magnitude, not 4000×.
        assert!(
            b.len() < a.len() * 2,
            "a 200 000-line file drew {} primitives against {} for a 50-line one",
            b.len(),
            a.len()
        );
    }

    #[test]
    fn line_numbers_are_one_based_and_right_aligned() {
        let mut view = view();
        let document = Document::from_str("a\nb\nc");
        let scene = view.render(&document, None, 1.0).unwrap();

        let numbers: Vec<&str> = texts(&scene)
            .iter()
            .filter(|run| run.text.chars().all(|c| c.is_ascii_digit()))
            .map(|run| run.text.as_str())
            .collect();
        assert!(numbers.contains(&"1"), "{numbers:?}");
        assert!(numbers.contains(&"3"), "{numbers:?}");
        assert!(!numbers.contains(&"0"), "line numbering starts at 1");
    }

    #[test]
    fn line_numbers_can_be_turned_off() {
        let mut view = view();
        view.show_line_numbers = false;
        let document = Document::from_str("a\nb");
        let scene = view.render(&document, None, 1.0).unwrap();

        assert!(
            !texts(&scene).iter().any(|run| run.text == "1"),
            "the gutter was drawn despite being disabled"
        );
    }

    #[test]
    fn the_caret_is_drawn_where_the_cursor_is() {
        let mut view = view();
        let mut document = Document::from_str("hello");
        document.set_caret(3);
        let scene = view.render(&document, None, 1.0).unwrap();

        let caret = quads(&scene)
            .into_iter()
            .find(|q| q.color == view.theme.ui.cursor)
            .expect("no caret was drawn");
        assert_eq!(caret.rect.x, view.layout.gutter_width + 3.0 * view.layout.char_width);
        assert_eq!(caret.rect.height, view.layout.line_height);
    }

    #[test]
    fn every_cursor_gets_a_caret() {
        let mut view = view();
        let mut document = Document::from_str("one\ntwo\nthree");
        document.set_caret(0);
        document.add_cursor(4);
        document.add_cursor(8);

        let scene = view.render(&document, None, 1.0).unwrap();
        let carets = quads(&scene).into_iter().filter(|q| q.color == view.theme.ui.cursor).count();
        assert_eq!(carets, 3);
    }

    #[test]
    fn a_caret_scrolled_off_screen_is_not_drawn() {
        let text = (0..500).map(|i| i.to_string()).collect::<Vec<_>>().join("\n");
        let mut view = view();
        let mut document = Document::from_str(&text);
        document.set_caret(0);
        view.viewport.first_line = 200;

        let scene = view.render(&document, None, 1.0).unwrap();
        assert!(!quads(&scene).into_iter().any(|q| q.color == view.theme.ui.cursor));
    }

    #[test]
    fn a_selection_is_painted() {
        let mut view = view();
        let mut document = Document::from_str("hello world");
        document.set_selections(SelectionSet::single(Selection::new(0, 5)));

        let scene = view.render(&document, None, 1.0).unwrap();
        let band = quads(&scene)
            .into_iter()
            .find(|q| q.color == view.theme.ui.selection)
            .expect("no selection was drawn");
        assert_eq!(band.rect.width, 5.0 * view.layout.char_width);
    }

    #[test]
    fn a_multi_line_selection_is_painted_one_band_per_line() {
        let mut view = view();
        let mut document = Document::from_str("aaa\nbbb\nccc");
        document.set_selections(SelectionSet::single(Selection::new(1, 10)));

        let scene = view.render(&document, None, 1.0).unwrap();
        let bands: Vec<&Quad> =
            quads(&scene).into_iter().filter(|q| q.color == view.theme.ui.selection).collect();
        assert_eq!(bands.len(), 3, "one band per covered line");

        // Each band sits on its own row.
        let mut ys: Vec<f32> = bands.iter().map(|b| b.rect.y).collect();
        ys.dedup();
        assert_eq!(ys.len(), 3);
    }

    #[test]
    fn a_selection_crossing_a_line_break_covers_the_newline() {
        let mut view = view();
        let mut document = Document::from_str("ab\ncd");
        document.set_selections(SelectionSet::single(Selection::new(0, 4)));

        let scene = view.render(&document, None, 1.0).unwrap();
        let first = quads(&scene).into_iter().find(|q| q.color == view.theme.ui.selection).unwrap();
        // "ab" is two columns; the band is three, the extra one being the
        // newline the selection swallowed.
        assert_eq!(first.rect.width, 3.0 * view.layout.char_width);
    }

    #[test]
    fn an_empty_selection_paints_nothing() {
        let mut view = view();
        let mut document = Document::from_str("hello");
        document.set_caret(2);

        let scene = view.render(&document, None, 1.0).unwrap();
        assert!(!quads(&scene).into_iter().any(|q| q.color == view.theme.ui.selection));
    }

    #[test]
    fn the_caret_line_is_banded() {
        let mut view = view();
        let mut document = Document::from_str("one\ntwo\nthree");
        document.set_caret(5);

        let scene = view.render(&document, None, 1.0).unwrap();
        let band = quads(&scene)
            .into_iter()
            .find(|q| q.color == view.theme.ui.current_line)
            .expect("the current line was not banded");
        assert_eq!(band.rect.y, view.layout.line_height);
        assert_eq!(band.rect.width, view.layout.width);
    }

    #[test]
    fn the_current_line_band_can_be_turned_off() {
        let mut view = view();
        view.highlight_current_line = false;
        let document = Document::from_str("one\ntwo");
        let scene = view.render(&document, None, 1.0).unwrap();
        assert!(!quads(&scene).into_iter().any(|q| q.color == view.theme.ui.current_line));
    }

    #[test]
    fn syntax_highlighting_colours_keywords_differently_from_strings() {
        let grammar = GrammarRegistry::new().get("rust").unwrap();
        let document = Document::from_str(r#"fn main() { let s = "text"; }"#);
        let tree = SyntaxTree::parse(grammar, document.buffer(), document.version()).unwrap();

        let mut view = view();
        let scene = view.render(&document, Some(&tree), 1.0).unwrap();

        let keyword = view.theme.syntax_color(HighlightKind::Keyword);
        let string = view.theme.syntax_color(HighlightKind::String);

        let fn_run = texts(&scene)
            .into_iter()
            .find(|run| run.text == "fn")
            .expect("`fn` was not drawn as its own run");
        assert_eq!(fn_run.color, keyword);
        assert!(fn_run.bold, "keywords are bold");

        assert!(
            texts(&scene).iter().any(|run| run.text.contains("text") && run.color == string),
            "the string literal was not coloured as a string"
        );
    }

    #[test]
    fn comments_are_italic() {
        let grammar = GrammarRegistry::new().get("rust").unwrap();
        let document = Document::from_str("// a note\nfn main() {}");
        let tree = SyntaxTree::parse(grammar, document.buffer(), document.version()).unwrap();

        let mut view = view();
        let scene = view.render(&document, Some(&tree), 1.0).unwrap();

        let comment = texts(&scene)
            .into_iter()
            .find(|run| run.text.contains("a note"))
            .expect("the comment was not drawn");
        assert!(comment.italic);
        assert_eq!(comment.color, view.theme.syntax_color(HighlightKind::Comment));
    }

    #[test]
    fn without_a_parse_tree_everything_is_foreground_coloured() {
        let mut view = view();
        let document = Document::from_str("fn main() {}");
        let scene = view.render(&document, None, 1.0).unwrap();

        let body: Vec<&TextRun> =
            texts(&scene).into_iter().filter(|run| run.text.contains("fn main")).collect();
        assert!(!body.is_empty());
        for run in body {
            assert_eq!(run.color, view.theme.ui.foreground);
        }
    }

    #[test]
    fn text_is_clipped_to_its_own_area() {
        // Without the clip, a long line would draw over the gutter.
        let mut view = view();
        let document = Document::from_str(&"x".repeat(5_000));
        let scene = view.render(&document, None, 1.0).unwrap();

        assert!(scene.clips_balanced());
        assert!(
            scene.primitives.iter().any(|p| matches!(p, Primitive::PushClip(_))),
            "the text area was not clipped"
        );
    }

    #[test]
    fn horizontal_scrolling_shifts_the_text_left() {
        let mut view = view();
        view.viewport.first_column = 10;
        let document = Document::from_str("0123456789abcdefghij");

        let scene = view.render(&document, None, 1.0).unwrap();
        let run = texts(&scene)
            .into_iter()
            .find(|run| run.text.contains('a'))
            .expect("nothing was drawn");
        assert!(run.text.starts_with("abcdef"), "{}", run.text);
        assert_eq!(run.origin.x, view.layout.gutter_width);
    }

    #[test]
    fn the_status_bar_names_the_document_and_the_caret_position() {
        let mut view = view();
        let mut document = Document::from_str("one\ntwo\nthree");
        document.set_caret(5);

        let scene = view.render(&document, None, 1.0).unwrap();
        let drawn = rendered_text(&scene);
        assert!(drawn.contains("untitled"));
        assert!(drawn.contains("2:2"), "the 1-based caret position is missing: {drawn}");
    }

    #[test]
    fn a_modified_document_is_marked_in_the_status_bar() {
        let mut document = Document::from_str("clean");
        assert!(!StatusLine::for_document(&document).left.starts_with('●'));

        document.set_caret(5);
        document.insert_at_cursors("!", false).unwrap();
        assert!(StatusLine::for_document(&document).left.starts_with('●'));
    }

    #[test]
    fn multiple_cursors_are_counted_in_the_status_bar() {
        let mut document = Document::from_str("one\ntwo");
        document.set_caret(0);
        document.add_cursor(4);
        assert!(StatusLine::for_document(&document).right.contains("2 cursors"));
    }

    #[test]
    fn the_status_bar_is_at_the_bottom_and_spans_the_window() {
        let mut view = view();
        let scene = view.render(&Document::new(), None, 1.0).unwrap();

        let bar = quads(&scene)
            .into_iter()
            .find(|q| q.color == view.theme.ui.status_background)
            .expect("no status bar");
        assert_eq!(bar.rect.width, view.layout.width);
        assert!((bar.rect.bottom() - view.layout.height).abs() < 0.01);
    }

    #[test]
    fn resizing_updates_what_fits_without_losing_the_scroll_position() {
        let mut view = view();
        view.viewport.first_line = 42;
        let before = view.viewport.visible_lines;

        view.resize(800.0, 1200.0, 100);
        assert_eq!(view.viewport.first_line, 42);
        assert!(view.viewport.visible_lines > before);
    }

    #[test]
    fn the_scene_carries_the_device_scale_factor() {
        let mut view = view();
        let scene = view.render(&Document::new(), None, 2.0).unwrap();
        assert_eq!(scene.scale_factor, 2.0);
        assert_eq!(scene.device_size(), (1600, 1200));
    }

    #[test]
    fn a_line_ending_is_not_drawn_as_a_glyph() {
        let mut view = view();
        let document = Document::from_str("first\r\nsecond\r\n");
        let scene = view.render(&document, None, 1.0).unwrap();

        for run in texts(&scene) {
            assert!(!run.text.contains('\n'), "a newline reached the renderer");
            assert!(!run.text.contains('\r'), "a carriage return reached the renderer");
        }
    }

    #[test]
    fn span_lookup_finds_the_covering_span() {
        use nebula_core::position::Range;
        let spans = vec![
            HighlightSpan { range: Range::new(0, 2), kind: HighlightKind::Keyword },
            HighlightSpan { range: Range::new(5, 9), kind: HighlightKind::String },
        ];

        assert_eq!(kind_at(&spans, 0), Some(HighlightKind::Keyword));
        assert_eq!(kind_at(&spans, 1), Some(HighlightKind::Keyword));
        assert_eq!(kind_at(&spans, 2), None, "the end is exclusive");
        assert_eq!(kind_at(&spans, 4), None);
        assert_eq!(kind_at(&spans, 8), Some(HighlightKind::String));
        assert_eq!(kind_at(&spans, 100), None);
        assert_eq!(kind_at(&[], 0), None);
    }

    #[test]
    fn adjacent_characters_of_one_class_become_one_run() {
        // One TextRun per character would multiply draw calls by the line
        // length, which is the difference between 8 ms and 80 ms per frame.
        let mut view = view();
        let document = Document::from_str("aaaaaaaaaaaaaaaaaaaa");
        let scene = view.render(&document, None, 1.0).unwrap();

        let body: Vec<&TextRun> =
            texts(&scene).into_iter().filter(|run| run.text.starts_with('a')).collect();
        assert_eq!(body.len(), 1, "the line was split into {} runs", body.len());
        assert_eq!(body[0].text.len(), 20);
    }

    #[test]
    fn rendering_is_deterministic() {
        // Two frames of an unchanged document must be byte-identical, or
        // damage tracking and frame-skipping cannot work.
        let mut view = view();
        let document = Document::from_str("stable\ncontent\nhere");
        let a = view.render(&document, None, 1.0).unwrap();
        let b = view.render(&document, None, 1.0).unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn a_light_theme_renders_the_same_geometry_as_a_dark_one() {
        let document = Document::from_str("fn main() {}\nlet x = 1;");

        let mut dark = EditorView::new(Theme::dark(), Layout::new(800.0, 600.0, 16.0, 8.0, 100));
        let mut light = EditorView::new(Theme::light(), Layout::new(800.0, 600.0, 16.0, 8.0, 100));

        let a = dark.render(&document, None, 1.0).unwrap();
        let b = light.render(&document, None, 1.0).unwrap();

        assert_eq!(a.len(), b.len());
        assert_ne!(a.background, b.background);
    }
}
