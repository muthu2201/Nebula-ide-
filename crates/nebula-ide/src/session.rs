//! Scripted editing sessions.
//!
//! A session script is a plain-text list of things a user does — open a file,
//! type, press a key, save — that the editor replays through exactly the code
//! paths a keyboard would drive. Nothing here simulates or stands in for the
//! editor: [`Session::run`] calls [`App::key`] and [`App::act`], the same
//! functions the window calls.
//!
//! That is what makes an end-to-end test possible on a machine with no display:
//! the script drives a real editor, real files are written to disk, and real
//! frames are rasterised and measured.
//!
//! ```text
//! open src/main.rs
//! end
//! type \n// a trailing comment
//! key ctrl+s
//! frame
//! expect-contains // a trailing comment
//! ```

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use nebula_ui::input::{Action, Key, KeyEvent, Modifiers, Motion};

use crate::app::{App, Response};

/// One scripted step.
#[derive(Debug, Clone, PartialEq)]
pub enum Step {
    /// Open a file.
    Open(PathBuf),
    /// Type literal text, one character at a time.
    Type(String),
    /// Press a key, written as `ctrl+s`, `shift+left`, `enter`.
    Press(KeyEvent),
    /// Carry out an action directly, for things with no default binding.
    Do(Action),
    /// Repeat the previous step `n` more times.
    Repeat(usize),
    /// Draw a frame and record how long it took.
    Frame,
    /// Save the focused document.
    Save,
    /// Fail unless the document contains this text.
    ExpectContains(String),
    /// Fail unless the document is exactly this many characters long.
    ExpectLength(usize),
    /// Fail unless the focused document has no unsaved changes.
    ExpectSaved,
    /// A comment or a blank line.
    Nothing,
}

/// A parsed script.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Session {
    /// The steps, in order.
    pub steps: Vec<Step>,
}

/// What a run produced.
#[derive(Debug, Clone, Default)]
pub struct SessionReport {
    /// How many steps ran.
    pub steps: usize,
    /// How many keystrokes were delivered.
    pub keystrokes: u64,
    /// How many frames were drawn.
    pub frames: u64,
    /// How long each frame took.
    pub frame_times: Vec<Duration>,
    /// How long each keystroke took to reach a finished frame, when a frame
    /// immediately followed it.
    pub keystroke_to_photon: Vec<Duration>,
    /// How long the whole run took.
    pub elapsed: Duration,
    /// Files written during the run.
    pub saved: Vec<PathBuf>,
    /// Anything the editor reported as an error.
    pub errors: Vec<String>,
}

impl SessionReport {
    /// The slowest frame.
    pub fn worst_frame(&self) -> Option<Duration> {
        self.frame_times.iter().copied().max()
    }

    /// The frame time that 95% of frames came in under.
    ///
    /// The mean hides exactly the stalls a user notices, so the budget is
    /// checked against a high percentile instead.
    pub fn frame_p95(&self) -> Option<Duration> {
        percentile(&self.frame_times, 0.95)
    }

    /// The median frame time.
    pub fn frame_p50(&self) -> Option<Duration> {
        percentile(&self.frame_times, 0.50)
    }

    /// The 95th-percentile keystroke-to-photon latency.
    pub fn latency_p95(&self) -> Option<Duration> {
        percentile(&self.keystroke_to_photon, 0.95)
    }

    /// Whether every frame met the given budget.
    pub fn within_budget(&self, budget: Duration) -> bool {
        self.frame_p95().is_none_or(|p95| p95 <= budget)
    }
}

/// The value `fraction` of the samples come in under.
fn percentile(samples: &[Duration], fraction: f64) -> Option<Duration> {
    if samples.is_empty() {
        return None;
    }
    let mut sorted = samples.to_vec();
    sorted.sort_unstable();
    let index = ((sorted.len() - 1) as f64 * fraction).round() as usize;
    Some(sorted[index])
}

/// Something wrong with the script itself.
#[derive(Debug, thiserror::Error)]
pub enum ScriptError {
    /// A line could not be understood.
    #[error("line {line}: {message}")]
    Syntax {
        /// 1-based line number.
        line: usize,
        /// What was wrong.
        message: String,
    },

