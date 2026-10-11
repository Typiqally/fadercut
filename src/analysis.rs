//! EBU R128 loudness timeline via ffmpeg `ebur128` + `ametadata` print.
//!
//! Splits the subprocess boundary (`measure_timeline`) from the pure parser
//! (`parse_ebur128_metadata`) so the parsing is unit-testable without ffmpeg.

use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::Command;

use crate::ffutil::{clip_args, describe_status, tail, Clip};

/// Loudness values at or below this (including -inf for digital silence) are
/// reported as the floor so downstream maths stays finite.
const FLOOR: f64 = -120.0;
const ERROR_TAIL_CHARS: usize = 2000;

#[derive(Debug, thiserror::Error)]
pub enum AnalysisError {
    #[error("could not run ffmpeg: {0}")]
    Spawn(#[source] std::io::Error),
    #[error("could not read ebur128 metadata: {0}")]
    Io(#[from] std::io::Error),
    #[error("temporary path is not valid UTF-8: {0}")]
    TempPath(String),
    #[error("ebur128 analysis failed ({status}).\n{stderr_tail}")]
    Failed { status: String, stderr_tail: String },
    #[error("No ebur128 frames parsed from ametadata output.")]
    NoFrames,
    #[error("ebur128 metadata has an invalid frame timestamp: {0:?}")]
    BadTimestamp(String),
    #[error("ebur128 frame timestamps are not increasing at {0}s")]
    NonMonotonic(f64),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Timeline {
    /// Frame times in seconds (~10 Hz), strictly increasing.
    pub times: Vec<f64>,
    /// Momentary (400 ms) loudness, LUFS.
    pub momentary: Vec<f64>,
    /// Short-term (3 s) loudness, LUFS.
    pub short_term: Vec<f64>,
    /// Final integrated (gated program) loudness, LUFS.
    pub integrated: f64,
    /// Max true peak over the file, dBTP.
    pub true_peak: f64,
    /// Loudness range, LU.
    pub lra: f64,
}

/// Whole-programme EBU R128 figures.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LoudnessStats {
    /// Integrated loudness (LUFS).
    pub integrated: f64,
    /// True peak (dBTP).
    pub true_peak: f64,
    /// Loudness range (LU).
    pub lra: f64,
}

impl LoudnessStats {
    pub fn summary(&self) -> String {
        format!(
            "I={:+.1} LUFS  TP={:+.1} dBTP  LRA={:.1} LU",
            self.integrated, self.true_peak, self.lra
        )
    }
}

impl Timeline {
    pub fn stats(&self) -> LoudnessStats {
        LoudnessStats {
            integrated: self.integrated,
            true_peak: self.true_peak,
            lra: self.lra,
        }
    }
}

fn to_lufs(value: &str) -> f64 {
    match value.trim().parse::<f64>() {
        Ok(v) if v >= FLOOR => v,
        _ => FLOOR,
    }
}

/// `frame:N  pts:P  pts_time:T` -> `T`, or `None` if this is not a frame header.
fn frame_time(line: &str) -> Option<&str> {
    if !line.starts_with("frame:") {
        return None;
    }
    let mut tokens = line.split_whitespace();
    let frame = tokens.next()?.strip_prefix("frame:")?;
    if frame.is_empty() || !frame.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    tokens.next()?.strip_prefix("pts:")?;
    tokens
        .next()?
        .strip_prefix("pts_time:")
        .filter(|t| !t.is_empty())
}

#[derive(Default)]
struct Frame {
    time: Option<f64>,
    momentary: Option<f64>,
    short_term: Option<f64>,
}

impl Frame {
    /// Append this frame to the timeline if it is complete.
    fn flush_into(&self, timeline: &mut Timeline) -> Result<(), AnalysisError> {
        if let (Some(t), Some(m), Some(s)) = (self.time, self.momentary, self.short_term) {
            if timeline.times.last().is_some_and(|&prev| t <= prev) {
                return Err(AnalysisError::NonMonotonic(t));
            }
            timeline.times.push(t);
            timeline.momentary.push(m);
            timeline.short_term.push(s);
        }
        Ok(())
    }
}

/// Parse the `ametadata=mode=print` dump of an ebur128 stream.
///
/// Block format per frame:
///
/// ```text
/// frame:N  pts:..  pts_time:T
/// lavfi.r128.M=..
/// lavfi.r128.S=..
/// lavfi.r128.I=..
/// lavfi.r128.LRA=..
/// lavfi.r128.true_peak=..      (linear amplitude, with peak=true)
/// ```
pub fn parse_ebur128_metadata(reader: impl BufRead) -> Result<Timeline, AnalysisError> {
    let mut timeline = Timeline {
        times: Vec::new(),
        momentary: Vec::new(),
        short_term: Vec::new(),
        integrated: FLOOR,
        true_peak: FLOOR,
        lra: 0.0,
    };
    let mut max_peak_linear = 0.0_f64;
    let mut frame = Frame::default();

    for line in reader.lines() {
        let line = line?;
        if let Some(time) = frame_time(&line) {
            frame.flush_into(&mut timeline)?;
            let time = time
                .parse::<f64>()
                .ok()
                .filter(|t| t.is_finite())
                .ok_or_else(|| AnalysisError::BadTimestamp(time.to_owned()))?;
            frame = Frame {
                time: Some(time),
                ..Frame::default()
            };
        } else if let Some(v) = line.strip_prefix("lavfi.r128.M=") {
            frame.momentary = Some(to_lufs(v));
        } else if let Some(v) = line.strip_prefix("lavfi.r128.S=") {
            frame.short_term = Some(to_lufs(v));
        } else if let Some(v) = line.strip_prefix("lavfi.r128.I=") {
            timeline.integrated = to_lufs(v);
        } else if let Some(v) = line.strip_prefix("lavfi.r128.LRA=") {
            timeline.lra = to_lufs(v);
        } else if let Some(v) = line.strip_prefix("lavfi.r128.true_peak=") {
            if let Ok(peak) = v.trim().parse::<f64>() {
                max_peak_linear = max_peak_linear.max(peak);
            }
        }
    }
    frame.flush_into(&mut timeline)?;

    if timeline.times.is_empty() {
        return Err(AnalysisError::NoFrames);
    }
    if max_peak_linear > 0.0 {
        timeline.true_peak = (20.0 * max_peak_linear.log10()).max(FLOOR);
    }
    Ok(timeline)
}

/// A temporary file that an `ebur128,ametadata=mode=print` filter writes to.
pub struct MeterFile(tempfile::NamedTempFile);

impl MeterFile {
    pub fn new() -> Result<Self, AnalysisError> {
        let file = tempfile::Builder::new()
            .prefix("fadercut-ebur128-")
            .suffix(".txt")
            .tempfile()?;
        // The path is spliced into a filter string, so it has to be valid UTF-8.
        if file.path().to_str().is_none() {
            return Err(AnalysisError::TempPath(file.path().display().to_string()));
        }
        Ok(Self(file))
    }

    pub fn path(&self) -> &str {
        self.0.path().to_str().unwrap_or_default()
    }

    /// Parse what the most recent ffmpeg run wrote.
    pub fn read(&self) -> Result<Timeline, AnalysisError> {
        parse_ebur128_metadata(BufReader::new(File::open(self.0.path())?))
    }
}

/// Run ebur128 over the file (or a clip) and return the parsed loudness timeline.
pub fn measure_timeline(
    ffmpeg: &Path,
    input: &Path,
    clip: Option<Clip>,
) -> Result<Timeline, AnalysisError> {
    let meter = MeterFile::new()?;
    let out = Command::new(ffmpeg)
        .args(["-hide_banner", "-nostats"])
        .args(clip_args(clip))
        .arg("-i")
        .arg(input)
        .arg("-af")
        .arg(format!(
            "ebur128=metadata=1:peak=true,ametadata=mode=print:file={}",
            meter.path()
        ))
        .args(["-f", "null", "-"])
        .output()
        .map_err(AnalysisError::Spawn)?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        return Err(AnalysisError::Failed {
            status: describe_status(out.status),
            stderr_tail: tail(&stderr, ERROR_TAIL_CHARS).to_owned(),
        });
    }
    meter.read()
}

#[cfg(test)]
mod tests {
    use super::*;

