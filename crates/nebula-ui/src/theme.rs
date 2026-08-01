//! Colours.
//!
//! A theme is a plain data file: every [`HighlightKind`] gets a colour, and so
//! does every piece of editor chrome. There is no fallback colour and no
//! "inherit from parent" rule, because both of those produce themes that look
//! right in the author's screenshot and wrong on someone else's file — a
//! missing key is a load error, not a grey rectangle.

use std::collections::BTreeMap;

use nebula_render::Color;
use nebula_syntax::HighlightKind;
use serde::{Deserialize, Serialize};

use crate::{Result, UiError};

/// The colours the editor draws with.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Theme {
    /// A human-readable name.
    pub name: String,

    /// Whether this is a dark theme. Used to pick a matching system window
    /// decoration and a matching default for anything not themed.
    pub dark: bool,

    /// Editor chrome.
    pub ui: UiColors,

    /// Syntax colours, one per [`HighlightKind`].
    ///
    /// Stored keyed by [`HighlightKind::name`] so a theme file is readable, and
    /// validated on load so a missing key cannot reach the renderer.
    pub syntax: BTreeMap<String, Color>,
}

/// Colours for everything that is not source code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct UiColors {
    /// The editing surface.
    pub background: Color,
    /// Default text, used where no highlight applies.
    pub foreground: Color,
    /// The line-number column's background.
    pub gutter_background: Color,
    /// Line numbers.
    pub gutter_foreground: Color,
    /// The line number of the line the caret is on.
    pub gutter_active: Color,
    /// The band behind the caret's line.
    pub current_line: Color,
    /// Selected text's background.
    pub selection: Color,
    /// The caret itself.
    pub cursor: Color,
    /// The status bar's background.
    pub status_background: Color,
    /// Status bar text.
    pub status_foreground: Color,
    /// Errors, in diagnostics and in the status bar.
    pub error: Color,
    /// Warnings.
    pub warning: Color,
}

impl Theme {
    /// The built-in dark theme.
    ///
    /// Shipped compiled in rather than as a file on disk: the editor has to be
    /// able to draw a frame before it has read anything, including its own
    /// configuration directory.
    pub fn dark() -> Theme {
        Theme {
            name: "Nebula Dark".to_string(),
            dark: true,
            ui: UiColors {
                background: Color::rgb(0x0d, 0x11, 0x17),
                foreground: Color::rgb(0xc9, 0xd1, 0xd9),
                gutter_background: Color::rgb(0x0d, 0x11, 0x17),
                gutter_foreground: Color::rgb(0x48, 0x4f, 0x58),
                gutter_active: Color::rgb(0xc9, 0xd1, 0xd9),
                current_line: Color::rgb(0x11, 0x16, 0x1d),
                selection: Color::rgba(0x38, 0x4b, 0x6b, 0xcc),
                cursor: Color::rgb(0x58, 0xa6, 0xff),
                status_background: Color::rgb(0x16, 0x1b, 0x22),
                status_foreground: Color::rgb(0x8b, 0x94, 0x9e),
                error: Color::rgb(0xf8, 0x51, 0x49),
                warning: Color::rgb(0xd2, 0x99, 0x22),
            },
            syntax: syntax_map(&[
                (HighlightKind::Keyword, Color::rgb(0xff, 0x7b, 0x72)),
                (HighlightKind::Function, Color::rgb(0xd2, 0xa8, 0xff)),
                (HighlightKind::Type, Color::rgb(0xff, 0xa6, 0x57)),
                (HighlightKind::Variable, Color::rgb(0xc9, 0xd1, 0xd9)),
                (HighlightKind::Constant, Color::rgb(0x79, 0xc0, 0xff)),
                (HighlightKind::String, Color::rgb(0xa5, 0xd6, 0xff)),
                (HighlightKind::Number, Color::rgb(0x79, 0xc0, 0xff)),
                (HighlightKind::Boolean, Color::rgb(0x79, 0xc0, 0xff)),
                (HighlightKind::Comment, Color::rgb(0x8b, 0x94, 0x9e)),
                (HighlightKind::Operator, Color::rgb(0xff, 0x7b, 0x72)),
                (HighlightKind::Punctuation, Color::rgb(0xc9, 0xd1, 0xd9)),
                (HighlightKind::Attribute, Color::rgb(0xd2, 0xa8, 0xff)),
                (HighlightKind::Property, Color::rgb(0x79, 0xc0, 0xff)),
                (HighlightKind::Namespace, Color::rgb(0xff, 0xa6, 0x57)),
                (HighlightKind::Escape, Color::rgb(0xff, 0xa6, 0x57)),
                (HighlightKind::Text, Color::rgb(0xc9, 0xd1, 0xd9)),
            ]),
        }
    }

