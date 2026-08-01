//! # nebula-ide
//!
//! The editor itself: the application state, the configuration, and the window.
//!
//! ## What lives where
//!
//! - [`workspace`] holds the open files and their parse trees. No window.
//! - [`app`] turns actions into changes and frames. No event loop.
//! - [`window`] is the winit/wgpu shell, and is deliberately thin — everything
//!   it does is a two-line call into [`app`].
//!
//! That layering is what makes the editor testable end to end. A test, or the
//! stress harness in CI, constructs an [`app::App`], sends keystrokes, saves
//! files and inspects the rendered pixels, with no display server anywhere.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod app;
pub mod config;
pub mod session;
pub mod workspace;

#[cfg(feature = "gui")]
pub mod window;

pub use app::{App, Response};
pub use config::Config;
pub use workspace::Workspace;

use std::path::PathBuf;

/// Errors from the editor.
#[derive(Debug, thiserror::Error)]
pub enum IdeError {
    /// A file could not be opened.
    #[error("could not open {path}: {source}")]
    Open {
        /// The file.
        path: PathBuf,
        /// Why.
        source: std::io::Error,
    },

    /// A file could not be written.
    #[error("could not save {path}: {source}")]
    Save {
        /// The file.
        path: PathBuf,
        /// Why.
        source: std::io::Error,
    },

    /// The document has never been saved and has no name to save under.
    #[error("this document has no file name")]
    NoPath,

    /// Closing would discard unsaved changes.
    #[error("there are unsaved changes")]
    UnsavedChanges,

    /// The document layer failed.
    #[error(transparent)]
    Core(#[from] nebula_core::CoreError),

    /// The syntax layer failed.
    #[error(transparent)]
    Syntax(#[from] nebula_syntax::SyntaxError),

    /// The renderer failed.
    #[error(transparent)]
    Render(#[from] nebula_render::RenderError),

    /// The UI layer failed.
    #[error(transparent)]
    Ui(#[from] nebula_ui::UiError),

    /// The filesystem layer failed.
    #[error(transparent)]
    Vfs(#[from] nebula_vfs::VfsError),

    /// Plain I/O.
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// Convenience result alias.
pub type Result<T, E = IdeError> = std::result::Result<T, E>;

/// The editor's version, from the crate metadata.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