    /// An expectation was not met.
    #[error("line {line}: {message}")]
    Failed {
        /// 1-based line number.
        line: usize,
        /// What was expected and what was found.
        message: String,
    },

    /// The editor failed.
    #[error(transparent)]
    Ide(#[from] crate::IdeError),
}

impl Session {
    /// Parse a script.
    pub fn parse(source: &str) -> Result<Session, ScriptError> {
        let mut steps = Vec::new();

        for (index, raw) in source.lines().enumerate() {
            let line = index + 1;
            let trimmed = raw.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                steps.push(Step::Nothing);
                continue;
            }

            let (command, rest) = match trimmed.split_once(char::is_whitespace) {
                Some((command, rest)) => (command, rest.trim_start()),
                None => (trimmed, ""),
            };

            let step = match command {
                "open" => {
                    if rest.is_empty() {
                        return Err(syntax(line, "`open` needs a path"));
                    }
                    Step::Open(PathBuf::from(rest))
                }
                "type" => Step::Type(unescape(rest)),
                "key" => {
                    Step::Press(parse_key(rest).ok_or_else(|| {
                        syntax(line, format!("`{rest}` is not a key I recognise"))
                    })?)
                }
                "repeat" => Step::Repeat(
                    rest.parse().map_err(|_| syntax(line, format!("`{rest}` is not a count")))?,
                ),
                "frame" => Step::Frame,
                "save" => Step::Save,
                "home" => Step::Do(Action::Move(Motion::LineStart)),
                "end" => Step::Do(Action::Move(Motion::DocumentEnd)),
                "start" => Step::Do(Action::Move(Motion::DocumentStart)),
                "select-all" => Step::Do(Action::SelectAll),
                "undo" => Step::Do(Action::Undo),
                "redo" => Step::Do(Action::Redo),
                "cursor-below" => Step::Do(Action::AddCursorBelow),
                "scroll" => Step::Do(Action::Scroll(
                    rest.parse().map_err(|_| syntax(line, format!("`{rest}` is not a count")))?,
                )),
                "expect-contains" => {
                    if rest.is_empty() {
                        return Err(syntax(line, "`expect-contains` needs some text"));
                    }
                    Step::ExpectContains(unescape(rest))
                }
                "expect-length" => Step::ExpectLength(
                    rest.parse().map_err(|_| syntax(line, format!("`{rest}` is not a length")))?,
                ),
                "expect-saved" => Step::ExpectSaved,
                other => return Err(syntax(line, format!("unknown command `{other}`"))),
            };

            steps.push(step);
        }

        Ok(Session { steps })
    }

    /// Load a script from a file.
    pub fn load(path: impl AsRef<Path>) -> Result<Session, ScriptError> {
        let source = std::fs::read_to_string(path).map_err(crate::IdeError::Io)?;
        Self::parse(&source)
    }

    /// Replay the script against an editor.
    ///
    /// Relative paths in `open` are resolved against `root`, so a script is
    /// portable between a checkout and a temporary directory.
    pub fn run(&self, app: &mut App, root: &Path) -> Result<SessionReport, ScriptError> {
        let mut report = SessionReport::default();
        let started = Instant::now();
        let mut previous: Option<&Step> = None;
        let mut last_keystroke: Option<Instant> = None;

        for (index, step) in self.steps.iter().enumerate() {
            let line = index + 1;

            let effective = match step {
                Step::Repeat(count) => {
                    let Some(previous) = previous else {
                        return Err(syntax(line, "`repeat` has nothing before it"));
                    };
                    for _ in 0..*count {
                        self.execute(previous, app, root, line, &mut report, &mut last_keystroke)?;
                    }
                    previous
                }
                other => {
                    self.execute(other, app, root, line, &mut report, &mut last_keystroke)?;
                    other
                }
            };

            if !matches!(effective, Step::Nothing) {
                report.steps += 1;
                previous = Some(effective);
            }
        }

        report.elapsed = started.elapsed();
        report.frames = app.frame_count();
        Ok(report)
    }