    /// The built-in light theme.
    pub fn light() -> Theme {
        Theme {
            name: "Nebula Light".to_string(),
            dark: false,
            ui: UiColors {
                background: Color::rgb(0xff, 0xff, 0xff),
                foreground: Color::rgb(0x1f, 0x23, 0x28),
                gutter_background: Color::rgb(0xff, 0xff, 0xff),
                gutter_foreground: Color::rgb(0x8c, 0x95, 0x9f),
                gutter_active: Color::rgb(0x1f, 0x23, 0x28),
                current_line: Color::rgb(0xf2, 0xf5, 0xf8),
                selection: Color::rgba(0xb6, 0xd5, 0xff, 0xcc),
                cursor: Color::rgb(0x09, 0x69, 0xda),
                status_background: Color::rgb(0xf6, 0xf8, 0xfa),
                status_foreground: Color::rgb(0x59, 0x63, 0x6e),
                error: Color::rgb(0xcf, 0x22, 0x2e),
                warning: Color::rgb(0x95, 0x36, 0x00),
            },
            syntax: syntax_map(&[
                (HighlightKind::Keyword, Color::rgb(0xcf, 0x22, 0x2e)),
                (HighlightKind::Function, Color::rgb(0x82, 0x50, 0xdf)),
                (HighlightKind::Type, Color::rgb(0x95, 0x36, 0x00)),
                (HighlightKind::Variable, Color::rgb(0x1f, 0x23, 0x28)),
                (HighlightKind::Constant, Color::rgb(0x09, 0x69, 0xda)),
                (HighlightKind::String, Color::rgb(0x0a, 0x30, 0x69)),
                (HighlightKind::Number, Color::rgb(0x09, 0x69, 0xda)),
                (HighlightKind::Boolean, Color::rgb(0x09, 0x69, 0xda)),
                (HighlightKind::Comment, Color::rgb(0x6e, 0x77, 0x81)),
                (HighlightKind::Operator, Color::rgb(0xcf, 0x22, 0x2e)),
                (HighlightKind::Punctuation, Color::rgb(0x1f, 0x23, 0x28)),
                (HighlightKind::Attribute, Color::rgb(0x82, 0x50, 0xdf)),
                (HighlightKind::Property, Color::rgb(0x09, 0x69, 0xda)),
                (HighlightKind::Namespace, Color::rgb(0x95, 0x36, 0x00)),
                (HighlightKind::Escape, Color::rgb(0x95, 0x36, 0x00)),
                (HighlightKind::Text, Color::rgb(0x1f, 0x23, 0x28)),
            ]),
        }
    }

    /// Parse a theme from JSON, rejecting anything the renderer could not draw.
    pub fn from_json(json: &str) -> Result<Theme> {
        let theme: Theme =
            serde_json::from_str(json).map_err(|e| UiError::Theme(e.to_string()))?;
        theme.validate()?;
        Ok(theme)
    }

    /// Serialise to JSON.
    pub fn to_json(&self) -> String {
        // A theme is edited by hand, so it is written pretty-printed.
        serde_json::to_string_pretty(self).expect("a Theme is always serialisable")
    }

    /// Check that every highlight class has a colour.
    pub fn validate(&self) -> Result<()> {
        let missing: Vec<&str> = HighlightKind::ALL
            .iter()
            .map(|kind| kind.name())
            .filter(|name| !self.syntax.contains_key(*name))
            .collect();

        if !missing.is_empty() {
            return Err(UiError::Theme(format!(
                "no colour for: {}. Every highlight class needs one, \
                 otherwise some code is invisible on some files.",
                missing.join(", ")
            )));
        }

        if self.name.trim().is_empty() {
            return Err(UiError::Theme("the theme has no name".to_string()));
        }

        Ok(())
    }

