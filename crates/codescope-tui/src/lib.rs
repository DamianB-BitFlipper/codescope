//! `codescope-tui` — the Ratatui interface.
//!
//! This crate renders a [`snapshot::UiSnapshot`] and emits [`action::Action`]s. It never
//! calls git, a language server, or an AI provider; the binary assembles snapshots and
//! dispatches work actions. Everything is testable headlessly via ratatui's `TestBackend`.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod action;
pub mod app;
mod canvas;
pub mod diagram;
pub mod divider;
pub mod elide;
pub mod file_rows;
pub mod geometry;
pub mod intraline;
pub mod layout;
pub mod mouse;
pub mod render;
pub mod run;
pub mod scroll;
pub mod snapshot;

pub use action::{map_key, Action, PlanNodeTarget};
pub use app::{App, Pane, UiPreferences};
pub use divider::{DividerId, DividerSizes};
pub use render::render;
pub use snapshot::UiSnapshot;
