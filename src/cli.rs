//! Command-line interface: argument definitions and value parsing.

use std::path::PathBuf;

use clap::{Parser, ValueEnum};

use crate::ffutil::Clip;
use crate::leveler::Profile;
use crate::presets::PresetName;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, ValueEnum)]
pub enum Leveling {
    /// No riding: constant gain, limiter only if peaks need it
    #[default]
    Off,
    /// Ease section-to-section loudness differences
    Gentle,
    /// Correct more of the difference, with larger moves
    Strong,
}

impl Leveling {
    pub fn profile(self) -> Option<Profile> {
        match self {
            Leveling::Off => None,
            Leveling::Gentle => Some(Profile::Gentle),
            Leveling::Strong => Some(Profile::Strong),
        }
    }
}

/// The loudness to master to.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Target {
    /// Integrated loudness (LUFS).
    Lufs(f64),
    /// As loud as the set goes while the limiter stays within its clean budget.
    Max,
}

/// Master a DJ set: even out loudness, tame spikes, hit a target.
#[derive(Debug, Parser)]
#[command(name = "fadercut", version)]
pub struct Args {
    /// Path to the input audio file (e.g. set.wav)
    pub input: PathBuf,

    /// Mastering preset
    #[arg(long, value_enum, default_value_t)]
    pub preset: PresetName,

    /// Section-to-section loudness riding
    #[arg(long, value_enum, default_value_t)]
    pub leveling: Leveling,

    /// Asymmetric leveling: lift quiet sections but leave loud ones
    /// (drops/climax/build) almost untouched
    #[arg(long)]
    pub preserve_loud: bool,

    /// Fold lows below HZ to mono for club translation [default HZ: 120]
    #[arg(long, value_name = "HZ", num_args = 0..=1, default_missing_value = "120",
          value_parser = parse_hz)]
    pub bass_mono: Option<f64>,

    /// High-pass below HZ to remove inaudible subsonic rumble [default HZ: 28]
    #[arg(long, value_name = "HZ", num_args = 0..=1, default_missing_value = "28",
          value_parser = parse_hz)]
    pub subsonic: Option<f64>,

    /// Process only a time range to audition fast, e.g. 0:30-1:30, 90-150 or 1:02:50-1:03:20
    #[arg(long, value_name = "START-END", value_parser = parse_preview)]
    pub preview: Option<Clip>,

    /// Override integrated loudness (LUFS), or `max`: the loudest the set goes
    /// before the limiter audibly works
    #[arg(long, value_name = "LUFS|max", allow_negative_numbers = true,
          value_parser = parse_target)]
    pub target: Option<Target>,

    /// Override the true-peak ceiling (dBTP, -20 to 0)
    #[arg(long, value_name = "DBTP", allow_negative_numbers = true,
          value_parser = parse_ceiling)]
    pub tp: Option<f64>,

    /// Output path [default: <input>-mastered.wav, or -preview.wav]
    #[arg(long)]
    pub output: Option<PathBuf>,

    /// Overwrite output if it exists
    #[arg(long)]
    pub force: bool,

    /// Measure and print commands without rendering
    #[arg(long)]
    pub dry_run: bool,
}

const MIN_CEILING_DB: f64 = -20.0;

fn parse_finite(s: &str) -> Result<f64, String> {
    match s.trim().parse::<f64>() {
        Ok(v) if v.is_finite() => Ok(v),
        _ => Err(format!("{s:?} is not a finite number")),
    }
}

fn parse_target(s: &str) -> Result<Target, String> {
    if s.trim().eq_ignore_ascii_case("max") {
        return Ok(Target::Max);
    }
    parse_finite(s)
        .map(Target::Lufs)
        .map_err(|_| format!("{s:?} is not a loudness in LUFS or `max`"))
}

/// The limiter cannot be set above full scale or below about -24 dBFS.
fn parse_ceiling(s: &str) -> Result<f64, String> {
    match parse_finite(s)? {
        tp if (MIN_CEILING_DB..=0.0).contains(&tp) => Ok(tp),
        _ => Err(format!(
            "ceiling must be between {MIN_CEILING_DB} and 0 dBTP"
        )),
    }
}

fn parse_hz(s: &str) -> Result<f64, String> {
    match parse_finite(s)? {
        hz if hz > 0.0 => Ok(hz),
        _ => Err("frequency must be greater than 0 Hz".to_owned()),
    }
}

/// `90` -> 90.0, `1:30` -> 90.0, `0:05.5` -> 5.5, `1:02:03` -> 3723.0.
fn parse_time(token: &str) -> Result<f64, String> {
    let token = token.trim();
    let bad = || format!("invalid time {token:?} (use seconds, mm:ss or h:mm:ss)");
    let mut parts: Vec<&str> = token.split(':').collect();
    let seconds = parts.pop().unwrap_or_default();
    if parts.len() > 2 {
        return Err(bad());
    }
    let mut total = parse_finite(seconds).map_err(|_| bad())?;
    if total < 0.0 {
        return Err(bad());
    }
    // Minutes, then hours, each worth sixty of the unit to its right.
    let mut unit = 60.0;
    for part in parts.iter().rev() {
        let count: u32 = part.parse().map_err(|_| bad())?;
        total += f64::from(count) * unit;
        unit *= 60.0;
    }
    Ok(total)
}

