//! Viewport arithmetic.
//!
//! The one property everything here exists to preserve: the cost of drawing a
//! frame depends on the size of the window, never on the size of the file. A
//! viewport over a 200 000-line document computes the same forty visible lines
//! as a viewport over a forty-line one.

use nebula_core::TextBuffer;
use serde::{Deserialize, Serialize};

/// What part of a document is on screen.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Viewport {
    /// First visible line, zero-based.
    pub first_line: usize,
    /// Horizontal scroll, in characters.
    pub first_column: usize,
    /// Height in whole lines.
    pub visible_lines: usize,
    /// Width in whole characters.
    pub visible_columns: usize,
}

impl Default for Viewport {
    fn default() -> Self {
        Self { first_line: 0, first_column: 0, visible_lines: 40, visible_columns: 120 }
    }
}

impl Viewport {
    /// A viewport of the given size, scrolled to the top.
    pub fn new(visible_lines: usize, visible_columns: usize) -> Self {
        Self { first_line: 0, first_column: 0, visible_lines, visible_columns }
    }

    /// One past the last visible line.
    pub fn last_line(&self) -> usize {
        self.first_line + self.visible_lines
    }

    /// Whether `line` is on screen.
    pub fn contains_line(&self, line: usize) -> bool {
        line >= self.first_line && line < self.last_line()
    }

    /// The visible line range, clamped to the document.
    pub fn visible_range(&self, buffer: &TextBuffer) -> std::ops::Range<usize> {
        let total = buffer.len_lines();
        let start = self.first_line.min(total.saturating_sub(1));
        let end = (start + self.visible_lines).min(total);
        start..end
    }

    /// Scroll so `line` is visible, moving as little as possible.
    ///
    /// Minimal movement matters: scrolling to centre on every cursor move makes
    /// the text jump around while someone is reading it.
    pub fn scroll_to_line(&mut self, line: usize, margin: usize) {
        // A margin larger than half the viewport would fight itself.
        let margin = margin.min(self.visible_lines / 2);

        if line < self.first_line + margin {
            self.first_line = line.saturating_sub(margin);
        } else if line + margin >= self.last_line() {
            self.first_line = (line + margin + 1).saturating_sub(self.visible_lines);
        }
    }

    /// Scroll so `column` is visible.
    pub fn scroll_to_column(&mut self, column: usize, margin: usize) {
        let margin = margin.min(self.visible_columns / 2);

        if column < self.first_column + margin {
            self.first_column = column.saturating_sub(margin);
        } else if column + margin >= self.first_column + self.visible_columns {
            self.first_column = (column + margin + 1).saturating_sub(self.visible_columns);
        }
    }

    /// Scroll by a number of lines, clamped to the document.
    pub fn scroll_by(&mut self, delta: isize, total_lines: usize) {
        let target = self.first_line as isize + delta;
        // Leave at least one line visible: scrolling into empty space below the
        // end of a file is disorienting.
        let max_first = total_lines.saturating_sub(1);
        self.first_line = target.clamp(0, max_first as isize) as usize;
    }

    /// Centre the viewport on `line`.
    pub fn center_on(&mut self, line: usize, total_lines: usize) {
        let half = self.visible_lines / 2;
        let max_first = total_lines.saturating_sub(1);
        self.first_line = line.saturating_sub(half).min(max_first);
    }
}

/// Where the parts of the window are.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Layout {
    /// Window width in logical pixels.
    pub width: f32,
    /// Window height in logical pixels.
    pub height: f32,
    /// Width of the line-number gutter.
    pub gutter_width: f32,
    /// Height of the status bar.
    pub status_height: f32,
    /// Height of one line of text.
    pub line_height: f32,
    /// Advance width of one character.
    pub char_width: f32,
}

impl Layout {
    /// Compute a layout for a window of the given size.
    pub fn new(width: f32, height: f32, line_height: f32, char_width: f32, total_lines: usize) -> Self {
        // The gutter is sized to the widest line number the file can produce,
        // so it does not resize as the user scrolls past line 999.
        let digits = total_lines.max(1).to_string().len().max(3);
        let gutter_width = char_width * (digits as f32 + 2.0);

        Self {
            width,
            height,
            gutter_width,
            status_height: line_height + 8.0,
            line_height,
            char_width,
        }
    }

    /// The rectangle text is drawn in.
    pub fn text_area(&self) -> nebula_render::Rect {
        nebula_render::Rect::new(
            self.gutter_width,
            0.0,
            (self.width - self.gutter_width).max(0.0),
            (self.height - self.status_height).max(0.0),
        )
    }

