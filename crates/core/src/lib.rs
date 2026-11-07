//! UI-free core of WoW is a Feeling: everything except drawing.
//!
//! The catalog builder and the app both depend on this crate, so a UI swap
//! would only ever cost the UI layer. See `docs/DESIGN.md`.

pub mod backup;
pub mod catalog;
pub mod deps;
pub mod details;
pub mod detect;
pub mod error;
pub mod extract;
pub mod gameproc;
pub mod install;
pub mod listfile;
pub mod logging;
pub mod net;
pub mod paths;
pub mod scan;
pub mod search;
pub mod selfupdate;
pub mod state;
pub mod toc;

pub use error::{Error, Result};