    fn execute(
        &self,
        step: &Step,
        app: &mut App,
        root: &Path,
        line: usize,
        report: &mut SessionReport,
        last_keystroke: &mut Option<Instant>,
    ) -> Result<(), ScriptError> {
        match step {
            Step::Nothing => {}

            Step::Open(path) => {
                let path = if path.is_absolute() { path.clone() } else { root.join(path) };
                app.open(&path)?;
            }

            Step::Type(text) => {
                for c in text.chars() {
                    let event =
                        if c == '\n' { KeyEvent::new(Key::Enter) } else { KeyEvent::char(c) };
                    record(report, app.key(&event));
                    report.keystrokes += 1;
                }
                *last_keystroke = Some(Instant::now());
            }

            Step::Press(event) => {
                record(report, app.key(event));
                report.keystrokes += 1;
                *last_keystroke = Some(Instant::now());
            }

            Step::Do(action) => {
                record(report, app.act(action.clone(), false));
            }

            Step::Repeat(_) => {
                // Handled by the caller, which knows what came before.
            }

            Step::Frame => {
                app.frame(1.0)?;
                if let Some(elapsed) = app.last_frame_time() {
                    report.frame_times.push(elapsed);
                }
                // A frame drawn straight after a keystroke measures the whole
                // path from key press to finished pixels.
                if let Some(at) = last_keystroke.take() {
                    report.keystroke_to_photon.push(at.elapsed());
                }
            }

            Step::Save => match app.save() {
                Response::Error(message) => {
                    return Err(ScriptError::Failed { line, message });
                }
                _ => {
                    if let Some(path) = app.workspace.document().path() {
                        report.saved.push(path.to_path_buf());
                    }
                }
            },

            Step::ExpectContains(needle) => {
                let text = app.workspace.document().text();
                if !text.contains(needle.as_str()) {
                    return Err(ScriptError::Failed {
                        line,
                        message: format!("the document does not contain {needle:?}"),
                    });
                }
            }

            Step::ExpectLength(expected) => {
                let actual = app.workspace.document().buffer().len_chars();
                if actual != *expected {
                    return Err(ScriptError::Failed {
                        line,
                        message: format!("expected {expected} characters, found {actual}"),
                    });
                }
            }

            Step::ExpectSaved => {
                if app.workspace.document().is_modified() {
                    return Err(ScriptError::Failed {
                        line,
                        message: "the document still has unsaved changes".to_string(),
                    });
                }
            }
        }

        Ok(())
    }
}

fn record(report: &mut SessionReport, response: Response) {
    if let Response::Error(message) = response {
        report.errors.push(message);
    }
}

fn syntax(line: usize, message: impl Into<String>) -> ScriptError {
    ScriptError::Syntax { line, message: message.into() }
}

/// Turn `\n`, `\t` and `\\` into the characters they name.
fn unescape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('\\') => out.push('\\'),
            // An unknown escape is left as written rather than swallowed, so a
            // Windows path in a script does not silently lose characters.
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

