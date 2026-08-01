//! Text shaping and rasterisation.
//!
//! Built on `cosmic-text`, which does the parts of text rendering that are
//! genuinely hard: font fallback, complex-script shaping, and bidirectional
//! runs. Writing those from scratch would be a mistake in any editor that
//! intends to display a file containing Arabic, Devanagari or an emoji.
//!
//! ## Why the default font is bundled
//!
//! The editor ships DejaVu Sans Mono rather than asking the system for "a
//! monospace font". Two reasons, both practical:
//!
//! * A container, a minimal VM or a fresh CI runner may have no fonts at all,
//!   and an editor that renders nothing there is broken.
//! * Rendering is then identical on every machine, which is what makes the
//!   pixel assertions in the test suite meaningful rather than
//!   environment-dependent.
//!
//! System fonts are still loaded and still used for fallback, so a file
//! containing scripts DejaVu does not cover renders correctly wherever the
//! system can supply a face for them.

use cosmic_text::{
    Attrs, Buffer, Family, FontSystem as CosmicFontSystem, Metrics, Shaping, SwashCache, Weight,
};
use parking_lot::Mutex;
use tiny_skia::Pixmap;

use crate::scene::{Rect, TextRun};
use crate::Result;

/// The bundled regular face.
pub const BUNDLED_REGULAR: &[u8] = include_bytes!("../assets/fonts/DejaVuSansMono.ttf");

/// The bundled bold face.
pub const BUNDLED_BOLD: &[u8] = include_bytes!("../assets/fonts/DejaVuSansMono-Bold.ttf");

/// The family name of the bundled font.
pub const BUNDLED_FAMILY: &str = "DejaVu Sans Mono";

/// A shaped line, with the measurements the editor needs for hit-testing.
#[derive(Debug, Clone, PartialEq)]
pub struct ShapedLine {
    /// Total advance width in logical pixels.
    pub width: f32,
    /// Height of one line.
    pub height: f32,
    /// The x offset of each character boundary, for placing a cursor.
    pub boundaries: Vec<f32>,
}

impl ShapedLine {
    /// The character index nearest to `x`.
    ///
    /// This is what a click resolves to. It rounds to the nearest boundary
    /// rather than the preceding one, so clicking the right half of a character
    /// puts the caret after it — which is what every editor does and what users
    /// expect without being able to articulate.
    pub fn index_at(&self, x: f32) -> usize {
        if self.boundaries.is_empty() {
            return 0;
        }
        let mut best = 0usize;
        let mut best_distance = f32::MAX;
        for (index, boundary) in self.boundaries.iter().enumerate() {
            let distance = (boundary - x).abs();
            if distance < best_distance {
                best_distance = distance;
                best = index;
            }
        }
        best
    }

    /// The x offset of character `index`.
    pub fn offset_of(&self, index: usize) -> f32 {
        self.boundaries.get(index).copied().unwrap_or(self.width)
    }
}

/// Loads fonts, shapes text and rasterises glyphs.
pub struct FontSystem {
    inner: Mutex<Inner>,
}

struct Inner {
    fonts: CosmicFontSystem,
    cache: SwashCache,
}

impl Default for FontSystem {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for FontSystem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("FontSystem")
    }
}

impl FontSystem {
    /// Load the bundled faces plus whatever the system provides.
    pub fn new() -> Self {
        // System fonts first, so fallback for scripts the bundled font does not
        // cover works wherever the system can supply a face.
        let mut fonts = CosmicFontSystem::new();
        fonts.db_mut().load_font_data(BUNDLED_REGULAR.to_vec());
        fonts.db_mut().load_font_data(BUNDLED_BOLD.to_vec());

        Self { inner: Mutex::new(Inner { fonts, cache: SwashCache::new() }) }
    }

    /// A system that uses only the bundled faces.
    ///
    /// Used where reproducibility matters more than script coverage.
    pub fn bundled_only() -> Self {
        let mut fonts = CosmicFontSystem::new_with_locale_and_db(
            "en-US".to_string(),
            cosmic_text::fontdb::Database::new(),
        );
        fonts.db_mut().load_font_data(BUNDLED_REGULAR.to_vec());
        fonts.db_mut().load_font_data(BUNDLED_BOLD.to_vec());

        Self { inner: Mutex::new(Inner { fonts, cache: SwashCache::new() }) }
    }