    /// The colour for a highlight class.
    ///
    /// A validated theme always has one; the fallback exists so that a theme
    /// constructed in memory without going through [`Theme::from_json`] cannot
    /// panic mid-frame.
    pub fn syntax_color(&self, kind: HighlightKind) -> Color {
        self.syntax.get(kind.name()).copied().unwrap_or(self.ui.foreground)
    }

    /// Whether the background is dark enough that light text is readable on it.
    ///
    /// Uses the WCAG relative-luminance formula rather than a naive average,
    /// because green contributes far more perceived brightness than blue.
    pub fn background_is_dark(&self) -> bool {
        relative_luminance(self.ui.background) < 0.5
    }

    /// The contrast ratio between foreground and background, as WCAG defines it.
    ///
    /// A ratio below 4.5 fails WCAG AA for body text, which for an editor means
    /// code that is tiring to read for a whole working day.
    pub fn contrast_ratio(&self) -> f32 {
        contrast(self.ui.foreground, self.ui.background)
    }
}

impl Default for Theme {
    fn default() -> Self {
        Theme::dark()
    }
}

/// Build the syntax map from a list, so the built-in themes read as tables.
fn syntax_map(entries: &[(HighlightKind, Color)]) -> BTreeMap<String, Color> {
    entries.iter().map(|(kind, color)| (kind.name().to_string(), *color)).collect()
}

/// WCAG relative luminance, on sRGB channels.
fn relative_luminance(color: Color) -> f32 {
    fn channel(value: u8) -> f32 {
        let v = value as f32 / 255.0;
        if v <= 0.040_45 { v / 12.92 } else { ((v + 0.055) / 1.055).powf(2.4) }
    }
    0.2126 * channel(color.r) + 0.7152 * channel(color.g) + 0.0722 * channel(color.b)
}

