//! Configuration.
//!
//! Everything here has a working default, and a missing or malformed config
//! file never stops the editor from starting — it is reported and ignored. An
//! editor that refuses to open because of a stray comma in a settings file is
//! an editor you cannot use to fix the settings file.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Which built-in theme, or one from disk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ThemeChoice {
    /// The built-in dark theme.
    Dark,
    /// The built-in light theme.
    Light,
    /// A theme file, relative to the config directory or absolute.
    File(PathBuf),
}

impl Default for ThemeChoice {
    fn default() -> Self {
        ThemeChoice::Dark
    }
}

/// The editor's settings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Which theme to use.
    pub theme: ThemeChoice,
    /// Font size in logical pixels.
    pub font_size: f32,
    /// Line height as a multiple of the font size.
    pub line_height: f32,
    /// Whether to draw line numbers.
    pub line_numbers: bool,
    /// Whether to band the caret's line.
    pub highlight_current_line: bool,
    /// How many spaces one indent is.
    pub tab_width: usize,
    /// Initial window size in logical pixels.
    pub window: [f32; 2],
    /// Force the CPU renderer even when a GPU is available.
    pub force_cpu_renderer: bool,
    /// Which model the AI panel talks to by default.
    pub model: String,
    /// Whether telemetry is sent. Off, and there is no code path that turns it
    /// on: the field exists so that the answer is visible in the settings file.
    pub telemetry: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            theme: ThemeChoice::Dark,
            font_size: 14.0,
            line_height: 1.5,
            line_numbers: true,
            highlight_current_line: true,
            tab_width: 4,
            window: [1280.0, 800.0],
            force_cpu_renderer: false,
            model: nebula_ai::Model::default_coding().id().to_string(),
            telemetry: false,
        }
    }
}

impl Config {
    /// Where the config file lives on this platform.
    ///
    /// `$NEBULA_CONFIG_DIR` overrides it, which is how the test suite and the
    /// CI stress harness get an isolated configuration without touching the
    /// running user's.
    pub fn directory() -> PathBuf {
        if let Ok(dir) = std::env::var("NEBULA_CONFIG_DIR") {
            return PathBuf::from(dir);
        }

        #[cfg(target_os = "macos")]
        let base = std::env::var_os("HOME")
            .map(|home| PathBuf::from(home).join("Library/Application Support"));

        #[cfg(target_os = "windows")]
        let base = std::env::var_os("APPDATA").map(PathBuf::from);

        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        let base = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from).or_else(|| {
            std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config"))
        });