    /// How many faces are available.
    pub fn face_count(&self) -> usize {
        self.inner.lock().fonts.db().len()
    }

    /// Measure a line of text.
    pub fn shape(&self, text: &str, size: f32, bold: bool) -> ShapedLine {
        let mut inner = self.inner.lock();
        let Inner { fonts, .. } = &mut *inner;

        let metrics = Metrics::new(size, size * 1.4);
        let mut buffer = Buffer::new(fonts, metrics);
        let mut buffer = buffer.borrow_with(fonts);

        buffer.set_text(text, &attrs(bold), Shaping::Advanced, None);
        buffer.shape_until_scroll(true);

        let mut width = 0.0f32;
        let mut boundaries = vec![0.0f32];

        for run in buffer.layout_runs() {
            for glyph in run.glyphs.iter() {
                width = width.max(glyph.x + glyph.w);
                boundaries.push(glyph.x + glyph.w);
            }
        }
        boundaries.dedup_by(|a, b| (*a - *b).abs() < f32::EPSILON);

        ShapedLine { width, height: metrics.line_height, boundaries }
    }

    /// The advance width of one character at `size`, for a monospace layout.
    ///
    /// The editor's whole layout depends on this being constant, so it is
    /// measured once rather than assumed.
    pub fn advance_width(&self, size: f32) -> f32 {
        let shaped = self.shape("M", size, false);
        if shaped.width > 0.0 { shaped.width } else { size * 0.6 }
    }

    /// Draw a text run into `pixmap`.
    pub fn draw_run(
        &self,
        pixmap: &mut Pixmap,
        run: &TextRun,
        scale: f32,
        clip: Option<Rect>,
    ) -> Result<()> {
        let mut inner = self.inner.lock();
        let Inner { fonts, cache } = &mut *inner;

        let size = run.size * scale;
        let metrics = Metrics::new(size, size * 1.4);
        let mut buffer = Buffer::new(fonts, metrics);
        let mut borrowed = buffer.borrow_with(fonts);
        borrowed.set_text(&run.text, &attrs(run.bold), Shaping::Advanced, None);
        borrowed.shape_until_scroll(true);

        let origin_x = run.origin.x * scale;
        // `origin.y` is the baseline; cosmic-text lays out from the top of the
        // line box, so the ascent has to be subtracted or every run sits one
        // line too low.
        let origin_y = run.origin.y * scale - size;

        // The clip is in logical coordinates; scale it once here rather than per
        // glyph.
        let device_clip = clip.map(|rect| Rect {
            x: rect.x * scale,
            y: rect.y * scale,
            width: rect.width * scale,
            height: rect.height * scale,
        });

        let color = cosmic_text::Color::rgba(run.color.r, run.color.g, run.color.b, run.color.a);

        // Blend coverage straight into the pixel buffer.
        //
        // cosmic-text hands back one box per covered pixel, so the obvious
        // implementation — build a `Paint` and call `fill_rect` for each — runs
        // the whole rasteriser pipeline a quarter of a million times per frame
        // on a full screen of text. Compositing by hand is a few lines of
        // integer arithmetic and is what makes the software path usable at all.
        let pixmap_width = pixmap.width() as i32;
        let pixmap_height = pixmap.height() as i32;
        let pixels = pixmap.pixels_mut();

        buffer.draw(fonts, cache, color, |x, y, w, h, pixel| {
            let alpha = pixel.a() as u32;
            if alpha == 0 {
                return;
            }

            let left = (origin_x + x as f32).floor() as i32;
            let top = (origin_y + y as f32).floor() as i32;

            for row in 0..h as i32 {
                let py = top + row;
                if py < 0 || py >= pixmap_height {
                    continue;
                }

                for column in 0..w as i32 {
                    let px = left + column;
                    if px < 0 || px >= pixmap_width {
                        continue;
                    }

                    if let Some(clip) = device_clip
                        && ((px as f32) < clip.x
                            || px as f32 >= clip.x + clip.width
                            || (py as f32) < clip.y
                            || py as f32 >= clip.y + clip.height)
                    {
                        continue;
                    }

                    let index = (py * pixmap_width + px) as usize;
                    let destination = pixels[index];

                    // Source-over, with both sides premultiplied. `+ 127` makes
                    // the integer division round rather than truncate, which
                    // otherwise darkens antialiased edges by up to a level.
                    let premultiply = |channel: u8| (channel as u32 * alpha + 127) / 255;
                    let inverse = 255 - alpha;
                    let over = |source: u32, dest: u8| -> u8 {
                        (source + (dest as u32 * inverse + 127) / 255).min(255) as u8
                    };

                    let red = over(premultiply(pixel.r()), destination.red());
                    let green = over(premultiply(pixel.g()), destination.green());
                    let blue = over(premultiply(pixel.b()), destination.blue());
                    let out_alpha =
                        (alpha + (destination.alpha() as u32 * inverse + 127) / 255).min(255) as u8;

                    // The constructor rejects colours whose channels exceed
                    // their alpha. Rounding can produce that by a single level,
                    // so the components are clamped rather than the pixel
                    // dropped.
                    pixels[index] = tiny_skia::PremultipliedColorU8::from_rgba(
                        red.min(out_alpha),
                        green.min(out_alpha),
                        blue.min(out_alpha),
                        out_alpha,
                    )
                    .unwrap_or(destination);
                }
            }
        });

        Ok(())
    }
}

