//! The scene: what to draw, independent of how.

use serde::{Deserialize, Serialize};

/// A point in logical pixels.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct Point {
    /// Horizontal.
    pub x: f32,
    /// Vertical.
    pub y: f32,
}

impl Point {
    /// A point.
    pub const fn new(x: f32, y: f32) -> Self {
        Self { x, y }
    }
}

/// An axis-aligned rectangle in logical pixels.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct Rect {
    /// Left edge.
    pub x: f32,
    /// Top edge.
    pub y: f32,
    /// Width.
    pub width: f32,
    /// Height.
    pub height: f32,
}

impl Rect {
    /// A rectangle.
    pub const fn new(x: f32, y: f32, width: f32, height: f32) -> Self {
        Self { x, y, width, height }
    }

    /// A rectangle from two corners, in either order.
    pub fn from_corners(a: Point, b: Point) -> Self {
        let x = a.x.min(b.x);
        let y = a.y.min(b.y);
        Self { x, y, width: (b.x - a.x).abs(), height: (b.y - a.y).abs() }
    }

    /// Right edge.
    pub fn right(&self) -> f32 {
        self.x + self.width
    }

    /// Bottom edge.
    pub fn bottom(&self) -> f32 {
        self.y + self.height
    }

    /// Whether the rectangle covers no area.
    pub fn is_empty(&self) -> bool {
        self.width <= 0.0 || self.height <= 0.0
    }

    /// Whether `point` is inside.
    pub fn contains(&self, point: Point) -> bool {
        point.x >= self.x && point.x < self.right() && point.y >= self.y && point.y < self.bottom()
    }

    /// Whether two rectangles overlap.
    pub fn intersects(&self, other: &Rect) -> bool {
        self.x < other.right()
            && other.x < self.right()
            && self.y < other.bottom()
            && other.y < self.bottom()
    }

    /// The overlapping region, if any.
    pub fn intersection(&self, other: &Rect) -> Option<Rect> {
        let x = self.x.max(other.x);
        let y = self.y.max(other.y);
        let right = self.right().min(other.right());
        let bottom = self.bottom().min(other.bottom());
        (right > x && bottom > y).then(|| Rect::new(x, y, right - x, bottom - y))
    }

    /// The smallest rectangle covering both.
    pub fn union(&self, other: &Rect) -> Rect {
        if self.is_empty() {
            return *other;
        }
        if other.is_empty() {
            return *self;
        }
        let x = self.x.min(other.x);
        let y = self.y.min(other.y);
        Rect::new(x, y, self.right().max(other.right()) - x, self.bottom().max(other.bottom()) - y)
    }
}

/// A colour, straight (non-premultiplied) alpha, 8 bits per channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Color {
    /// Red.
    pub r: u8,
    /// Green.
    pub g: u8,
    /// Blue.
    pub b: u8,
    /// Alpha, 255 being opaque.
    pub a: u8,
}

impl Color {
    /// Opaque black.
    pub const BLACK: Color = Color { r: 0, g: 0, b: 0, a: 255 };
    /// Opaque white.
    pub const WHITE: Color = Color { r: 255, g: 255, b: 255, a: 255 };
    /// Fully transparent.
    pub const TRANSPARENT: Color = Color { r: 0, g: 0, b: 0, a: 0 };

    /// An opaque colour.
    pub const fn rgb(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b, a: 255 }
    }

    /// A colour with alpha.
    pub const fn rgba(r: u8, g: u8, b: u8, a: u8) -> Self {
        Self { r, g, b, a }
    }

    /// Parse `#rgb`, `#rrggbb` or `#rrggbbaa`.
    ///
    /// Themes are written by hand, so accepting the shorthand forms is worth the
    /// few lines.
    pub fn from_hex(hex: &str) -> Option<Color> {
        let hex = hex.strip_prefix('#').unwrap_or(hex);
        let parse = |s: &str| u8::from_str_radix(s, 16).ok();

        match hex.len() {
            3 => {
                let expand = |c: char| parse(&format!("{c}{c}"));
                let mut chars = hex.chars();
                Some(Color::rgb(
                    expand(chars.next()?)?,
                    expand(chars.next()?)?,
                    expand(chars.next()?)?,
                ))
            }
            6 => Some(Color::rgb(parse(&hex[0..2])?, parse(&hex[2..4])?, parse(&hex[4..6])?)),
            8 => Some(Color::rgba(
                parse(&hex[0..2])?,
                parse(&hex[2..4])?,
                parse(&hex[4..6])?,
                parse(&hex[6..8])?,
            )),
            _ => None,
        }
    }

    /// Whether this colour is fully transparent.
    pub const fn is_transparent(&self) -> bool {
        self.a == 0
    }

    /// The colour as linear-space floats, for the GPU.
    ///
    /// The conversion matters: blending 8-bit sRGB values as if they were linear
    /// produces visibly wrong intermediate colours, most noticeably in
    /// antialiased text edges.
    pub fn to_linear(self) -> [f32; 4] {
        fn channel(value: u8) -> f32 {
            let value = value as f32 / 255.0;
            if value <= 0.04045 { value / 12.92 } else { ((value + 0.055) / 1.055).powf(2.4) }
        }
        [channel(self.r), channel(self.g), channel(self.b), self.a as f32 / 255.0]
    }
}

