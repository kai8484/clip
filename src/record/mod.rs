//! Recording pipeline: screen & audio capture, GPU encoding, RAM ring buffer, and clip saving.

pub mod audio;
mod capture;
pub mod clock;
mod convert;
mod encoder;
mod game;
mod mux;
pub mod recorder;
