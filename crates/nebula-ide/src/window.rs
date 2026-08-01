//! The window and the event loop.
//!
//! This module is deliberately thin. It translates winit events into
//! [`nebula_ui`] key events, hands them to [`App`], and asks for a redraw when
//! the app says something changed. Every decision — what a key means, what to
//! draw, whether to save — is made in [`crate::app`], where it can be tested.
//!
//! Compiled only with the `gui` feature, so a headless build (CI, a server, a
//! container) does not need a windowing library present at all.

use std::sync::Arc;

use winit::application::ApplicationHandler;
use winit::event::{ElementState, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{Key as WinitKey, ModifiersState, NamedKey};
use winit::window::{Window, WindowId};

use nebula_ui::input::{Key, KeyEvent, Modifiers};
use nebula_ui::{Action, input};

use crate::app::{App, Response};
use crate::{IdeError, Result};

/// Run the editor with a window until the user quits.
pub fn run(app: App) -> Result<()> {
    let event_loop = EventLoop::new().map_err(|error| {
        IdeError::Io(std::io::Error::other(format!("could not start an event loop: {error}")))
    })?;

    // `Wait` rather than `Poll`: an editor that is not being typed into should
    // use no CPU at all, which is the difference between a laptop that lasts
    // the afternoon and one that does not.
    event_loop.set_control_flow(ControlFlow::Wait);

    let mut handler = Shell { app, window: None, modifiers: ModifiersState::empty() };
    event_loop.run_app(&mut handler).map_err(|error| {
        IdeError::Io(std::io::Error::other(format!("the event loop failed: {error}")))
    })
}

struct Shell {
    app: App,
    window: Option<Arc<Window>>,
    modifiers: ModifiersState,
}

impl ApplicationHandler for Shell {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }

        let attributes = Window::default_attributes()
            .with_title("Nebula")
            .with_inner_size(winit::dpi::LogicalSize::new(
                self.app.config.window[0],
                self.app.config.window[1],
            ))
            .with_min_inner_size(winit::dpi::LogicalSize::new(320.0, 240.0))
            .with_theme(Some(if self.app.view.theme.dark {
                winit::window::Theme::Dark
            } else {
                winit::window::Theme::Light
            }));

        match event_loop.create_window(attributes) {
            Ok(window) => {
                let window = Arc::new(window);
                let size = window.inner_size();
                if let Err(error) = self.app.resize(size.width, size.height) {
                    tracing::error!(%error, "could not size the surface");
                }
                window.request_redraw();
                self.window = Some(window);
            }
            Err(error) => {
                tracing::error!(%error, "could not create a window");
                event_loop.exit();
            }
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => {
                if self.app.workspace.has_unsaved_changes() {
                    // Refusing to close on unsaved work would trap the user, so
                    // the warning is shown once and the next request goes
                    // through.
                    self.app.set_message(
                        "Unsaved changes. Press Ctrl+S to save, or close again to discard."
                            .to_string(),
                    );
                    self.request_redraw();
                    return;
                }
                event_loop.exit();
            }

            WindowEvent::Resized(size) => {
                if let Err(error) = self.app.resize(size.width, size.height) {
                    tracing::error!(%error, "resize failed");
                }
                self.request_redraw();
            }

            WindowEvent::ModifiersChanged(modifiers) => {
                self.modifiers = modifiers.state();
            }

            WindowEvent::KeyboardInput { event, .. } => {
                if event.state != ElementState::Pressed {
                    return;
                }
                let Some(key) = translate(&event.logical_key) else { return };

                let response = self.app.key(&KeyEvent {
                    key,
                    modifiers: modifiers_from(self.modifiers),
                    repeat: event.repeat,
                });
                self.handle(event_loop, response);
            }

            WindowEvent::MouseWheel { delta, .. } => {
                // Three lines per notch is the platform convention; a pixel
                // delta from a trackpad is converted through the line height so
                // both devices scroll at the same rate.
                let lines = match delta {
                    MouseScrollDelta::LineDelta(_, y) => -(y as f64 * 3.0),
                    MouseScrollDelta::PixelDelta(position) => {
                        -position.y / self.app.view.layout.line_height as f64
                    }
                };
                let response = self.app.act(Action::Scroll(lines.round() as isize), false);
                self.handle(event_loop, response);
            }

            WindowEvent::RedrawRequested => {
                let scale = self.window.as_ref().map(|w| w.scale_factor()).unwrap_or(1.0);
                if let Err(error) = self.app.frame(scale as f32) {
                    tracing::error!(%error, "the frame could not be drawn");
                }
            }

            _ => {}
        }
    }
}