/// A filled rectangle.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Quad {
    /// Where.
    pub rect: Rect,
    /// What colour.
    pub color: Color,
    /// Corner radius, in logical pixels.
    pub corner_radius: f32,
}

impl Quad {
    /// A square-cornered quad.
    pub fn new(rect: Rect, color: Color) -> Self {
        Self { rect, color, corner_radius: 0.0 }
    }

    /// A rounded quad.
    pub fn rounded(rect: Rect, color: Color, radius: f32) -> Self {
        Self { rect, color, corner_radius: radius }
    }
}

/// A run of text sharing one style.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TextRun {
    /// Where the run's baseline starts.
    pub origin: Point,
    /// The text.
    pub text: String,
    /// Colour.
    pub color: Color,
    /// Font size in logical pixels.
    pub size: f32,
    /// Whether to use the bold face.
    pub bold: bool,
    /// Whether to use the italic face.
    pub italic: bool,
}

impl TextRun {
    /// A plain run.
    pub fn new(origin: Point, text: impl Into<String>, color: Color, size: f32) -> Self {
        Self { origin, text: text.into(), color, size, bold: false, italic: false }
    }
}

/// One thing to draw.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Primitive {
    /// A filled rectangle.
    Quad(Quad),
    /// A run of text.
    Text(TextRun),
    /// Restrict subsequent drawing to this rectangle.
    PushClip(Rect),
    /// Undo the most recent clip.
    PopClip,
}

/// A frame to draw.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Scene {
    /// The primitives, in paint order.
    pub primitives: Vec<Primitive>,
    /// The background, painted before anything else.
    pub background: Color,
    /// Logical size.
    pub width: f32,
    /// Logical size.
    pub height: f32,
    /// Device pixels per logical pixel.
    pub scale_factor: f32,
}

impl Scene {
    /// An empty scene of the given logical size.
    pub fn new(width: f32, height: f32, scale_factor: f32) -> Self {
        Self {
            primitives: Vec::new(),
            background: Color::BLACK,
            width,
            height,
            // A zero or negative scale would produce a zero-sized buffer.
            scale_factor: if scale_factor > 0.0 { scale_factor } else { 1.0 },
        }
    }

    /// Set the background.
    pub fn background(mut self, color: Color) -> Self {
        self.background = color;
        self
    }

    /// Add a filled rectangle.
    pub fn quad(&mut self, quad: Quad) -> &mut Self {
        // A quad with no area or no opacity costs a draw call and changes
        // nothing; dropping it here keeps every backend from having to.
        if !quad.rect.is_empty() && !quad.color.is_transparent() {
            self.primitives.push(Primitive::Quad(quad));
        }
        self
    }

    /// Add a run of text.
    pub fn text(&mut self, run: TextRun) -> &mut Self {
        if !run.text.is_empty() && !run.color.is_transparent() && run.size > 0.0 {
            self.primitives.push(Primitive::Text(run));
        }
        self
    }

    /// Restrict subsequent drawing to `rect`.
    pub fn push_clip(&mut self, rect: Rect) -> &mut Self {
        self.primitives.push(Primitive::PushClip(rect));
        self
    }

    /// Undo the most recent clip.
    pub fn pop_clip(&mut self) -> &mut Self {
        self.primitives.push(Primitive::PopClip);
        self
    }

    /// Number of primitives.
    pub fn len(&self) -> usize {
        self.primitives.len()
    }

    /// Whether nothing will be drawn.
    pub fn is_empty(&self) -> bool {
        self.primitives.is_empty()
    }

    /// Size in device pixels.
    pub fn device_size(&self) -> (u32, u32) {
        (
            (self.width * self.scale_factor).round().max(1.0) as u32,
            (self.height * self.scale_factor).round().max(1.0) as u32,
        )
    }