    // Key names and units as printed by ffmpeg 9: true peaks are linear amplitude.
    const EBUR128_META: &str = "\
frame:0    pts:0       pts_time:0
lavfi.r128.M=-120.691
lavfi.r128.S=-120.691
lavfi.r128.I=-70.000
lavfi.r128.LRA=0.000
lavfi.r128.LRA.low=0.000
lavfi.r128.LRA.high=0.000
lavfi.r128.true_peaks_ch0=0.448
lavfi.r128.true_peaks_ch1=0.449
lavfi.r128.true_peak=0.449
frame:1    pts:4410    pts_time:0.1
lavfi.r128.M=-18.5
lavfi.r128.S=-19.0
lavfi.r128.I=-18.8
lavfi.r128.LRA=5.2
lavfi.r128.LRA.low=-20.000
lavfi.r128.LRA.high=-14.800
lavfi.r128.true_peak=0.861
frame:2    pts:8820    pts_time:0.2
lavfi.r128.M=-inf
lavfi.r128.S=-17.5
lavfi.r128.I=-18.0
lavfi.r128.LRA=5.5
lavfi.r128.true_peak=0.794
";

    #[test]
    fn parse_basic() {
        let tl = parse_ebur128_metadata(EBUR128_META.as_bytes()).unwrap();
        assert_eq!(tl.times, [0.0, 0.1, 0.2]);
        assert_eq!(tl.short_term, [-120.0, -19.0, -17.5]); // -120.691 clamped to floor
        assert_eq!(tl.momentary[2], -120.0); // -inf coerced to floor
        assert_eq!(tl.integrated, -18.0); // last I seen
        assert_eq!(tl.lra, 5.5); // last LRA, not the .low/.high variants
    }

