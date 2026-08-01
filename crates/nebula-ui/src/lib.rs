//! # nebula-ui
//!
//! The editor's presentation layer: what is on screen, where, and what a
//! keystroke does to it.
//!
//! ## Why this crate has no window in it
//!
//! [`EditorView`] turns a document plus a viewport into a
//! [`nebula_render::Scene`], and [`input`] turns a key event into an action on a
//! document. Neither touches a window, a GPU, or an event loop. That separation
//! is what lets the whole interface be tested — a test opens a document, sends
//! keystrokes, and asserts on the resulting scene or the resulting text, with no
//! display server involved.
//!
//! The window and event loop live in `nebula-ide`, and they are thin.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod input;
pub mod layout;
pub mod theme;
pub mod view;

pub use input::{Action, KeyEvent, Modifiers, keymap};
pub use layout::{Layout, Viewport};
pub use theme::Theme;
pub use view::EditorView;

/// Errors from the UI layer.
#[derive(Debug, thiserror::Error)]
pub enum UiError {
    /// A document operation failed.
    #[error(transparent)]
    Core(#[from] nebula_core::CoreError),

    /// Syntax highlighting failed.
    #[error(transparent)]
    Syntax(#[from] nebula_syntax::SyntaxError),

    /// Rendering failed.
    #[error(transparent)]
    Render(#[from] nebula_render::RenderError),

    /// A theme could not be parsed.
    #[error("invalid theme: {0}")]
    Theme(String),
}

/// Convenience result alias.
pub type Result<T, E = UiError> = std::result::Result<T, E>;