    /// Whether every `PushClip` has a matching `PopClip`.
    ///
    /// An unbalanced scene draws correctly in some backends and not others, so
    /// it is caught here rather than becoming a rendering difference.
    pub fn clips_balanced(&self) -> bool {
        let mut depth = 0i32;
        for primitive in &self.primitives {
            match primitive {
                Primitive::PushClip(_) => depth += 1,
                Primitive::PopClip => {
                    depth -= 1;
                    if depth < 0 {
                        return false;
                    }
                }
                _ => {}
            }
        }
        depth == 0
    }

    /// The smallest rectangle covering everything drawn.
    ///
    /// This is what damage tracking uses: a frame that changed one line does not
    /// need the whole window recomposited.
    pub fn bounds(&self) -> Rect {
        let mut bounds = Rect::default();
        for primitive in &self.primitives {
            let rect = match primitive {
                Primitive::Quad(quad) => quad.rect,
                Primitive::Text(run) => Rect::new(
                    run.origin.x,
                    // A conservative box around the baseline: text extends above
                    // it by roughly the ascent and below by the descent.
                    run.origin.y - run.size,
                    run.text.chars().count() as f32 * run.size,
                    run.size * 1.5,
                ),
                Primitive::PushClip(rect) => *rect,
                Primitive::PopClip => continue,
            };
            bounds = bounds.union(&rect);
        }
        bounds
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rectangles_report_their_edges() {
        let rect = Rect::new(10.0, 20.0, 30.0, 40.0);
        assert_eq!(rect.right(), 40.0);
        assert_eq!(rect.bottom(), 60.0);
        assert!(!rect.is_empty());
        assert!(Rect::new(0.0, 0.0, 0.0, 10.0).is_empty());
    }

    #[test]
    fn corners_are_sorted() {
        let from_corners = Rect::from_corners(Point::new(40.0, 60.0), Point::new(10.0, 20.0));
        assert_eq!(from_corners, Rect::new(10.0, 20.0, 30.0, 40.0));
    }

    #[test]
    fn containment_is_half_open() {
        let rect = Rect::new(0.0, 0.0, 10.0, 10.0);
        assert!(rect.contains(Point::new(0.0, 0.0)));
        assert!(rect.contains(Point::new(9.9, 9.9)));
        assert!(!rect.contains(Point::new(10.0, 5.0)), "the right edge is exclusive");
    }

    #[test]
    fn intersection_and_union_behave() {
        let a = Rect::new(0.0, 0.0, 10.0, 10.0);
        let b = Rect::new(5.0, 5.0, 10.0, 10.0);

        assert!(a.intersects(&b));
        assert_eq!(a.intersection(&b), Some(Rect::new(5.0, 5.0, 5.0, 5.0)));
        assert_eq!(a.union(&b), Rect::new(0.0, 0.0, 15.0, 15.0));

        let far = Rect::new(100.0, 100.0, 5.0, 5.0);
        assert!(!a.intersects(&far));
        assert_eq!(a.intersection(&far), None);
    }

    #[test]
    fn union_with_an_empty_rectangle_is_the_other_one() {
        let rect = Rect::new(5.0, 5.0, 10.0, 10.0);
        assert_eq!(Rect::default().union(&rect), rect);
        assert_eq!(rect.union(&Rect::default()), rect);
    }

    #[test]
    fn colours_parse_from_every_hex_form() {
        assert_eq!(Color::from_hex("#ff0000"), Some(Color::rgb(255, 0, 0)));
        assert_eq!(Color::from_hex("00ff00"), Some(Color::rgb(0, 255, 0)));
        assert_eq!(Color::from_hex("#f00"), Some(Color::rgb(255, 0, 0)));
        assert_eq!(Color::from_hex("#ff000080"), Some(Color::rgba(255, 0, 0, 128)));

        assert_eq!(Color::from_hex("#ff00"), None);
        assert_eq!(Color::from_hex("not a colour"), None);
        assert_eq!(Color::from_hex(""), None);
    }

    #[test]
    fn srgb_is_converted_to_linear_for_the_gpu() {
        // Blending 8-bit sRGB as if it were linear is what makes antialiased
        // text edges look wrong.
        let [r, g, b, a] = Color::rgb(128, 128, 128).to_linear();
        assert!((r - 0.2159).abs() < 0.001, "mid grey linearises to ~0.216, got {r}");
        assert_eq!(r, g);
        assert_eq!(g, b);
        assert_eq!(a, 1.0);

        assert_eq!(Color::BLACK.to_linear(), [0.0, 0.0, 0.0, 1.0]);
        let [white, ..] = Color::WHITE.to_linear();
        assert!((white - 1.0).abs() < 1e-6);
    }

    #[test]
    fn invisible_primitives_are_dropped_before_they_reach_a_backend() {
        let mut scene = Scene::new(100.0, 100.0, 1.0);
        scene
            .quad(Quad::new(Rect::new(0.0, 0.0, 0.0, 10.0), Color::WHITE))
            .quad(Quad::new(Rect::new(0.0, 0.0, 10.0, 10.0), Color::TRANSPARENT))
            .text(TextRun::new(Point::new(0.0, 0.0), "", Color::WHITE, 14.0))
            .text(TextRun::new(Point::new(0.0, 0.0), "hidden", Color::TRANSPARENT, 14.0))
            .text(TextRun::new(Point::new(0.0, 0.0), "zero size", Color::WHITE, 0.0));

        assert!(scene.is_empty(), "invisible primitives reached the scene: {scene:?}");
    }

    #[test]
    fn visible_primitives_are_kept_in_paint_order() {
        let mut scene = Scene::new(100.0, 100.0, 1.0);
        scene.quad(Quad::new(Rect::new(0.0, 0.0, 10.0, 10.0), Color::WHITE)).text(TextRun::new(
            Point::new(1.0, 8.0),
            "hi",
            Color::BLACK,
            12.0,
        ));

        assert_eq!(scene.len(), 2);
        assert!(matches!(scene.primitives[0], Primitive::Quad(_)));
        assert!(matches!(scene.primitives[1], Primitive::Text(_)));
    }

    #[test]
    fn device_size_accounts_for_the_scale_factor() {
        assert_eq!(Scene::new(800.0, 600.0, 1.0).device_size(), (800, 600));
        assert_eq!(Scene::new(800.0, 600.0, 2.0).device_size(), (1600, 1200));
        assert_eq!(Scene::new(800.0, 600.0, 1.5).device_size(), (1200, 900));
    }

    #[test]
    fn an_invalid_scale_factor_falls_back_to_one() {
        // A zero scale would produce a zero-sized buffer and a panic downstream.
        assert_eq!(Scene::new(100.0, 100.0, 0.0).scale_factor, 1.0);
        assert_eq!(Scene::new(100.0, 100.0, -2.0).scale_factor, 1.0);
    }

    #[test]
    fn clip_balance_is_checked() {
        let mut balanced = Scene::new(100.0, 100.0, 1.0);
        balanced.push_clip(Rect::new(0.0, 0.0, 50.0, 50.0)).pop_clip();
        assert!(balanced.clips_balanced());

        let mut unclosed = Scene::new(100.0, 100.0, 1.0);
        unclosed.push_clip(Rect::new(0.0, 0.0, 50.0, 50.0));
        assert!(!unclosed.clips_balanced());

        let mut over_popped = Scene::new(100.0, 100.0, 1.0);
        over_popped.pop_clip();
        assert!(!over_popped.clips_balanced());
    }

    #[test]
    fn bounds_cover_everything_drawn() {
        let mut scene = Scene::new(1000.0, 1000.0, 1.0);
        scene
            .quad(Quad::new(Rect::new(10.0, 10.0, 20.0, 20.0), Color::WHITE))
            .quad(Quad::new(Rect::new(100.0, 200.0, 50.0, 10.0), Color::WHITE));

        let bounds = scene.bounds();
        assert!(bounds.x <= 10.0);
        assert!(bounds.y <= 10.0);
        assert!(bounds.right() >= 150.0);
        assert!(bounds.bottom() >= 210.0);
    }

    #[test]
    fn an_empty_scene_has_empty_bounds() {
        assert!(Scene::new(100.0, 100.0, 1.0).bounds().is_empty());
    }

    #[test]
    fn scenes_round_trip_through_serde() {
        // Serialisable so a frame can be captured from a bug report and replayed.
        let mut scene = Scene::new(80.0, 60.0, 2.0).background(Color::rgb(30, 30, 40));
        scene
            .push_clip(Rect::new(0.0, 0.0, 40.0, 30.0))
            .quad(Quad::rounded(Rect::new(1.0, 1.0, 10.0, 10.0), Color::WHITE, 2.0))
            .text(TextRun::new(Point::new(2.0, 12.0), "text", Color::BLACK, 11.0))
            .pop_clip();

        let json = serde_json::to_string(&scene).unwrap();
        assert_eq!(serde_json::from_str::<Scene>(&json).unwrap(), scene);
    }
}