    #[test]
    fn true_peak_is_max_linear_peak_in_db() {
        let tl = parse_ebur128_metadata(EBUR128_META.as_bytes()).unwrap();
        // max linear 0.861 -> -1.3 dBTP
        assert!((tl.true_peak - (-1.3)).abs() < 0.01, "{}", tl.true_peak);
    }

    #[test]
    fn true_peak_floors_without_peak_metadata() {
        let meta = "frame:0 pts:0 pts_time:0\nlavfi.r128.M=-20\nlavfi.r128.S=-20\n";
        let tl = parse_ebur128_metadata(meta.as_bytes()).unwrap();
        assert_eq!(tl.true_peak, -120.0);
    }

    #[test]
    fn incomplete_frames_are_dropped() {
        let meta = "\
frame:0 pts:0 pts_time:0
lavfi.r128.M=-20
frame:1 pts:4410 pts_time:0.1
lavfi.r128.M=-20
lavfi.r128.S=-21
";
        let tl = parse_ebur128_metadata(meta.as_bytes()).unwrap();
        assert_eq!(tl.times, [0.1]);
    }

    #[test]
    fn stats_summarise_the_timeline() {
        let stats = parse_ebur128_metadata(EBUR128_META.as_bytes())
            .unwrap()
            .stats();
        assert_eq!(stats.summary(), "I=-18.0 LUFS  TP=-1.3 dBTP  LRA=5.5 LU");
    }

    #[test]
    fn summary_shows_sign_on_positive_values() {
        let stats = LoudnessStats {
            integrated: 0.0,
            true_peak: 0.44,
            lra: 0.0,
        };
        assert_eq!(stats.summary(), "I=+0.0 LUFS  TP=+0.4 dBTP  LRA=0.0 LU");
    }

    #[test]
    fn empty_raises() {
        let err = parse_ebur128_metadata("no frames here".as_bytes()).unwrap_err();
        assert!(matches!(err, AnalysisError::NoFrames));
    }

    #[test]
    fn bad_timestamp_raises() {
        let meta = "frame:0 pts:0 pts_time:soon\n";
        let err = parse_ebur128_metadata(meta.as_bytes()).unwrap_err();
        assert!(matches!(err, AnalysisError::BadTimestamp(_)));
    }

    #[test]
    fn non_increasing_timestamps_raise() {
        let meta = "\
frame:0 pts:0 pts_time:0.1
lavfi.r128.M=-20
lavfi.r128.S=-20
frame:1 pts:0 pts_time:0.1
lavfi.r128.M=-20
lavfi.r128.S=-20
";
        let err = parse_ebur128_metadata(meta.as_bytes()).unwrap_err();
        assert!(matches!(err, AnalysisError::NonMonotonic(_)));
    }
}
