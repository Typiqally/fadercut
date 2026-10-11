//! Top-level error type and its mapping to process exit codes.

use crate::analysis::AnalysisError;
use crate::apply::ApplyError;
use crate::aup::AupError;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Bad invocation or a failed preflight check — nothing was processed.
    #[error("{0}")]
    Preflight(String),
    #[error("input has no measurable loudness (silent audio)")]
    SilentInput,
    #[error(transparent)]
    Analysis(#[from] AnalysisError),
    #[error(transparent)]
    Apply(#[from] ApplyError),
    #[error(transparent)]
    Project(#[from] AupError),
    #[error("render failed ({status}).\n{stderr_tail}")]
    Render { status: String, stderr_tail: String },
    #[error(
        "true peak {measured:+.2} dBTP is still above the {ceiling:+.2} dBTP ceiling after \
         {attempts} passes; nothing was written"
    )]
    CeilingExceeded {
        measured: f64,
        ceiling: f64,
        attempts: usize,
    },
    #[error("could not run ffmpeg: {0}")]
    Spawn(#[source] std::io::Error),
    #[error("could not move the finished render to {path}: {source}")]
    Finalize {
        path: String,
        #[source]
        source: std::io::Error,
    },
}

impl Error {
    /// 2 = nothing was attempted (usage/preflight), 1 = processing failed.
    pub fn exit_code(&self) -> u8 {
        match self {
            Error::Preflight(_) => 2,
            _ => 1,
        }
    }
}
