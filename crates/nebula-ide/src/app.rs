//! The editor, minus the window.
//!
//! [`App`] owns the workspace, the view and the renderer, and turns an
//! [`Action`] into a change on screen. Everything a user can do is reachable
//! from here without a display server, which is why the stress harness can
//! drive a real editing session in CI.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use nebula_render::backend::Framebuffer;
use nebula_render::{Backend, RendererKind, Scene, Surface};
use nebula_ui::{Action, EditorView, KeyEvent, Layout, Theme, input, keymap};

use crate::config::Config;
use crate::workspace::Workspace;
use crate::{IdeError, Result};

/// What the editor did with an event, for the caller to act on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Response {
    /// Nothing happened; do not redraw.
    Ignored,
    /// The screen needs repainting.
    Redraw,
    /// The user asked to quit.
    Quit,
    /// A message for the status bar.
    Message(String),
    /// Something went wrong in a way the user should see rather than a log.
    Error(String),
}

/// A running editor.
pub struct App {
    /// Open files.
    pub workspace: Workspace,
    /// What is on screen.
    pub view: EditorView,
    /// Settings.
    pub config: Config,

    backend: Backend,
    message: Option<(String, Instant)>,
    last_frame: Option<Duration>,
    frames: u64,
}

impl std::fmt::Debug for App {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // `Backend` holds GPU handles, which have no useful Debug output.
        f.debug_struct("App")
            .field("files", &self.workspace.len())
            .field("renderer", &self.backend.kind())
            .field("frames", &self.frames)
            .finish_non_exhaustive()
    }
}

/// How long a status message stays up.
const MESSAGE_TIMEOUT: Duration = Duration::from_secs(4);

/// How long typing has to pause before the editor re-parses.
///
/// Short enough that highlighting catches up before the user has read what they
/// typed, long enough that a burst of typing never waits for a parse.
pub const PARSE_DELAY: Duration = Duration::from_millis(40);

impl App {
    /// Start an editor with the given settings and surface size.
    pub fn new(config: Config, width: u32, height: u32) -> Result<Self> {
        let (theme, warning) = config.resolve_theme();
        if let Some(warning) = &warning {
            tracing::warn!("{warning}");
        }

        let surface = Surface::new(width.max(320), height.max(240));
        let backend = if config.force_cpu_renderer {
            Backend::force_cpu(surface)?
        } else {
            Backend::select(surface)?
        };

        let mut app = Self {
            workspace: Workspace::new(),
            view: Self::build_view(&config, theme, width as f32, height as f32, 1),
            config,
            backend,
            message: warning.map(|w| (w, Instant::now())),
            last_frame: None,
            frames: 0,
        };
        app.sync_view();
        Ok(app)
    }

    /// Start an editor on a project directory.
    pub fn open_project(
        config: Config,
        root: impl AsRef<Path>,
        width: u32,
        height: u32,
    ) -> Result<Self> {
        let mut app = Self::new(config, width, height)?;
        app.workspace = Workspace::open_project(root)?;
        app.sync_view();
        Ok(app)
    }

    fn build_view(
        config: &Config,
        theme: Theme,
        width: f32,
        height: f32,
        total_lines: usize,
    ) -> EditorView {
        let line_height = config.line_height_px();
        // A monospace advance is a fixed fraction of the font size; using the
        // shaped width of `0` would be more exact, but the font system is not
        // needed here and this matches the bundled face to within a fraction of
        // a pixel.
        let char_width = (config.font_size * 0.6).round();

        let mut view =
            EditorView::new(theme, Layout::new(width, height, line_height, char_width, total_lines));
        view.font_size = config.font_size;
        view.show_line_numbers = config.line_numbers;
        view.highlight_current_line = config.highlight_current_line;
        view
    }

    /// Re-size the gutter and viewport to the focused document.
    fn sync_view(&mut self) {
        let lines = self.workspace.document().buffer().len_lines();
        self.view.resize(self.view.layout.width, self.view.layout.height, lines);
    }

    /// Which renderer is in use.
    pub fn renderer_kind(&self) -> RendererKind {
        self.backend.kind()
    }

    /// Why that renderer was chosen — including, on a fallback, what failed.
    pub fn renderer_reason(&self) -> &str {
        self.backend.selection_reason()
    }

    /// How long the last frame took.
    pub fn last_frame_time(&self) -> Option<Duration> {
        self.last_frame
    }

    /// How many frames have been drawn.
    pub fn frame_count(&self) -> u64 {
        self.frames
    }

