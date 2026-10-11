//! fadercut — reusable DJ-set mastering CLI.
//!
//! Tames reverb/transient peaks and (optionally) evens out section-to-section
//! loudness the way a mastering engineer rides the fader, then hits a target loudness
//! under a true-peak ceiling. All audio DSP except the gain ride is delegated to
//! `ffmpeg`.

pub mod analysis;
pub mod apply;
pub mod aup;
pub mod chain;
pub mod cli;
pub mod error;
pub mod ffutil;
pub mod hotspots;
pub mod landing;
pub mod leveler;
pub mod pipeline;
pub mod presets;
pub mod progress;

pub use cli::Args;
pub use error::Error;
pub use pipeline::run;
