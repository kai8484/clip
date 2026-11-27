//! Shared core library for clip (recording, configuration, clip library, and utilities).
//!
//! Public API lints do not apply: this is an internal app library, not a published crate.
#![allow(clippy::new_without_default)]
#![allow(clippy::missing_safety_doc)]
#![allow(clippy::result_unit_err)]

pub mod app;
pub mod config;
#[cfg(feature = "devtools")]
pub mod devtools;
pub mod library;
pub mod log;
pub mod mf;
pub mod record;
pub mod share;
pub mod ui_kit;