impl Shell {
    fn request_redraw(&self) {
        if let Some(window) = &self.window {
            window.request_redraw();
        }
    }

    fn handle(&mut self, event_loop: &ActiveEventLoop, response: Response) {
        match response {
            Response::Quit => event_loop.exit(),
            Response::Redraw => self.request_redraw(),
            Response::Error(message) => {
                self.app.set_message(message);
                self.request_redraw();
            }
            Response::Message(message) => {
                self.app.set_message(message);
                self.request_redraw();
            }
            Response::Ignored => {}
        }
    }
}

/// Turn winit's key into the editor's.
fn translate(key: &WinitKey) -> Option<Key> {
    match key {
        WinitKey::Named(named) => Some(match named {
            NamedKey::Enter => Key::Enter,
            NamedKey::Tab => Key::Tab,
            NamedKey::Backspace => Key::Backspace,
            NamedKey::Delete => Key::Delete,
            NamedKey::Escape => Key::Escape,
            NamedKey::ArrowLeft => Key::Left,
            NamedKey::ArrowRight => Key::Right,
            NamedKey::ArrowUp => Key::Up,
            NamedKey::ArrowDown => Key::Down,
            NamedKey::Home => Key::Home,
            NamedKey::End => Key::End,
            NamedKey::PageUp => Key::PageUp,
            NamedKey::PageDown => Key::PageDown,
            NamedKey::Space => Key::Char(' '),
            NamedKey::F1 => Key::Function(1),
            NamedKey::F2 => Key::Function(2),
            NamedKey::F3 => Key::Function(3),
            NamedKey::F4 => Key::Function(4),
            NamedKey::F5 => Key::Function(5),
            NamedKey::F6 => Key::Function(6),
            NamedKey::F7 => Key::Function(7),
            NamedKey::F8 => Key::Function(8),
            NamedKey::F9 => Key::Function(9),
            NamedKey::F10 => Key::Function(10),
            NamedKey::F11 => Key::Function(11),
            NamedKey::F12 => Key::Function(12),
            // Modifier presses on their own, media keys, and everything else
            // the editor has no meaning for.
            _ => return None,
        }),

        // The logical key has already been through the keyboard layout, so a
        // French AZERTY or a Dvorak layout produces the character the user sees
        // printed on the key.
        WinitKey::Character(text) => text.chars().next().map(Key::Char),

        WinitKey::Dead(_) | WinitKey::Unidentified(_) => None,
    }
}

fn modifiers_from(state: ModifiersState) -> Modifiers {
    Modifiers {
        ctrl: state.control_key(),
        shift: state.shift_key(),
        alt: state.alt_key(),
        meta: state.super_key(),
    }
}

/// How many spaces one indent is, re-exported so the window and the editor
/// cannot disagree about it.
pub const INDENT_WIDTH: usize = input::INDENT_WIDTH;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn named_keys_map_onto_the_editors_keys() {
        assert_eq!(translate(&WinitKey::Named(NamedKey::Enter)), Some(Key::Enter));
        assert_eq!(translate(&WinitKey::Named(NamedKey::ArrowLeft)), Some(Key::Left));
        assert_eq!(translate(&WinitKey::Named(NamedKey::F5)), Some(Key::Function(5)));
        assert_eq!(translate(&WinitKey::Named(NamedKey::Space)), Some(Key::Char(' ')));
    }

    #[test]
    fn a_lone_modifier_press_produces_no_key() {
        // Otherwise tapping Shift would insert something.
        assert_eq!(translate(&WinitKey::Named(NamedKey::Shift)), None);
        assert_eq!(translate(&WinitKey::Named(NamedKey::Control)), None);
    }

    #[test]
    fn a_character_key_carries_the_layouts_character() {
        let key = WinitKey::Character("é".into());
        assert_eq!(translate(&key), Some(Key::Char('é')));
    }

    #[test]
    fn a_dead_key_is_ignored_until_it_composes() {
        assert_eq!(translate(&WinitKey::Dead(Some('´'))), None);
    }

    #[test]
    fn modifier_state_is_carried_across_unchanged() {
        let state = ModifiersState::CONTROL | ModifiersState::SHIFT;
        let modifiers = modifiers_from(state);
        assert!(modifiers.ctrl);
        assert!(modifiers.shift);
        assert!(!modifiers.alt);
        assert!(!modifiers.meta);
    }

    #[test]
    fn the_indent_width_matches_the_editors() {
        assert_eq!(INDENT_WIDTH, input::INDENT_WIDTH);
    }
}