        base.unwrap_or_else(|| PathBuf::from(".")).join("nebula")
    }

    /// The config file's path.
    pub fn path() -> PathBuf {
        Self::directory().join("config.json")
    }

    /// Load the config, falling back to defaults.
    ///
    /// Returns the config and a warning if one had to be reported — the caller
    /// prints it rather than this function, so that the same code path works in
    /// the GUI, in the CLI and in a test.
    pub fn load() -> (Config, Option<String>) {
        Self::load_from(&Self::path())
    }

    /// Load from a specific path.
    pub fn load_from(path: &Path) -> (Config, Option<String>) {
        let bytes = match std::fs::read_to_string(path) {
            Ok(bytes) => bytes,
            // A missing file is the normal first-run state, not a problem.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return (Config::default(), None);
            }
            Err(error) => {
                return (
                    Config::default(),
                    Some(format!("could not read {}: {error}", path.display())),
                );
            }
        };

        match serde_json::from_str::<Config>(&bytes) {
            Ok(config) => match config.validate() {
                Ok(()) => (config, None),
                Err(message) => (
                    Config::default(),
                    Some(format!("{} is not usable: {message}", path.display())),
                ),
            },
            Err(error) => (
                Config::default(),
                Some(format!("{} could not be parsed: {error}", path.display())),
            ),
        }
    }

    /// Write the config, creating the directory if needed.
    pub fn save_to(&self, path: &Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, serde_json::to_string_pretty(self).expect("serialisable"))
    }

    /// Reject values that would produce an unusable window.
    pub fn validate(&self) -> Result<(), String> {
        if !(6.0..=96.0).contains(&self.font_size) {
            return Err(format!("font_size {} is outside 6-96", self.font_size));
        }
        if !(1.0..=3.0).contains(&self.line_height) {
            return Err(format!("line_height {} is outside 1.0-3.0", self.line_height));
        }
        if !(1..=16).contains(&self.tab_width) {
            return Err(format!("tab_width {} is outside 1-16", self.tab_width));
        }
        if self.window[0] < 320.0 || self.window[1] < 240.0 {
            return Err(format!(
                "window {}x{} is smaller than the 320x240 minimum",
                self.window[0], self.window[1]
            ));
        }
        if nebula_ai::Model::from_id(&self.model).is_none() {
            return Err(format!("model `{}` is not a known model", self.model));
        }
        Ok(())
    }

    /// Line height in logical pixels.
    pub fn line_height_px(&self) -> f32 {
        (self.font_size * self.line_height).round()
    }

    /// Resolve the theme, reporting rather than failing if a file is bad.
    pub fn resolve_theme(&self) -> (nebula_ui::Theme, Option<String>) {
        match &self.theme {
            ThemeChoice::Dark => (nebula_ui::Theme::dark(), None),
            ThemeChoice::Light => (nebula_ui::Theme::light(), None),
            ThemeChoice::File(path) => {
                let path =
                    if path.is_absolute() { path.clone() } else { Self::directory().join(path) };

                match std::fs::read_to_string(&path)
                    .map_err(|e| e.to_string())
                    .and_then(|text| {
                        nebula_ui::Theme::from_json(&text).map_err(|e| e.to_string())
                    }) {
                    Ok(theme) => (theme, None),
                    Err(message) => (
                        nebula_ui::Theme::dark(),
                        Some(format!("theme {} could not be loaded: {message}", path.display())),
                    ),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn the_defaults_are_valid() {
        Config::default().validate().unwrap();
    }

    #[test]
    fn a_missing_file_is_the_first_run_state_not_an_error() {
        let dir = TempDir::new().unwrap();
        let (config, warning) = Config::load_from(&dir.path().join("nothing-here.json"));
        assert_eq!(config, Config::default());
        assert!(warning.is_none(), "a first run must not print a warning");
    }

    #[test]
    fn a_malformed_file_is_reported_and_the_editor_still_starts() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(&path, "{ this is not json").unwrap();

        let (config, warning) = Config::load_from(&path);
        assert_eq!(config, Config::default());
        assert!(warning.unwrap().contains("could not be parsed"));
    }

    #[test]
    fn an_unknown_key_is_reported_rather_than_silently_ignored() {
        // Silently ignoring a typo'd key is how a user spends an hour wondering
        // why their setting does nothing.
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(&path, r#"{"font_sizes": 20}"#).unwrap();

        let (_, warning) = Config::load_from(&path);
        assert!(warning.unwrap().contains("font_sizes"));
    }

    #[test]
    fn an_absurd_font_size_is_rejected_with_a_usable_message() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(&path, r#"{"font_size": 4000}"#).unwrap();

        let (config, warning) = Config::load_from(&path);
        assert_eq!(config.font_size, Config::default().font_size);
        let warning = warning.unwrap();
        assert!(warning.contains("font_size"), "{warning}");
        assert!(warning.contains("6-96"), "{warning}");
    }

    #[test]
    fn an_unknown_model_is_rejected() {
        let mut config = Config::default();
        config.model = "gpt-imaginary".to_string();
        assert!(config.validate().unwrap_err().contains("gpt-imaginary"));
    }

    #[test]
    fn the_default_model_exists() {
        // The default has to name a model the provider will actually accept.
        assert!(nebula_ai::Model::from_id(&Config::default().model).is_some());
    }

    #[test]
    fn a_config_survives_a_round_trip() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("nested/config.json");

        let mut config = Config::default();
        config.font_size = 18.0;
        config.theme = ThemeChoice::Light;
        config.save_to(&path).unwrap();

        let (loaded, warning) = Config::load_from(&path);
        assert!(warning.is_none());
        assert_eq!(loaded, config);
    }

    #[test]
    fn the_built_in_themes_resolve_without_touching_the_disk() {
        let mut config = Config::default();
        config.theme = ThemeChoice::Light;
        let (theme, warning) = config.resolve_theme();
        assert!(warning.is_none());
        assert!(!theme.dark);
    }

    #[test]
    fn a_broken_theme_file_falls_back_and_says_so() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("broken.json");
        std::fs::write(&path, "{}").unwrap();

        let mut config = Config::default();
        config.theme = ThemeChoice::File(path);
        let (theme, warning) = config.resolve_theme();
        assert!(theme.dark, "the fallback is the built-in dark theme");
        assert!(warning.unwrap().contains("could not be loaded"));
    }

    #[test]
    fn a_theme_file_written_by_the_editor_loads_back() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("custom.json");
        std::fs::write(&path, nebula_ui::Theme::light().to_json()).unwrap();

        let mut config = Config::default();
        config.theme = ThemeChoice::File(path);
        let (theme, warning) = config.resolve_theme();
        assert!(warning.is_none());
        assert_eq!(theme.name, "Nebula Light");
    }

    #[test]
    fn the_line_height_is_a_whole_number_of_pixels() {
        // Fractional line heights make every other row of text land on a
        // different subpixel, which reads as uneven spacing.
        let config = Config::default();
        assert_eq!(config.line_height_px(), 21.0);
        assert_eq!(config.line_height_px().fract(), 0.0);
    }

    #[test]
    fn the_config_directory_can_be_redirected() {
        // The stress harness and the test suite both depend on this.
        let expected = std::env::var("NEBULA_CONFIG_DIR").ok();
        if let Some(dir) = expected {
            assert_eq!(Config::directory(), PathBuf::from(dir));
        } else {
            assert!(Config::directory().ends_with("nebula"));
        }
    }

    #[test]
    fn telemetry_is_off_by_default() {
        assert!(!Config::default().telemetry);
    }
}