/// Parse `ctrl+shift+left` into a key event.
fn parse_key(spec: &str) -> Option<KeyEvent> {
    let mut modifiers = Modifiers::NONE;
    let mut key = None;

    for part in spec.split('+') {
        match part.trim().to_ascii_lowercase().as_str() {
            "" => return None,
            "ctrl" | "control" => modifiers.ctrl = true,
            "shift" => modifiers.shift = true,
            "alt" | "option" => modifiers.alt = true,
            "cmd" | "meta" | "super" | "win" => modifiers.meta = true,
            // `primary` is how a script stays portable: Cmd on macOS, Ctrl
            // everywhere else.
            "primary" => {
                if cfg!(target_os = "macos") {
                    modifiers.meta = true;
                } else {
                    modifiers.ctrl = true;
                }
            }
            "enter" | "return" => key = Some(Key::Enter),
            "tab" => key = Some(Key::Tab),
            "backspace" => key = Some(Key::Backspace),
            "delete" | "del" => key = Some(Key::Delete),
            "escape" | "esc" => key = Some(Key::Escape),
            "left" => key = Some(Key::Left),
            "right" => key = Some(Key::Right),
            "up" => key = Some(Key::Up),
            "down" => key = Some(Key::Down),
            "home" => key = Some(Key::Home),
            "end" => key = Some(Key::End),
            "pageup" | "pgup" => key = Some(Key::PageUp),
            "pagedown" | "pgdn" => key = Some(Key::PageDown),
            "space" => key = Some(Key::Char(' ')),
            other => {
                if let Some(number) = other.strip_prefix('f').and_then(|n| n.parse().ok()) {
                    key = Some(Key::Function(number));
                } else {
                    let mut chars = other.chars();
                    match (chars.next(), chars.next()) {
                        (Some(c), None) => key = Some(Key::Char(c)),
                        _ => return None,
                    }
                }
            }
        }
    }

    key.map(|key| KeyEvent::with(key, modifiers))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Config;
    use tempfile::TempDir;

    fn app() -> App {
        let config = Config { force_cpu_renderer: true, ..Config::default() };
        App::new(config, 800, 600).unwrap()
    }

    #[test]
    fn a_script_parses_into_steps() {
        let session = Session::parse("open a.rs\ntype hello\nkey ctrl+s\nframe").unwrap();
        assert_eq!(session.steps.len(), 4);
        assert_eq!(session.steps[0], Step::Open(PathBuf::from("a.rs")));
        assert_eq!(session.steps[1], Step::Type("hello".to_string()));
        assert_eq!(session.steps[3], Step::Frame);
    }

    #[test]
    fn comments_and_blank_lines_are_ignored() {
        let session = Session::parse("# a note\n\n   \ntype x").unwrap();
        assert_eq!(session.steps.iter().filter(|s| **s != Step::Nothing).count(), 1);
    }

    #[test]
    fn an_unknown_command_names_its_line() {
        let error = Session::parse("type x\nfly to the moon").unwrap_err();
        let message = error.to_string();
        assert!(message.contains("line 2"), "{message}");
        assert!(message.contains("fly"), "{message}");
    }

    #[test]
    fn escapes_become_the_characters_they_name() {
        assert_eq!(unescape(r"a\nb"), "a\nb");
        assert_eq!(unescape(r"a\tb"), "a\tb");
        assert_eq!(unescape(r"a\\b"), r"a\b");
        // An unknown escape survives intact rather than being eaten.
        assert_eq!(unescape(r"C:\users"), r"C:\users");
    }

    #[test]
    fn key_specs_parse_with_modifiers() {
        let event = parse_key("ctrl+shift+left").unwrap();
        assert_eq!(event.key, Key::Left);
        assert!(event.modifiers.ctrl && event.modifiers.shift);
        assert!(!event.modifiers.alt);

        assert_eq!(parse_key("enter").unwrap().key, Key::Enter);
        assert_eq!(parse_key("f5").unwrap().key, Key::Function(5));
        assert_eq!(parse_key("a").unwrap().key, Key::Char('a'));
        assert!(parse_key("not-a-key").is_none());
    }

    #[test]
    fn primary_resolves_to_the_platforms_modifier() {
        let event = parse_key("primary+s").unwrap();
        assert!(event.modifiers.primary());
    }

    #[test]
    fn a_script_edits_a_real_file_end_to_end() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("main.rs"), "fn main() {}\n").unwrap();

        let session = Session::parse(
            r"open main.rs
end
type \n// added by the script
save
expect-contains // added by the script
expect-saved
frame",
        )
        .unwrap();

        let mut app = app();
        let report = session.run(&mut app, dir.path()).unwrap();

        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert_eq!(report.saved.len(), 1);
        assert_eq!(report.frame_times.len(), 1);

        let written = std::fs::read_to_string(dir.path().join("main.rs")).unwrap();
        assert!(written.ends_with("// added by the script"), "{written}");
    }

    #[test]
    fn a_failed_expectation_names_the_line_and_what_was_wrong() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("a.txt"), "hello").unwrap();

        let session = Session::parse("open a.txt\nexpect-contains goodbye").unwrap();
        let error = session.run(&mut app(), dir.path()).unwrap_err();

        let message = error.to_string();
        assert!(message.contains("line 2"), "{message}");
        assert!(message.contains("goodbye"), "{message}");
    }

    #[test]
    fn expect_length_counts_characters_not_bytes() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("u.txt"), "héllo").unwrap();

        let session = Session::parse("open u.txt\nexpect-length 5").unwrap();
        session.run(&mut app(), dir.path()).unwrap();
    }

    #[test]
    fn repeat_reruns_the_previous_step() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("a.txt"), "").unwrap();

        let session = Session::parse("open a.txt\ntype ab\nrepeat 4\nexpect-length 10").unwrap();
        session.run(&mut app(), dir.path()).unwrap();
    }

    #[test]
    fn repeat_with_nothing_before_it_is_a_script_error() {
        let session = Session::parse("repeat 3").unwrap();
        let dir = TempDir::new().unwrap();
        assert!(matches!(session.run(&mut app(), dir.path()), Err(ScriptError::Syntax { .. })));
    }

    #[test]
    fn undo_and_redo_work_through_a_script() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("a.txt"), "base").unwrap();

        let session = Session::parse(
            "open a.txt
end
type +more
expect-contains base+more
undo
expect-length 4
redo
expect-contains base+more",
        )
        .unwrap();

        session.run(&mut app(), dir.path()).unwrap();
    }

    #[test]
    fn frame_timings_are_collected_and_summarised() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("a.txt"), "x\n".repeat(500)).unwrap();

        let session = Session::parse("open a.txt\nframe\nrepeat 20").unwrap();
        let report = session.run(&mut app(), dir.path()).unwrap();

        assert_eq!(report.frame_times.len(), 21);
        assert!(report.frame_p50().unwrap() <= report.frame_p95().unwrap());
        assert!(report.frame_p95().unwrap() <= report.worst_frame().unwrap());
    }

    #[test]
    fn keystroke_to_photon_is_measured_when_a_frame_follows_a_keystroke() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("a.txt"), "x").unwrap();

        let session = Session::parse("open a.txt\ntype y\nframe\ntype z\nframe").unwrap();
        let report = session.run(&mut app(), dir.path()).unwrap();

        assert_eq!(report.keystroke_to_photon.len(), 2);
        assert!(report.latency_p95().is_some());
    }

    #[test]
    fn percentiles_are_computed_over_sorted_samples() {
        // Nearest-rank over `len - 1`, so p0 is the fastest sample and p100 the
        // slowest, with no interpolation between neighbours.
        let samples: Vec<Duration> = (1..=100).map(Duration::from_millis).collect();
        assert_eq!(percentile(&samples, 0.0), Some(Duration::from_millis(1)));
        assert_eq!(percentile(&samples, 0.95), Some(Duration::from_millis(95)));
        assert_eq!(percentile(&samples, 1.0), Some(Duration::from_millis(100)));
        assert_eq!(percentile(&[], 0.5), None);
        assert_eq!(percentile(&[Duration::from_millis(7)], 0.95), Some(Duration::from_millis(7)));
    }

    #[test]
    fn a_budget_with_no_frames_is_trivially_met() {
        assert!(SessionReport::default().within_budget(Duration::from_millis(8)));
    }

    #[test]
    fn a_relative_path_resolves_against_the_scripts_root() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/main.rs"), "fn main() {}").unwrap();

        let session = Session::parse("open src/main.rs\nexpect-contains fn main").unwrap();
        session.run(&mut app(), dir.path()).unwrap();
    }

    #[test]
    fn a_script_can_be_loaded_from_a_file() {
        let dir = TempDir::new().unwrap();
        let script = dir.path().join("session.nbs");
        std::fs::write(&script, "type hello\nexpect-contains hello").unwrap();

        let session = Session::load(&script).unwrap();
        session.run(&mut app(), dir.path()).unwrap();
    }
}