fn attrs(bold: bool) -> Attrs<'static> {
    let mut attrs = Attrs::new().family(Family::Monospace);
    if bold {
        attrs = attrs.weight(Weight::BOLD);
    }
    attrs
}

/// A glyph atlas for the GPU backend.
///
/// Rasterised glyphs are packed into one texture so a line of text is a single
/// draw call rather than one per character. Uploading a texture per glyph per
/// frame would make the GPU path slower than the CPU one.
pub struct GlyphAtlas {
    allocator: etagere::AtlasAllocator,
    /// Where each rasterised glyph lives, keyed by (glyph id, size in 1/64ths).
    slots: std::collections::HashMap<(u16, u32), etagere::Rectangle>,
    width: u32,
    height: u32,
}

impl std::fmt::Debug for GlyphAtlas {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The allocator has no Debug, and its internal free list would not be
        // useful in a log anyway.
        f.debug_struct("GlyphAtlas")
            .field("size", &(self.width, self.height))
            .field("glyphs", &self.slots.len())
            .finish()
    }
}

impl GlyphAtlas {
    /// An empty atlas.
    pub fn new(width: u32, height: u32) -> Self {
        Self {
            allocator: etagere::AtlasAllocator::new(etagere::size2(width as i32, height as i32)),
            slots: std::collections::HashMap::new(),
            width,
            height,
        }
    }

    /// Reserve space for a glyph, returning where it went.
    ///
    /// `None` means the atlas is full; the caller grows it or evicts.
    pub fn allocate(
        &mut self,
        glyph: u16,
        size_subpixels: u32,
        width: u32,
        height: u32,
    ) -> Option<etagere::Rectangle> {
        let key = (glyph, size_subpixels);
        if let Some(existing) = self.slots.get(&key) {
            return Some(*existing);
        }
        // A one-pixel gutter, or bilinear sampling bleeds a neighbour's coverage
        // into a glyph's edge.
        let allocation = self
            .allocator
            .allocate(etagere::size2(width as i32 + 2, height as i32 + 2))?;
        self.slots.insert(key, allocation.rectangle);
        Some(allocation.rectangle)
    }

    /// Where a glyph is, if it has been allocated.
    pub fn lookup(&self, glyph: u16, size_subpixels: u32) -> Option<etagere::Rectangle> {
        self.slots.get(&(glyph, size_subpixels)).copied()
    }

    /// How many glyphs are resident.
    pub fn len(&self) -> usize {
        self.slots.len()
    }

    /// Whether the atlas is empty.
    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    /// Drop everything, e.g. after a font or scale change.
    pub fn clear(&mut self) {
        self.allocator.clear();
        self.slots.clear();
    }

    /// The atlas texture size.
    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scene::{Color, Point};

    #[test]
    fn the_bundled_font_is_present_and_loadable() {
        // The editor must render text on a machine with no system fonts at all.
        assert!(BUNDLED_REGULAR.len() > 100_000, "the bundled font looks truncated");
        assert_eq!(&BUNDLED_REGULAR[..4], b"\x00\x01\x00\x00", "not a TrueType file");

        let fonts = FontSystem::bundled_only();
        assert!(fonts.face_count() >= 2, "expected the regular and bold faces");
    }

