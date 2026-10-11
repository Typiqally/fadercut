//! Pure construction of ffmpeg filter chains. No I/O, no subprocess — unit-testable.
//!
//! Mastering chain, in order:
//!
//! ```text
//! [subsonic high-pass] -> [bass-mono split] -> volume -> [oversampled limiter]
//!                                              (constant   (only when peaks would
//!                                               gain)       exceed the ceiling)
//! ```
//!
//! There is deliberately no time-varying program gain here (no dynaudnorm/leveler),
//! and no waveshaping: the limiter is the only stage that changes dynamics.

use crate::presets::Preset;

/// The limiter runs at no less than this rate so that it sees inter-sample peaks.
const MIN_LIMITER_RATE_HZ: u32 = 176_400;

/// Resampler filter length around the limiter. Longer than ffmpeg's default (32) so
/// the up/down round trip is transparent in the audio band.
const RESAMPLE_FILTER_SIZE: u32 = 64;

/// An ffmpeg filter spec plus how it must be passed on the command line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Filter {
    pub spec: String,
    /// `true` => a `-filter_complex` graph ending in `[out]`; `false` => `-af`.
    pub complex: bool,
}

impl Filter {
    pub fn ffmpeg_args(&self) -> Vec<String> {
        if self.complex {
            vec![
                "-filter_complex".to_owned(),
                self.spec.clone(),
                "-map".to_owned(),
                "[out]".to_owned(),
            ]
        } else {
            vec!["-af".to_owned(), self.spec.clone()]
        }
    }
}

/// Optional low-end processing applied ahead of the gain/limiter tail.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct LowEnd {
    /// High-pass corner (Hz) removing inaudible subsonic rumble.
    pub subsonic_hz: Option<f64>,
    /// Fold everything below this frequency (Hz) to mono.
    pub bass_mono_hz: Option<f64>,
}

impl LowEnd {
    pub fn enabled(&self) -> bool {
        self.subsonic_hz.is_some() || self.bass_mono_hz.is_some()
    }

    /// Bass-mono needs a band split/merge, which requires a filter_complex graph.
    pub fn needs_complex(&self) -> bool {
        self.bass_mono_hz.is_some()
    }
}

/// One render attempt: a constant gain and, when needed, a limiter.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RenderPlan {
    /// Gain applied before the limiter (dB).
    pub gain_db: f64,
    /// Limiter threshold (dBFS), or `None` to leave the limiter out entirely so the
    /// render is a pure gain change.
    pub limit_db: Option<f64>,
}

pub fn db_to_linear(db: f64) -> f64 {
    10f64.powf(db / 20.0)
}

/// Constant gain (dB) to move measured integrated loudness to the target.
pub fn gain_db(target_i: f64, measured_i: f64) -> f64 {
    target_i - measured_i
}

/// How many times the limiter oversamples at this sample rate.
pub fn oversample_factor(sample_rate: u32) -> u32 {
    MIN_LIMITER_RATE_HZ.div_ceil(sample_rate.max(1)).max(1)
}

fn volume(gain_db: f64) -> String {
    format!("volume={gain_db:.2}dB")
}

/// Look-ahead limiter run at an oversampled rate, so inter-sample (true) peaks are
/// limited rather than only sample values.
///
/// `level=disabled` => no auto make-up gain. `latency=1` compensates the look-ahead
/// delay; without it the delay is a fractional sample at the base rate.
pub fn limiter(preset: &Preset, limit_db: f64, sample_rate: u32) -> Vec<String> {
    let alimiter = format!(
        "alimiter=limit={:.6}:level=disabled:attack={}:release={}:latency=1",
        db_to_linear(limit_db),
        preset.lim_attack_ms,
        preset.lim_release_ms
    );
    let factor = oversample_factor(sample_rate);
    if factor == 1 {
        return vec![alimiter];
    }
    let resample = |rate: u32| format!("aresample={rate}:filter_size={RESAMPLE_FILTER_SIZE}");
    vec![
        resample(sample_rate * factor),
        alimiter,
        resample(sample_rate),
    ]
}