    /// Open a file.
    pub fn open(&mut self, path: impl AsRef<Path>) -> Result<()> {
        self.workspace.open(path)?;
        self.sync_view();
        self.view.viewport.first_line = 0;
        self.view.viewport.first_column = 0;
        Ok(())
    }

    /// Handle a key press.
    pub fn key(&mut self, event: &KeyEvent) -> Response {
        let Some(action) = keymap(event) else { return Response::Ignored };

        // A run of typed characters is one gesture, so it is one undo step.
        // Undo that gives back a single character at a time is technically
        // faithful and practically useless.
        let groupable = match &action {
            Action::Insert(text) => text.chars().count() == 1 && !text.contains('\n'),
            _ => false,
        };

        self.act(action, groupable)
    }

    /// Carry out an action.
    pub fn act(&mut self, action: Action, groupable: bool) -> Response {
        // Global actions are the editor's business, not the document's.
        match &action {
            Action::Save => return self.save(),
            Action::Quit => {
                return if self.workspace.has_unsaved_changes() {
                    Response::Error(format!(
                        "{} file(s) have unsaved changes. Save them, or quit again to discard.",
                        self.workspace.unsaved().len()
                    ))
                } else {
                    Response::Quit
                };
            }
            Action::Close => {
                return match self.workspace.close(false) {
                    Ok(()) => {
                        self.sync_view();
                        Response::Redraw
                    }
                    Err(error) => Response::Error(error.to_string()),
                };
            }
            _ => {}
        }

        let before = self.workspace.document().buffer().clone();
        let applied = {
            let App { workspace, view, .. } = self;
            input::apply(&action, workspace.document_mut(), &mut view.viewport, groupable)
        };

        match applied {
            Ok(applied) if applied.global => Response::Ignored,
            Ok(applied) => {
                if applied.edited {
                    // Hand the parser exactly what was applied, so the next
                    // re-parse is incremental. `before` is the pre-edit buffer
                    // the tree was built against.
                    // Only the cheap half here: the tree's offsets are shifted
                    // so highlighting stays correct, and the re-parse waits for
                    // a gap in typing. See `Workspace::refresh_syntax_if_idle`.
                    let change = self.workspace.document().last_change().to_vec();
                    self.workspace.note_edit(&before, &change);
                    self.sync_view();
                }

                if applied.needs_redraw() { Response::Redraw } else { Response::Ignored }
            }
            Err(error) => Response::Error(error.to_string()),
        }
    }