    #[test]
    fn shaping_measures_text() {
        let fonts = FontSystem::bundled_only();
        let shaped = fonts.shape("hello", 14.0, false);

        assert!(shaped.width > 0.0);
        assert!(shaped.height >= 14.0);
        assert_eq!(shaped.boundaries.len(), 6, "one boundary per character, plus the start");
    }

    #[test]
    fn the_bundled_font_is_monospace() {
        // The editor's layout depends on every character having the same
        // advance; a proportional default would misalign every column.
        let fonts = FontSystem::bundled_only();
        let narrow = fonts.shape("iiii", 14.0, false).width;
        let wide = fonts.shape("MMMM", 14.0, false).width;

        assert!(
            (narrow - wide).abs() < 0.5,
            "advances differ: 'iiii' is {narrow}, 'MMMM' is {wide}"
        );
    }

    #[test]
    fn width_scales_with_the_number_of_characters() {
        let fonts = FontSystem::bundled_only();
        let one = fonts.shape("M", 14.0, false).width;
        let ten = fonts.shape(&"M".repeat(10), 14.0, false).width;

        assert!((ten - one * 10.0).abs() < 1.0, "{one} * 10 should be about {ten}");
    }

    #[test]
    fn width_scales_with_font_size() {
        let fonts = FontSystem::bundled_only();
        let small = fonts.advance_width(10.0);
        let large = fonts.advance_width(20.0);

        assert!(large > small * 1.8, "{small} at 10px vs {large} at 20px");
    }

    #[test]
    fn empty_text_measures_zero() {
        let shaped = FontSystem::bundled_only().shape("", 14.0, false);
        assert_eq!(shaped.width, 0.0);
    }

    #[test]
    fn a_click_resolves_to_the_nearest_character_boundary() {
        let fonts = FontSystem::bundled_only();
        let shaped = fonts.shape("abcdef", 14.0, false);
        let advance = fonts.advance_width(14.0);

        assert_eq!(shaped.index_at(-100.0), 0, "before the line clamps to the start");
        assert_eq!(shaped.index_at(0.0), 0);
        assert_eq!(
            shaped.index_at(advance * 3.0),
            3,
            "a click on a boundary lands on it"
        );
        assert_eq!(
            shaped.index_at(advance * 2.6),
            3,
            "the right half of a character puts the caret after it"
        );
        assert_eq!(shaped.index_at(10_000.0), shaped.boundaries.len() - 1);
    }

    #[test]
    fn character_offsets_are_monotonic() {
        let shaped = FontSystem::bundled_only().shape("monotonic", 14.0, false);
        for pair in shaped.boundaries.windows(2) {
            assert!(pair[1] >= pair[0], "boundaries went backwards: {:?}", shaped.boundaries);
        }
    }

    #[test]
    fn unicode_text_shapes_without_panicking() {
        let fonts = FontSystem::new();
        for text in ["héllo wörld", "日本語のテキスト", "مرحبا", "🌌 emoji"] {
            let shaped = fonts.shape(text, 14.0, false);
            assert!(shaped.width >= 0.0, "shaping failed for {text}");
        }
    }

    #[test]
    fn drawing_marks_the_pixmap() {
        let fonts = FontSystem::bundled_only();
        let mut pixmap = Pixmap::new(64, 24).unwrap();
        pixmap.fill(tiny_skia::Color::BLACK);

        let run = TextRun::new(Point::new(2.0, 18.0), "Nebula", Color::WHITE, 14.0);
        fonts.draw_run(&mut pixmap, &run, 1.0, None).unwrap();

        let lit = pixmap.data().chunks_exact(4).filter(|p| p[0] > 40).count();
        assert!(lit > 30, "expected glyph coverage, found {lit} lit pixels");
    }

    #[test]
    fn drawing_is_deterministic() {
        let fonts = FontSystem::bundled_only();
        let run = TextRun::new(Point::new(2.0, 18.0), "deterministic", Color::WHITE, 13.0);

        let draw = || {
            let mut pixmap = Pixmap::new(128, 24).unwrap();
            pixmap.fill(tiny_skia::Color::BLACK);
            fonts.draw_run(&mut pixmap, &run, 1.0, None).unwrap();
            pixmap.data().to_vec()
        };
        assert_eq!(draw(), draw());
    }