    /// The rectangle line numbers are drawn in.
    pub fn gutter_area(&self) -> nebula_render::Rect {
        nebula_render::Rect::new(
            0.0,
            0.0,
            self.gutter_width,
            (self.height - self.status_height).max(0.0),
        )
    }

    /// The rectangle the status bar occupies.
    pub fn status_area(&self) -> nebula_render::Rect {
        nebula_render::Rect::new(
            0.0,
            (self.height - self.status_height).max(0.0),
            self.width,
            self.status_height,
        )
    }

    /// How many whole lines fit.
    pub fn visible_lines(&self) -> usize {
        if self.line_height <= 0.0 {
            return 0;
        }
        ((self.height - self.status_height) / self.line_height).floor().max(0.0) as usize
    }

    /// How many whole characters fit.
    pub fn visible_columns(&self) -> usize {
        if self.char_width <= 0.0 {
            return 0;
        }
        ((self.width - self.gutter_width) / self.char_width).floor().max(0.0) as usize
    }

    /// A viewport matching this layout.
    pub fn viewport(&self) -> Viewport {
        Viewport::new(self.visible_lines(), self.visible_columns())
    }

    /// Where a document position lands on screen.
    pub fn position_of(&self, line: usize, column: usize, viewport: &Viewport) -> nebula_render::Point {
        nebula_render::Point::new(
            self.gutter_width + (column.saturating_sub(viewport.first_column)) as f32 * self.char_width,
            (line.saturating_sub(viewport.first_line)) as f32 * self.line_height,
        )
    }