/// WCAG contrast ratio between two colours.
fn contrast(a: Color, b: Color) -> f32 {
    let la = relative_luminance(a);
    let lb = relative_luminance(b);
    let (lighter, darker) = if la > lb { (la, lb) } else { (lb, la) };
    (lighter + 0.05) / (darker + 0.05)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_builtin_themes_colour_every_highlight_class() {
        // A grammar can emit any class at any time; one without a colour would
        // be an invisible token, and the user would just see a gap.
        Theme::dark().validate().unwrap();
        Theme::light().validate().unwrap();
    }

    #[test]
    fn every_highlight_class_resolves_to_a_colour() {
        let theme = Theme::dark();
        for kind in HighlightKind::ALL {
            let color = theme.syntax_color(*kind);
            assert!(!color.is_transparent(), "{} is invisible", kind.name());
        }
    }

    #[test]
    fn a_theme_survives_a_round_trip_through_json() {
        let original = Theme::dark();
        let restored = Theme::from_json(&original.to_json()).unwrap();
        assert_eq!(original, restored);
    }

    #[test]
    fn a_theme_missing_a_colour_is_rejected_by_name() {
        let mut theme = Theme::dark();
        theme.syntax.remove("string");
        theme.syntax.remove("comment");

        let error = Theme::from_json(&theme.to_json()).unwrap_err();
        let message = error.to_string();
        // The message has to say which keys, or fixing the file is guesswork.
        assert!(message.contains("string"), "{message}");
        assert!(message.contains("comment"), "{message}");
    }

    #[test]
    fn a_nameless_theme_is_rejected() {
        let mut theme = Theme::light();
        theme.name = "   ".to_string();
        assert!(theme.validate().is_err());
    }

    #[test]
    fn malformed_json_is_a_theme_error_not_a_panic() {
        assert!(matches!(Theme::from_json("{ not json"), Err(UiError::Theme(_))));
        assert!(matches!(Theme::from_json("[]"), Err(UiError::Theme(_))));
    }

    #[test]
    fn the_dark_theme_is_dark_and_the_light_theme_is_not() {
        assert!(Theme::dark().background_is_dark());
        assert!(!Theme::light().background_is_dark());
        assert_eq!(Theme::dark().dark, Theme::dark().background_is_dark());
        assert_eq!(Theme::light().dark, Theme::light().background_is_dark());
    }

    #[test]
    fn both_builtin_themes_pass_wcag_aa_for_body_text() {
        // 4.5:1 is the AA threshold. Code is read for hours at a time, so this
        // is a floor rather than a target.
        for theme in [Theme::dark(), Theme::light()] {
            let ratio = theme.contrast_ratio();
            assert!(ratio >= 4.5, "{} has a contrast ratio of only {ratio:.2}", theme.name);
        }
    }

    #[test]
    fn comments_stay_readable_against_the_background() {
        // Comment colours are the usual casualty of a theme tuned by eye: they
        // are deliberately dimmed, and dimmed too far they vanish. 3:1 is the
        // WCAG threshold for large/secondary text.
        for theme in [Theme::dark(), Theme::light()] {
            let comment = theme.syntax_color(HighlightKind::Comment);
            let ratio = contrast(comment, theme.ui.background);
            assert!(ratio >= 3.0, "{}: comments are at {ratio:.2}:1", theme.name);
        }
    }

    #[test]
    fn the_cursor_is_visible_against_the_current_line_band() {
        // The caret sits on the highlighted current line, not the plain
        // background, so that is the pair that has to contrast.
        for theme in [Theme::dark(), Theme::light()] {
            let ratio = contrast(theme.ui.cursor, theme.ui.current_line);
            assert!(ratio >= 3.0, "{}: the caret is at {ratio:.2}:1", theme.name);
        }
    }

    #[test]
    fn selection_is_translucent_so_text_shows_through() {
        for theme in [Theme::dark(), Theme::light()] {
            assert!(
                theme.ui.selection.a < 255,
                "{}: an opaque selection hides the text it selects",
                theme.name
            );
            assert!(theme.ui.selection.a > 0, "{}: an invisible selection", theme.name);
        }
    }

    #[test]
    fn luminance_matches_the_wcag_reference_values() {
        assert!((relative_luminance(Color::rgb(255, 255, 255)) - 1.0).abs() < 1e-4);
        assert!(relative_luminance(Color::rgb(0, 0, 0)).abs() < 1e-6);
        // Pure green is the brightest primary by a wide margin.
        assert!(
            relative_luminance(Color::rgb(0, 255, 0))
                > relative_luminance(Color::rgb(255, 0, 0))
        );
    }

    #[test]
    fn black_on_white_is_the_maximum_contrast_ratio() {
        let ratio = contrast(Color::rgb(0, 0, 0), Color::rgb(255, 255, 255));
        assert!((ratio - 21.0).abs() < 0.01, "{ratio}");
    }

    #[test]
    fn contrast_is_symmetric() {
        let a = Color::rgb(0x12, 0x34, 0x56);
        let b = Color::rgb(0xab, 0xcd, 0xef);
        assert!((contrast(a, b) - contrast(b, a)).abs() < 1e-6);
    }

    #[test]
    fn a_hand_written_theme_file_loads() {
        // This is the shape a user's theme file actually takes, and it has to
        // parse without any of the fields the built-ins happen to set first.
        let mut syntax = String::new();
        for kind in HighlightKind::ALL {
            syntax.push_str(&format!(
                r#""{}": {{"r":255,"g":0,"b":255,"a":255}},"#,
                kind.name()
            ));
        }
        let syntax = syntax.trim_end_matches(',');

        let json = format!(
            r#"{{
              "name": "Handwritten",
              "dark": true,
              "ui": {{
                "background": {{"r":0,"g":0,"b":0,"a":255}},
                "foreground": {{"r":255,"g":255,"b":255,"a":255}},
                "gutter_background": {{"r":0,"g":0,"b":0,"a":255}},
                "gutter_foreground": {{"r":128,"g":128,"b":128,"a":255}},
                "gutter_active": {{"r":255,"g":255,"b":255,"a":255}},
                "current_line": {{"r":16,"g":16,"b":16,"a":255}},
                "selection": {{"r":40,"g":70,"b":120,"a":200}},
                "cursor": {{"r":90,"g":170,"b":255,"a":255}},
                "status_background": {{"r":16,"g":16,"b":16,"a":255}},
                "status_foreground": {{"r":160,"g":160,"b":160,"a":255}},
                "error": {{"r":255,"g":80,"b":80,"a":255}},
                "warning": {{"r":210,"g":150,"b":30,"a":255}}
              }},
              "syntax": {{{syntax}}}
            }}"#
        );

        let theme = Theme::from_json(&json).unwrap();
        assert_eq!(theme.name, "Handwritten");
        assert_eq!(theme.syntax_color(HighlightKind::Keyword), Color::rgb(255, 0, 255));
    }
}