/// The complete mastering filter for one render attempt. Every path — peaks-only or
/// leveling, with or without low-end processing — uses this same chain.
///
/// With `tap_file`, a limited render also writes an ebur128 metadata dump of the
/// signal entering the limiter there, so the caller can tell what the limiter cost.
pub fn master_filter(
    preset: &Preset,
    plan: &RenderPlan,
    low_end: LowEnd,
    sample_rate: u32,
    tap_file: Option<&str>,
) -> Filter {
    let gain = volume(plan.gain_db);
    let limiter = plan
        .limit_db
        .map(|limit_db| limiter(preset, limit_db, sample_rate));

    if let (Some(limiter), Some(file)) = (&limiter, tap_file) {
        let head = graph_body(&[gain, "asplit=2[lim][tap]".to_owned()], low_end);
        let limiter = limiter.join(",");
        return Filter {
            spec: format!(
                "{head};[tap]ebur128=metadata=1,ametadata=mode=print:file={file},anullsink;\
                 [lim]{limiter}[out]"
            ),
            complex: true,
        };
    }

    let mut tail = vec![gain];
    tail.extend(limiter.into_iter().flatten());
    if low_end.enabled() {
        return Filter {
            spec: format!("{}[out]", graph_body(&tail, low_end)),
            complex: low_end.needs_complex(),
        };
    }
    Filter {
        spec: tail.join(","),
        complex: false,
    }
}