    #[test]
    fn a_clip_removes_coverage_outside_it() {
        let fonts = FontSystem::bundled_only();
        let run = TextRun::new(Point::new(2.0, 18.0), "clipped text here", Color::WHITE, 13.0);

        let count = |clip: Option<Rect>| {
            let mut pixmap = Pixmap::new(160, 24).unwrap();
            pixmap.fill(tiny_skia::Color::BLACK);
            fonts.draw_run(&mut pixmap, &run, 1.0, clip).unwrap();
            pixmap.data().chunks_exact(4).filter(|p| p[0] > 40).count()
        };

        let unclipped = count(None);
        let clipped = count(Some(Rect::new(0.0, 0.0, 40.0, 24.0)));
        assert!(clipped > 0, "the clip removed everything");
        assert!(clipped < unclipped, "the clip removed nothing: {clipped} vs {unclipped}");
    }

    #[test]
    fn bold_text_is_heavier_than_regular() {
        let fonts = FontSystem::bundled_only();
        let count = |bold: bool| {
            let mut pixmap = Pixmap::new(96, 24).unwrap();
            pixmap.fill(tiny_skia::Color::BLACK);
            let mut run = TextRun::new(Point::new(2.0, 18.0), "Weight", Color::WHITE, 14.0);
            run.bold = bold;
            fonts.draw_run(&mut pixmap, &run, 1.0, None).unwrap();
            pixmap.data().chunks_exact(4).map(|p| p[0] as u64).sum::<u64>()
        };

        assert!(count(true) > count(false), "the bold face should put down more ink");
    }

    #[test]
    fn the_atlas_packs_glyphs_and_remembers_them() {
        let mut atlas = GlyphAtlas::new(256, 256);
        assert!(atlas.is_empty());

        let first = atlas.allocate(42, 14 * 64, 10, 12).unwrap();
        let again = atlas.allocate(42, 14 * 64, 10, 12).unwrap();
        assert_eq!(first, again, "the same glyph must not be packed twice");
        assert_eq!(atlas.len(), 1);

        let different_size = atlas.allocate(42, 20 * 64, 14, 18).unwrap();
        assert_ne!(first, different_size, "a different size is a different raster");
        assert_eq!(atlas.len(), 2);
    }

    #[test]
    fn allocations_do_not_overlap() {
        // Overlapping slots would make one glyph draw part of another.
        let mut atlas = GlyphAtlas::new(256, 256);
        let mut placed: Vec<etagere::Rectangle> = Vec::new();

        for glyph in 0..40u16 {
            let rectangle = atlas.allocate(glyph, 14 * 64, 12, 16).unwrap();
            for existing in &placed {
                let disjoint = rectangle.max.x <= existing.min.x
                    || existing.max.x <= rectangle.min.x
                    || rectangle.max.y <= existing.min.y
                    || existing.max.y <= rectangle.min.y;
                assert!(disjoint, "glyph {glyph} overlaps an earlier allocation");
            }
            placed.push(rectangle);
        }
    }

    #[test]
    fn a_full_atlas_reports_failure_rather_than_corrupting_itself() {
        let mut atlas = GlyphAtlas::new(32, 32);
        let mut allocated = 0;
        for glyph in 0..100u16 {
            if atlas.allocate(glyph, 14 * 64, 16, 16).is_none() {
                break;
            }
            allocated += 1;
        }
        assert!(allocated < 100, "a 32x32 atlas cannot hold 100 16x16 glyphs");
    }

    #[test]
    fn clearing_the_atlas_frees_its_space() {
        let mut atlas = GlyphAtlas::new(64, 64);
        while atlas.allocate(atlas.len() as u16, 14 * 64, 16, 16).is_some() {}
        let before = atlas.len();
        assert!(before > 0);

        atlas.clear();
        assert!(atlas.is_empty());
        assert!(atlas.allocate(0, 14 * 64, 16, 16).is_some(), "space was not reclaimed");
    }

    #[test]
    fn glyph_lookup_finds_what_was_allocated() {
        let mut atlas = GlyphAtlas::new(128, 128);
        assert_eq!(atlas.lookup(7, 14 * 64), None);

        let allocated = atlas.allocate(7, 14 * 64, 8, 10).unwrap();
        assert_eq!(atlas.lookup(7, 14 * 64), Some(allocated));
        assert_eq!(atlas.lookup(7, 20 * 64), None, "size is part of the key");
    }
}