    /// Which document position a click lands on.
    ///
    /// Saturating rather than clamping to the viewport: a click in the gutter
    /// should place the caret at the start of that line, not be ignored.
    pub fn position_at(&self, x: f32, y: f32, viewport: &Viewport) -> (usize, usize) {
        let line = viewport.first_line + (y / self.line_height).max(0.0) as usize;
        let column = if x <= self.gutter_width {
            viewport.first_column
        } else {
            viewport.first_column + ((x - self.gutter_width) / self.char_width).max(0.0).round() as usize
        };
        (line, column)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layout() -> Layout {
        Layout::new(1000.0, 600.0, 16.0, 8.0, 1000)
    }

    #[test]
    fn a_viewport_covers_only_what_fits() {
        let layout = layout();
        // 600 - status (24) = 576, at 16px per line.
        assert_eq!(layout.visible_lines(), 36);
        assert!(layout.visible_columns() > 100);
    }

    #[test]
    fn the_visible_range_is_clamped_to_the_document() {
        let buffer = TextBuffer::from_str(&"line\n".repeat(10));
        let mut viewport = Viewport::new(40, 100);

        assert_eq!(viewport.visible_range(&buffer), 0..11);

        // Scrolled past the end, the range must not run off the document.
        viewport.first_line = 1000;
        let range = viewport.visible_range(&buffer);
        assert!(range.end <= buffer.len_lines());
        assert!(range.start < buffer.len_lines());
    }

    #[test]
    fn the_visible_range_does_not_grow_with_the_document() {
        // The property the whole viewport exists for.
        let small = TextBuffer::from_str(&"line\n".repeat(50));
        let huge = TextBuffer::from_str(&"line\n".repeat(200_000));
        let viewport = Viewport::new(40, 100);

        assert_eq!(viewport.visible_range(&small).len(), 40);
        assert_eq!(
            viewport.visible_range(&huge).len(),
            40,
            "a 200 000-line file must not cost more to display"
        );
    }

    #[test]
    fn scrolling_to_a_visible_line_does_not_move_the_viewport() {
        // Scrolling on every cursor move makes text jump while being read.
        let mut viewport = Viewport::new(40, 100);
        viewport.first_line = 100;

        viewport.scroll_to_line(120, 3);
        assert_eq!(viewport.first_line, 100);
    }

    #[test]
    fn scrolling_up_moves_the_minimum_needed() {
        let mut viewport = Viewport::new(40, 100);
        viewport.first_line = 100;

        viewport.scroll_to_line(98, 3);
        assert_eq!(viewport.first_line, 95, "the line lands `margin` from the top");
    }

    #[test]
    fn scrolling_down_moves_the_minimum_needed() {
        let mut viewport = Viewport::new(40, 100);
        viewport.first_line = 100;

        viewport.scroll_to_line(140, 3);
        assert!(viewport.contains_line(140));
        assert!(
            viewport.first_line <= 104,
            "moved further than needed: first_line is {}",
            viewport.first_line
        );
    }

    #[test]
    fn scrolling_to_the_first_line_does_not_underflow() {
        let mut viewport = Viewport::new(40, 100);
        viewport.scroll_to_line(0, 5);
        assert_eq!(viewport.first_line, 0);
    }

    #[test]
    fn an_oversized_margin_does_not_fight_itself() {
        let mut viewport = Viewport::new(10, 100);
        viewport.first_line = 50;
        // A margin bigger than the viewport would place the line both above and
        // below the visible region.
        viewport.scroll_to_line(55, 100);
        assert!(viewport.contains_line(55), "first_line is {}", viewport.first_line);
    }

    #[test]
    fn horizontal_scrolling_follows_the_cursor() {
        let mut viewport = Viewport::new(40, 80);
        viewport.scroll_to_column(200, 5);

        assert!(viewport.first_column > 0);
        assert!(200 >= viewport.first_column && 200 < viewport.first_column + 80);
    }

    #[test]
    fn scrolling_stops_at_the_document_boundaries() {
        let mut viewport = Viewport::new(40, 100);

        viewport.scroll_by(-100, 1000);
        assert_eq!(viewport.first_line, 0, "scrolling above the first line is meaningless");

        viewport.scroll_by(100_000, 1000);
        assert!(viewport.first_line < 1000, "scrolled past the end of the document");
    }

    #[test]
    fn centring_puts_the_line_in_the_middle() {
        let mut viewport = Viewport::new(40, 100);
        viewport.center_on(500, 1000);
        assert_eq!(viewport.first_line, 480);
        assert!(viewport.contains_line(500));
    }

    #[test]
    fn the_gutter_is_sized_for_the_longest_line_number() {
        // A gutter that resizes as you scroll past line 999 makes the text jump.
        let small = Layout::new(1000.0, 600.0, 16.0, 8.0, 50);
        let large = Layout::new(1000.0, 600.0, 16.0, 8.0, 200_000);

        assert!(large.gutter_width > small.gutter_width);
        assert!(small.gutter_width >= 8.0 * 5.0, "a minimum of three digits plus padding");
    }

    #[test]
    fn the_areas_tile_the_window_without_overlapping() {
        let layout = layout();
        let gutter = layout.gutter_area();
        let text = layout.text_area();
        let status = layout.status_area();

        assert_eq!(gutter.right(), text.x, "the gutter and text must be adjacent");
        assert_eq!(text.bottom(), status.y, "the text and status bar must be adjacent");
        assert_eq!(status.bottom(), layout.height);
        assert_eq!(text.right(), layout.width);
    }

    #[test]
    fn a_window_smaller_than_its_furniture_produces_no_negative_areas() {
        let layout = Layout::new(10.0, 10.0, 16.0, 8.0, 1000);

        for area in [layout.text_area(), layout.gutter_area(), layout.status_area()] {
            assert!(area.width >= 0.0 && area.height >= 0.0, "{area:?}");
        }
        assert_eq!(layout.visible_lines(), 0);
    }

    #[test]
    fn screen_positions_account_for_scrolling() {
        let layout = layout();
        let mut viewport = layout.viewport();
        viewport.first_line = 100;

        let point = layout.position_of(100, 0, &viewport);
        assert_eq!(point.y, 0.0, "the first visible line is at the top");
        assert_eq!(point.x, layout.gutter_width);

        let point = layout.position_of(105, 4, &viewport);
        assert_eq!(point.y, 5.0 * 16.0);
        assert_eq!(point.x, layout.gutter_width + 4.0 * 8.0);
    }

    #[test]
    fn clicks_resolve_to_document_positions() {
        let layout = layout();
        let mut viewport = layout.viewport();
        viewport.first_line = 100;

        let (line, column) = layout.position_at(layout.gutter_width, 0.0, &viewport);
        assert_eq!((line, column), (100, 0));

        let (line, _) = layout.position_at(layout.gutter_width, 5.0 * 16.0, &viewport);
        assert_eq!(line, 105);
    }

    #[test]
    fn a_click_in_the_gutter_goes_to_the_start_of_the_line() {
        let layout = layout();
        let viewport = layout.viewport();

        let (line, column) = layout.position_at(2.0, 32.0, &viewport);
        assert_eq!((line, column), (2, 0), "a gutter click should select the line, not be ignored");
    }

    #[test]
    fn screen_and_document_positions_round_trip() {
        let layout = layout();
        let viewport = layout.viewport();

        for (line, column) in [(0, 0), (5, 10), (35, 100)] {
            let point = layout.position_of(line, column, &viewport);
            assert_eq!(layout.position_at(point.x, point.y, &viewport), (line, column));
        }
    }
}