/// Build the graph `[0:a] ...` (left open, without an output label) that applies:
///
/// 1. subsonic high-pass (removes inaudible rumble, frees headroom)
/// 2. bass-mono below `bass_mono_hz` via an LR4 band split: low band summed to
///    mono, high band kept stereo
/// 3. the linear `tail` filters (gain, limiter)
fn graph_body(tail: &[String], low_end: LowEnd) -> String {
    let pre: Vec<String> = low_end
        .subsonic_hz
        .map(|hz| format!("highpass=f={hz}"))
        .into_iter()
        .collect();

    let Some(hz) = low_end.bass_mono_hz else {
        let body = [pre.as_slice(), tail].concat().join(",");
        return format!("[0:a]{body}");
    };

    let lr4_lp = format!("lowpass=f={hz},lowpass=f={hz}");
    let lr4_hp = format!("highpass=f={hz},highpass=f={hz}");
    let head = [pre.as_slice(), &["asplit=2[lo][hi]".to_owned()]]
        .concat()
        .join(",");
    let mono = "pan=stereo|c0=0.5*c0+0.5*c1|c1=0.5*c0+0.5*c1";
    let merge = [&["amix=inputs=2:normalize=0".to_owned()], tail]
        .concat()
        .join(",");
    format!("[0:a]{head};[lo]{lr4_lp},{mono}[lom];[hi]{lr4_hp}[hist];[lom][hist]{merge}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::presets::PresetName;

    fn club() -> Preset {
        PresetName::Club.preset()
    }

    fn tail() -> Vec<String> {
        vec!["volume=0dB".to_owned(), "alimiter=limit=0.9".to_owned()]
    }

    const GAIN_ONLY: RenderPlan = RenderPlan {
        gain_db: 7.2,
        limit_db: None,
    };
    const LIMITED: RenderPlan = RenderPlan {
        gain_db: 7.2,
        limit_db: Some(-1.2),
    };

    #[test]
    fn gain_db_moves_measured_to_target() {
        assert!((gain_db(-9.0, -16.2) - 7.2).abs() < 1e-9);
    }

    #[test]
    fn gain_db_negative_when_source_louder_than_target() {
        assert!((gain_db(-14.0, -10.0) + 4.0).abs() < 1e-9);
    }

    #[test]
    fn oversample_factor_reaches_the_minimum_limiter_rate() {
        assert_eq!(oversample_factor(44_100), 4);
        assert_eq!(oversample_factor(48_000), 4);
        assert_eq!(oversample_factor(88_200), 2);
        assert_eq!(oversample_factor(96_000), 2);
        assert_eq!(oversample_factor(176_400), 1);
        assert_eq!(oversample_factor(192_000), 1);
    }

    #[test]
    fn limiter_is_oversampled_and_delay_compensated() {
        assert_eq!(
            limiter(&club(), -1.2, 44_100),
            [
                "aresample=176400:filter_size=64",
                "alimiter=limit=0.870964:level=disabled:attack=5:release=100:latency=1",
                "aresample=44100:filter_size=64",
            ]
        );
    }

    #[test]
    fn limiter_skips_resampling_at_high_sample_rates() {
        let stages = limiter(&club(), -1.2, 192_000);
        assert_eq!(stages.len(), 1);
        assert!(stages[0].starts_with("alimiter="));
    }

    #[test]
    fn master_filter_without_limiter_is_a_pure_gain() {
        let f = master_filter(&club(), &GAIN_ONLY, LowEnd::default(), 44_100, None);
        assert_eq!(f.spec, "volume=7.20dB");
        assert!(!f.complex);
        assert_eq!(f.ffmpeg_args(), ["-af", "volume=7.20dB"]);
    }

    #[test]
    fn master_filter_puts_gain_before_the_limiter() {
        let f = master_filter(&club(), &LIMITED, LowEnd::default(), 48_000, None);
        let parts: Vec<&str> = f.spec.split(',').collect();
        assert_eq!(parts[0], "volume=7.20dB");
        assert_eq!(parts[1], "aresample=192000:filter_size=64");
        assert!(parts[2].starts_with("alimiter=limit=0.870964:"));
        assert_eq!(parts[3], "aresample=48000:filter_size=64");
    }

    #[test]
    fn master_filter_never_shapes_the_waveform() {
        for plan in [GAIN_ONLY, LIMITED] {
            let f = master_filter(&club(), &plan, LowEnd::default(), 44_100, None);
            for banned in ["asoftclip", "acompressor", "dynaudnorm", "loudnorm"] {
                assert!(!f.spec.contains(banned), "{banned} in {}", f.spec);
            }
        }
    }

    #[test]
    fn master_filter_uses_the_same_tail_with_low_end_processing() {
        let low_end = LowEnd {
            subsonic_hz: Some(28.0),
            bass_mono_hz: Some(120.0),
        };
        let plain = master_filter(&club(), &LIMITED, LowEnd::default(), 44_100, None);
        let low = master_filter(&club(), &LIMITED, low_end, 44_100, None);
        assert!(low.complex);
        assert!(low.spec.ends_with(&format!(",{}[out]", plain.spec)));
        assert_eq!(
            low.ffmpeg_args(),
            ["-filter_complex", low.spec.as_str(), "-map", "[out]"]
        );
    }

    #[test]
    fn tap_meters_the_signal_entering_the_limiter() {
        let f = master_filter(
            &club(),
            &LIMITED,
            LowEnd::default(),
            44_100,
            Some("/t/tap.txt"),
        );
        assert!(f.complex);
        assert_eq!(
            f.spec,
            "[0:a]volume=7.20dB,asplit=2[lim][tap];\
             [tap]ebur128=metadata=1,ametadata=mode=print:file=/t/tap.txt,anullsink;\
             [lim]aresample=176400:filter_size=64,\
             alimiter=limit=0.870964:level=disabled:attack=5:release=100:latency=1,\
             aresample=44100:filter_size=64[out]"
        );
    }

    #[test]
    fn tap_sits_after_low_end_processing_and_gain() {
        let low_end = LowEnd {
            subsonic_hz: Some(28.0),
            bass_mono_hz: Some(120.0),
        };
        let f = master_filter(&club(), &LIMITED, low_end, 44_100, Some("/t/tap.txt"));
        assert!(f
            .spec
            .contains("amix=inputs=2:normalize=0,volume=7.20dB,asplit=2[lim][tap];"));
        assert!(f.spec.ends_with("aresample=44100:filter_size=64[out]"));
    }

    #[test]
    fn no_tap_without_a_limiter() {
        let f = master_filter(
            &club(),
            &GAIN_ONLY,
            LowEnd::default(),
            44_100,
            Some("/t/tap.txt"),
        );
        assert_eq!(f.spec, "volume=7.20dB");
    }

    #[test]
    fn build_graph_subsonic_only_is_linear() {
        let low_end = LowEnd {
            subsonic_hz: Some(28.0),
            bass_mono_hz: None,
        };
        assert_eq!(
            format!("{}[out]", graph_body(&tail(), low_end)),
            "[0:a]highpass=f=28,volume=0dB,alimiter=limit=0.9[out]"
        );
        assert!(!low_end.needs_complex());
    }

    #[test]
    fn build_graph_bass_mono_splits_and_merges() {
        let low_end = LowEnd {
            subsonic_hz: Some(28.0),
            bass_mono_hz: Some(120.0),
        };
        assert_eq!(
            format!("{}[out]", graph_body(&tail(), low_end)),
            "[0:a]highpass=f=28,asplit=2[lo][hi];\
             [lo]lowpass=f=120,lowpass=f=120,pan=stereo|c0=0.5*c0+0.5*c1|c1=0.5*c0+0.5*c1[lom];\
             [hi]highpass=f=120,highpass=f=120[hist];\
             [lom][hist]amix=inputs=2:normalize=0,volume=0dB,alimiter=limit=0.9[out]"
        );
        assert!(low_end.needs_complex());
    }

    #[test]
    fn low_end_enabled() {
        assert!(!LowEnd::default().enabled());
        let subsonic = LowEnd {
            subsonic_hz: Some(28.0),
            bass_mono_hz: None,
        };
        let bass_mono = LowEnd {
            subsonic_hz: None,
            bass_mono_hz: Some(120.0),
        };
        assert!(subsonic.enabled() && bass_mono.enabled());
    }
}