/// Parse `START-END` (seconds, mm:ss or h:mm:ss) into a clip.
pub fn parse_preview(spec: &str) -> Result<Clip, String> {
    let (start, end) = spec
        .split_once('-')
        .ok_or("preview must be START-END, e.g. 0:30-1:30 or 90-150")?;
    let (start, end) = (parse_time(start)?, parse_time(end)?);
    if end <= start {
        return Err(format!("preview end ({end}) must be after start ({start})"));
    }
    Ok(Clip {
        start,
        duration: end - start,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(argv: &[&str]) -> Result<Args, clap::Error> {
        Args::try_parse_from(std::iter::once("fadercut").chain(argv.iter().copied()))
    }

    #[test]
    fn cli_definition_is_consistent() {
        use clap::CommandFactory;
        Args::command().debug_assert();
    }

    #[test]
    fn defaults() {
        let args = parse(&["set.wav"]).unwrap();
        assert_eq!(args.preset, PresetName::Club);
        assert_eq!(args.leveling, Leveling::Off);
        assert_eq!((args.bass_mono, args.subsonic), (None, None));
        assert!(!args.force && !args.dry_run && !args.preserve_loud);
    }

    #[test]
    fn low_end_flags_take_optional_frequency() {
        let args = parse(&["set.wav", "--bass-mono", "--subsonic"]).unwrap();
        assert_eq!((args.bass_mono, args.subsonic), (Some(120.0), Some(28.0)));
        let args = parse(&["set.wav", "--bass-mono", "100", "--subsonic=35"]).unwrap();
        assert_eq!((args.bass_mono, args.subsonic), (Some(100.0), Some(35.0)));
    }

    #[test]
    fn low_end_rejects_non_positive_frequency() {
        assert!(parse(&["set.wav", "--bass-mono", "0"]).is_err());
        assert!(parse(&["set.wav", "--subsonic=-5"]).is_err());
    }

    #[test]
    fn negative_targets_are_accepted() {
        let args = parse(&["set.wav", "--target", "-10", "--tp", "-1.5"]).unwrap();
        assert_eq!(
            (args.target, args.tp),
            (Some(Target::Lufs(-10.0)), Some(-1.5))
        );
        assert!(parse(&["set.wav", "--target", "nan"]).is_err());
    }

    #[test]
    fn target_accepts_max() {
        let args = parse(&["set.wav", "--target", "max"]).unwrap();
        assert_eq!(args.target, Some(Target::Max));
        assert_eq!(
            parse(&["set.wav", "--target=MAX"]).unwrap().target,
            Some(Target::Max)
        );
        assert!(parse(&["set.wav", "--target", "loud"]).is_err());
    }

    #[test]
    fn ceiling_must_be_within_the_limiter_range() {
        assert!(parse(&["set.wav", "--tp", "0"]).is_ok());
        assert!(parse(&["set.wav", "--tp", "0.5"]).is_err());
        assert!(parse(&["set.wav", "--tp", "-30"]).is_err());
    }

    #[test]
    fn enum_values_use_kebab_case() {
        let args = parse(&["set.wav", "--preset", "even-only", "--leveling", "strong"]).unwrap();
        assert_eq!(args.preset, PresetName::EvenOnly);
        assert_eq!(args.leveling.profile(), Some(Profile::Strong));
        assert!(parse(&["set.wav", "--preset", "nope"]).is_err());
    }

    #[test]
    fn parse_preview_seconds() {
        let clip = parse_preview("90-150").unwrap();
        assert_eq!((clip.start, clip.duration), (90.0, 60.0));
    }

    #[test]
    fn parse_preview_mmss() {
        let clip = parse_preview("0:30-1:30").unwrap();
        assert_eq!((clip.start, clip.duration), (30.0, 60.0));
        assert_eq!(parse_time("0:05.5"), Ok(5.5));
    }

    #[test]
    fn parse_preview_hours() {
        let clip = parse_preview("1:02:50-1:03:20").unwrap();
        assert_eq!((clip.start, clip.duration), (3770.0, 30.0));
    }

    #[test]
    fn parse_preview_rejects_bad_range() {
        assert!(parse_preview("1:30-0:30").is_err()); // end before start
        assert!(parse_preview("90").is_err()); // no range
        assert!(parse_preview("a-b").is_err());
        assert!(parse_preview("-5-10").is_err());
        assert!(parse_preview("1:2:3:4-1:2:3:9").is_err());
        assert!(parse_preview("1:xx-2:00").is_err());
    }
}