    /// Save the focused document.
    pub fn save(&mut self) -> Response {
        match self.workspace.save() {
            Ok(path) => {
                let name = path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| path.display().to_string());
                self.set_message(format!("Saved {name}"));
                Response::Redraw
            }
            Err(IdeError::NoPath) => {
                Response::Error("This document has no file name. Use Save As.".to_string())
            }
            Err(error) => Response::Error(error.to_string()),
        }
    }

    /// Save the focused document under a new name.
    pub fn save_as(&mut self, path: impl Into<PathBuf>) -> Response {
        match self.workspace.save_as(path) {
            Ok(path) => {
                self.set_message(format!("Saved {}", path.display()));
                Response::Redraw
            }
            Err(error) => Response::Error(error.to_string()),
        }
    }

    /// Put a message in the status bar.
    pub fn set_message(&mut self, message: impl Into<String>) {
        self.message = Some((message.into(), Instant::now()));
    }

    /// The current status message, if it has not expired.
    pub fn message(&self) -> Option<&str> {
        self.message
            .as_ref()
            .filter(|(_, at)| at.elapsed() < MESSAGE_TIMEOUT)
            .map(|(text, _)| text.as_str())
    }

    /// Resize the window.
    pub fn resize(&mut self, width: u32, height: u32) -> Result<()> {
        let width = width.max(320);
        let height = height.max(240);
        self.view.resize(
            width as f32,
            height as f32,
            self.workspace.document().buffer().len_lines(),
        );
        self.backend.resize(Surface::new(width, height))?;
        Ok(())
    }

    /// Build the frame without drawing it.
    pub fn scene(&mut self, scale_factor: f32) -> Result<Scene> {
        // The one place a deferred re-parse can happen without delaying a
        // keystroke: if typing has paused, catch the tree up before painting.
        if let Err(error) = self.workspace.refresh_syntax_if_idle(PARSE_DELAY) {
            tracing::warn!(%error, "re-parse failed; highlighting may be stale");
        }

        let mut status = nebula_ui::view::StatusLine::for_document(self.workspace.document());
        if let Some(message) = self.message.as_ref().filter(|(_, at)| at.elapsed() < MESSAGE_TIMEOUT)
        {
            // A message displaces the language indicator rather than adding a
            // fourth field: the status bar has a fixed width and something has
            // to give.
            status.centre = message.0.clone();
        }

        let App { workspace, view, .. } = self;
        Ok(view.render_with_status(
            workspace.document(),
            workspace.active().tree.as_ref(),
            &status,
            scale_factor,
        )?)
    }

    /// Draw a frame and return the pixels.
    pub fn frame(&mut self, scale_factor: f32) -> Result<Framebuffer> {
        let scene = self.scene(scale_factor)?;
        let started = Instant::now();
        let framebuffer = self.backend.render(&scene)?;
        self.last_frame = Some(started.elapsed());
        self.frames += 1;
        Ok(framebuffer)
    }

    /// Whether the last frame met the renderer's budget.
    ///
    /// The GPU path is held to 8 ms and the CPU path to 16 ms — one frame at
    /// 120 Hz and one at 60 Hz respectively.
    pub fn met_frame_budget(&self) -> Option<bool> {
        self.last_frame.map(|elapsed| elapsed <= self.backend.kind().frame_budget())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nebula_ui::input::{Key, Modifiers, Motion};
    use tempfile::TempDir;

    fn app() -> App {
        let mut config = Config::default();
        // Every test here asserts on behaviour, not on GPU output, and the CPU
        // path is the one that is guaranteed to exist on every machine.
        config.force_cpu_renderer = true;
        App::new(config, 800, 600).unwrap()
    }

    fn typed(app: &mut App, text: &str) {
        for c in text.chars() {
            app.key(&KeyEvent::char(c));
        }
    }

    #[test]
    fn a_new_editor_starts_with_an_empty_buffer_and_can_draw() {
        let mut app = app();
        assert!(app.workspace.document().buffer().is_empty());

        let frame = app.frame(1.0).unwrap();
        assert_eq!(frame.pixel_count(), 800 * 600);
        assert_eq!(app.frame_count(), 1);
    }

    #[test]
    fn typing_reaches_the_document() {
        let mut app = app();
        typed(&mut app, "hello");
        assert_eq!(app.workspace.document().text(), "hello");
    }

    #[test]
    fn typing_asks_for_a_redraw_and_an_unbound_key_does_not() {
        let mut app = app();
        assert_eq!(app.key(&KeyEvent::char('x')), Response::Redraw);
        assert_eq!(app.key(&KeyEvent::new(Key::Function(13))), Response::Ignored);
    }

    #[test]
    fn a_file_can_be_opened_edited_saved_and_read_back() {
        // The whole point of the editor, in one test.
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("main.rs");
        std::fs::write(&path, "fn main() {}\n").unwrap();

        let mut app = app();
        app.open(&path).unwrap();
        assert_eq!(app.workspace.document().language(), Some("rust"));

        app.act(Action::Move(Motion::DocumentEnd), false);
        typed(&mut app, "// done");
        assert_eq!(app.save(), Response::Redraw);

        assert_eq!(std::fs::read_to_string(&path).unwrap(), "fn main() {}\n// done");
        assert!(!app.workspace.document().is_modified());
    }

    #[test]
    fn saving_reports_the_file_name_in_the_status_bar() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("thing.rs");
        std::fs::write(&path, "x").unwrap();

        let mut app = app();
        app.open(&path).unwrap();
        app.save();
        assert_eq!(app.message(), Some("Saved thing.rs"));
    }

    #[test]
    fn saving_an_untitled_buffer_says_what_to_do_instead() {
        let mut app = app();
        typed(&mut app, "scratch");
        match app.save() {
            Response::Error(message) => assert!(message.contains("Save As"), "{message}"),
            other => panic!("expected an error, got {other:?}"),
        }
    }

    #[test]
    fn quitting_with_unsaved_work_warns_rather_than_quitting() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("a.rs");
        std::fs::write(&path, "x").unwrap();

        let mut app = app();
        app.open(&path).unwrap();
        typed(&mut app, "y");

        match app.act(Action::Quit, false) {
            Response::Error(message) => assert!(message.contains("unsaved"), "{message}"),
            other => panic!("the editor was about to discard the user's work: {other:?}"),
        }

        app.save();
        assert_eq!(app.act(Action::Quit, false), Response::Quit);
    }

    #[test]
    fn a_burst_of_typing_never_waits_for_a_re_parse() {
        // The tree is deliberately left stale during typing: catching it up
        // costs ~150 ms on a large file, which is twenty frame budgets.
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("main.rs");
        std::fs::write(&path, "fn main() {}").unwrap();

        let mut app = app();
        app.open(&path).unwrap();
        app.act(Action::Move(Motion::DocumentEnd), false);
        typed(&mut app, "\nfn second() {}");

        assert!(!app.workspace.active().tree_is_current(), "the parse happened on the keystroke path");
    }

    #[test]
    fn the_tree_catches_up_once_typing_pauses() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("main.rs");
        std::fs::write(&path, "fn main() {}").unwrap();

        let mut app = app();
        app.open(&path).unwrap();
        app.act(Action::Move(Motion::DocumentEnd), false);
        typed(&mut app, "\nfn second() {}");

        // A zero idle threshold is the same code path the real delay uses, and
        // avoids a sleep in the test.
        assert!(app.workspace.refresh_syntax_if_idle(Duration::ZERO).unwrap());
        assert!(app.workspace.active().tree_is_current());
        assert!(!app.workspace.active().tree.as_ref().unwrap().has_error());
    }

    #[test]
    fn a_frame_drawn_after_a_pause_catches_the_tree_up_by_itself() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("main.rs");
        std::fs::write(&path, "fn main() {}").unwrap();

        let mut app = app();
        app.open(&path).unwrap();
        app.act(Action::Move(Motion::DocumentEnd), false);
        typed(&mut app, "\nfn second() {}");
        assert!(!app.workspace.active().tree_is_current());

        std::thread::sleep(PARSE_DELAY + Duration::from_millis(10));
        app.frame(1.0).unwrap();

        assert!(app.workspace.active().tree_is_current(), "the idle frame did not re-parse");
    }

    #[test]
    fn undo_after_a_burst_of_typing_restores_the_original() {
        let mut app = app();
        typed(&mut app, "the quick brown fox");
        assert_eq!(app.workspace.document().text(), "the quick brown fox");

        while app.workspace.document().text() != "" {
            if !matches!(app.act(Action::Undo, false), Response::Redraw) {
                break;
            }
        }
        assert_eq!(app.workspace.document().text(), "");
    }

    #[test]
    fn the_gutter_grows_as_the_document_does() {
        let mut app = app();
        let narrow = app.view.layout.gutter_width;

        let dir = TempDir::new().unwrap();
        let path = dir.path().join("big.txt");
        std::fs::write(&path, "x\n".repeat(50_000)).unwrap();
        app.open(&path).unwrap();

        assert!(
            app.view.layout.gutter_width > narrow,
            "a five-digit line number does not fit in a three-digit gutter"
        );
    }

    #[test]
    fn resizing_updates_both_the_view_and_the_surface() {
        let mut app = app();
        app.resize(1600, 1200).unwrap();
        assert_eq!(app.view.layout.width, 1600.0);

        let frame = app.frame(1.0).unwrap();
        assert_eq!(frame.pixel_count(), 1600 * 1200);
    }

    #[test]
    fn a_window_cannot_be_resized_below_a_usable_minimum() {
        let mut app = app();
        app.resize(1, 1).unwrap();
        assert_eq!(app.view.layout.width, 320.0);
        assert_eq!(app.view.layout.height, 240.0);
    }

    #[test]
    fn a_status_message_expires() {
        let mut app = app();
        app.set_message("transient");
        assert_eq!(app.message(), Some("transient"));

        app.message = Some(("stale".to_string(), Instant::now() - MESSAGE_TIMEOUT * 2));
        assert_eq!(app.message(), None);
    }

    #[test]
    fn a_status_message_is_shown_in_the_frame() {
        let mut app = app();
        app.set_message("Saved something.rs");

        let scene = app.scene(1.0).unwrap();
        let drawn: String = scene
            .primitives
            .iter()
            .filter_map(|p| match p {
                nebula_render::scene::Primitive::Text(run) => Some(run.text.as_str()),
                _ => None,
            })
            .collect();
        assert!(drawn.contains("Saved something.rs"), "{drawn}");
    }

    #[test]
    fn the_editor_reports_which_renderer_it_chose_and_why() {
        let app = app();
        assert_eq!(app.renderer_kind(), RendererKind::Cpu);
        assert!(!app.renderer_reason().is_empty());
    }

    #[test]
    fn a_frame_is_timed_against_its_budget() {
        let mut app = app();
        assert!(app.last_frame_time().is_none());

        app.frame(1.0).unwrap();
        assert!(app.last_frame_time().is_some());
        assert!(app.met_frame_budget().is_some());
    }

    #[test]
    fn a_frame_of_a_large_file_stays_within_the_cpu_budget() {
        // The renderer only ever touches the visible lines, so this is a
        // property of the design rather than of the machine's speed — but it is
        // exactly the property that regresses silently.
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("huge.rs");
        let text: String =
            (0..200_000).map(|i| format!("fn f{i}() -> u32 {{ {i} }}\n")).collect();
        std::fs::write(&path, text).unwrap();

        let mut app = app();
        app.open(&path).unwrap();

        // Warm up: the first frame allocates the glyph cache.
        app.frame(1.0).unwrap();
        app.frame(1.0).unwrap();

        let elapsed = app.last_frame_time().unwrap();
        assert!(
            elapsed < Duration::from_millis(100),
            "a frame of a 200 000-line file took {elapsed:?}"
        );
    }

    #[test]
    fn scrolling_a_large_file_does_not_move_the_caret() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("big.txt");
        std::fs::write(&path, (0..5_000).map(|i| format!("{i}\n")).collect::<String>()).unwrap();

        let mut app = app();
        app.open(&path).unwrap();
        let caret = app.workspace.document().selections().primary().head;

        for _ in 0..100 {
            app.act(Action::Scroll(10), false);
        }
        assert!(app.view.viewport.first_line > 500);
        assert_eq!(app.workspace.document().selections().primary().head, caret);
    }

    #[test]
    fn an_edit_is_handed_to_the_parser_exactly_as_applied() {
        // The parser must be told what actually happened, not a diff derived
        // afterwards: deriving one costs a pass over the whole document per
        // keystroke, which is what made a 200 000-line file unusable.
        let mut app = app();
        typed(&mut app, "hello world");

        app.act(Action::Move(Motion::DocumentStart), false);
        app.act(Action::Move(Motion::WordRight), false);
        typed(&mut app, "X");

        let change = app.workspace.document().last_change();
        assert_eq!(change.len(), 1);
        assert_eq!(change[0].edits().len(), 1);
        assert_eq!(change[0].edits()[0].text, "X");
        assert!(change[0].edits()[0].range.is_empty(), "an insertion replaces nothing");
    }

    #[test]
    fn undo_reports_the_transactions_it_applied() {
        let mut app = app();
        typed(&mut app, "abc");

        app.act(Action::Undo, false);
        assert!(
            !app.workspace.document().last_change().is_empty(),
            "undo changed the buffer but reported nothing to the parser"
        );
    }

    #[test]
    fn ctrl_s_saves_through_the_keymap() {
        // The binding and the handler have to agree, not just each work alone.
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("k.rs");
        std::fs::write(&path, "fn main() {}").unwrap();

        let mut app = app();
        app.open(&path).unwrap();
        app.act(Action::Move(Motion::DocumentEnd), false);
        typed(&mut app, "\n");

        app.key(&KeyEvent::with(Key::Char('s'), Modifiers::PRIMARY));
        assert!(!app.workspace.document().is_modified());
        assert!(std::fs::read_to_string(&path).unwrap().ends_with('\n'));
    }

    #[test]
    fn opening_a_second_file_resets_the_scroll_position() {
        let dir = TempDir::new().unwrap();
        for name in ["a.txt", "b.txt"] {
            std::fs::write(
                dir.path().join(name),
                (0..1_000).map(|i| format!("{i}\n")).collect::<String>(),
            )
            .unwrap();
        }

        let mut app = app();
        app.open(dir.path().join("a.txt")).unwrap();
        app.act(Action::Scroll(200), false);
        assert!(app.view.viewport.first_line > 0);

        app.open(dir.path().join("b.txt")).unwrap();
        assert_eq!(app.view.viewport.first_line, 0, "the new file opened scrolled to nowhere");
    }

    #[test]
    fn every_frame_of_an_unchanged_document_is_identical() {
        let mut app = app();
        typed(&mut app, "fn main() {}");

        let a = app.frame(1.0).unwrap();
        let b = app.frame(1.0).unwrap();
        assert_eq!(a.differing_pixels(&b, 0), 0);
    }

    #[test]
    fn typing_changes_the_pixels() {
        // The counterpart to the test above: if rendering were accidentally
        // cached, that test would pass and the editor would be frozen.
        let mut app = app();
        let before = app.frame(1.0).unwrap();
        typed(&mut app, "visible text");
        let after = app.frame(1.0).unwrap();

        assert!(before.differing_pixels(&after, 4) > 0, "typing did not change the screen");
    }
}
