//! Gain-riding leveler — mimics an engineer riding the fader by section.
//!
//! Given an EBU R128 loudness timeline, compute a slow gain (in dB, per frame) that
//! pulls each section toward the program loudness, while:
//!
//!   * GATING  — sections whose momentary loudness sits far below the program mean
//!     (breakdowns, drops, quiet intros) are excluded from correction and the gain
//!     is held across them, so they are never boosted or ducked.
//!   * PARTIAL — only a fraction of the deviation is corrected, easing the spread
//!     instead of flattening the musical energy arc.
//!   * CLAMP   — the gain is bounded (engineers make small moves).
//!   * SLEW    — the rate of change is capped, which structurally prevents pumping
//!     and any fast loud->quiet move (the hard constraint).
//!
//! All functions are pure. No ffmpeg, no I/O.

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LevelingParams {
    /// Fraction of deviation corrected when boosting quiet sections.
    pub up_strength: f64,
    /// Fraction corrected when cutting loud sections (low => preserve loud).
    pub down_strength: f64,
    /// Clamp on |gain| (dB).
    pub max_gain_db: f64,
    /// Max rate of gain change (dB/s).
    pub slew_db_per_s: f64,
    /// Exclude frames more than this many LU below program loudness.
    pub gate_rel_lu: f64,
    /// Section-loudness smoothing window (s).
    pub smooth_s: f64,
    /// Leave this long of audio after silence untouched (drops/intros).
    pub onset_hold_s: f64,
    pub abs_gate_lufs: f64,
}

/// `down_strength` when `--preserve-loud` is set: loud sections barely touched.
pub const PRESERVE_LOUD_DOWN: f64 = 0.15;

/// Leading intro is held at 0 dB until the groove first comes within this of program.
const INTRO_MARGIN_LU: f64 = 2.0;

const DEFAULT_ONSET_HOLD_S: f64 = 4.0;
const DEFAULT_ABS_GATE_LUFS: f64 = -60.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Profile {
    Gentle,
    Strong,
}

impl Profile {
    pub fn name(self) -> &'static str {
        match self {
            Profile::Gentle => "gentle",
            Profile::Strong => "strong",
        }
    }

    pub fn params(self) -> LevelingParams {
        match self {
            Profile::Gentle => LevelingParams {
                up_strength: 0.6,
                down_strength: 0.6,
                max_gain_db: 3.0,
                slew_db_per_s: 0.3,
                gate_rel_lu: 10.0,
                smooth_s: 15.0,
                onset_hold_s: DEFAULT_ONSET_HOLD_S,
                abs_gate_lufs: DEFAULT_ABS_GATE_LUFS,
            },
            Profile::Strong => LevelingParams {
                up_strength: 0.85,
                down_strength: 0.85,
                max_gain_db: 4.0,
                slew_db_per_s: 0.4,
                gate_rel_lu: 12.0,
                smooth_s: 12.0,
                onset_hold_s: DEFAULT_ONSET_HOLD_S,
                abs_gate_lufs: DEFAULT_ABS_GATE_LUFS,
            },
        }
    }
}

/// Median spacing between consecutive frame times.
fn median_step(times: &[f64]) -> f64 {
    let mut steps: Vec<f64> = times.windows(2).map(|w| w[1] - w[0]).collect();
    steps.sort_by(f64::total_cmp);
    let mid = steps.len() / 2;
    if steps.len() % 2 == 1 {
        steps[mid]
    } else {
        (steps[mid - 1] + steps[mid]) / 2.0
    }
}

/// A duration expressed in whole timeline frames (at least one).
fn frames(seconds: f64, dt: f64) -> usize {
    ((seconds / dt).round_ties_even() as usize).max(1)
}

/// Centered moving average over masked-true frames only (gated frames excluded).
///
/// Frames with no valid neighbours in the window are `None`.
fn masked_moving_average(y: &[f64], mask: &[bool], win: usize) -> Vec<Option<f64>> {
    let n = y.len();
    let half = win / 2;
    let mut sum = Vec::with_capacity(n + 1);
    let mut count = Vec::with_capacity(n + 1);
    sum.push(0.0);
    count.push(0usize);
    for (&v, &m) in y.iter().zip(mask) {
        sum.push(sum[sum.len() - 1] + if m { v } else { 0.0 });
        count.push(count[count.len() - 1] + usize::from(m));
    }
    (0..n)
        .map(|i| {
            let a = i.saturating_sub(half);
            let b = (i + half + 1).min(n);
            let cnt = count[b] - count[a];
            (cnt > 0).then(|| (sum[b] - sum[a]) / cnt as f64)
        })
        .collect()
}

/// Count of consecutive true values ending at each index.
fn consecutive_true(mask: &[bool]) -> Vec<usize> {
    let mut run = 0;
    mask.iter()
        .map(|&v| {
            run = if v { run + 1 } else { 0 };
            run
        })
        .collect()
}

/// Limit the per-step change of `g` to +/- `max_step`.
fn slew_limit(g: &[f64], max_step: f64) -> Vec<f64> {
    let mut out = Vec::with_capacity(g.len());
    let mut prev: Option<f64> = None;
    for &target in g {
        let next = match prev {
            None => target,
            Some(p) => p + (target - p).clamp(-max_step, max_step),
        };
        out.push(next);
        prev = Some(next);
    }
    out
}

/// Return the per-frame gain (dB) implementing gated, partial, slewed riding.
///
/// `times` must be strictly increasing; all slices must have the same length.
pub fn compute_gain_db(
    times: &[f64],
    short_term: &[f64],
    momentary: &[f64],
    integrated: f64,
    params: &LevelingParams,
) -> Vec<f64> {
    let n = times.len();
    assert!(
        short_term.len() == n && momentary.len() == n,
        "timeline series must have equal lengths"
    );
    if n < 2 {
        return vec![0.0; n];
    }
    let dt = median_step(times);

    // Foreground = loud enough to be program material (BS.1770-style relative gate).
    let foreground: Vec<bool> = momentary
        .iter()
        .map(|&m| m > params.abs_gate_lufs && m > integrated - params.gate_rel_lu)
        .collect();

    // Section loudness: moving average over foreground frames only, so silence/
    // breakdowns don't drag the section level down (that caused over-boost + drift).
    let section = masked_moving_average(short_term, &foreground, frames(params.smooth_s, dt));

    // "Established" = foreground that has been continuous for onset_hold_s. This
    // exempts onsets emerging from silence (intros, drops): silent->loud is left
    // alone, so we only level material that is already running.
    let hold = frames(params.onset_hold_s, dt);
    let mut established: Vec<bool> = foreground
        .iter()
        .zip(consecutive_true(&foreground))
        .map(|(&fg, run)| fg && run >= hold)
        .collect();

    // Centre the ride on the average loudness of established material (variance-
    // reducing / loudness-neutral); overall level is set by make-up gain.
    let levels: Vec<f64> = established
        .iter()
        .zip(&section)
        .filter_map(|(&est, &level)| level.filter(|_| est))
        .collect();
    if levels.is_empty() {
        return vec![0.0; n];
    }
    let reference = levels.iter().sum::<f64>() / levels.len() as f64;

    // Leading intro: hold gain at 0 dB through the opening build until the groove
    // first reaches program level, so the intro comes in at its natural level
    // instead of being boosted (it reads as "quiet" only because it builds in).
    let reached = established
        .iter()
        .zip(&section)
        .position(|(&est, level)| est && level.is_some_and(|s| s >= reference - INTRO_MARGIN_LU));
    if let Some(first) = reached {
        established[..first].fill(false);
    }

    // Asymmetric correction: boost quiet (deviation > 0) with up_strength, cut loud
    // (deviation < 0) with down_strength. Applied only to established material; the
    // gain is held across onsets/silences (and stays at 0 dB before any is seen).
    let mut held = 0.0;
    let desired: Vec<f64> = established
        .iter()
        .zip(&section)
        .map(|(&est, &level)| {
            if let Some(level) = level.filter(|_| est) {
                let deviation = reference - level;
                let strength = if deviation >= 0.0 {
                    params.up_strength
                } else {
                    params.down_strength
                };
                held = (strength * deviation).clamp(-params.max_gain_db, params.max_gain_db);
            }
            held
        })
        .collect();

    slew_limit(&desired, params.slew_db_per_s * dt)
}

pub fn gain_to_linear(gain_db: &[f64]) -> Vec<f64> {
    gain_db.iter().map(|&g| 10f64.powf(g / 20.0)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params(
        up: f64,
        down: f64,
        max_gain_db: f64,
        slew: f64,
        gate: f64,
        smooth: f64,
    ) -> LevelingParams {
        LevelingParams {
            up_strength: up,
            down_strength: down,
            max_gain_db,
            slew_db_per_s: slew,
            gate_rel_lu: gate,
            smooth_s: smooth,
            onset_hold_s: DEFAULT_ONSET_HOLD_S,
            abs_gate_lufs: DEFAULT_ABS_GATE_LUFS,
        }
    }

    fn times(n: usize) -> Vec<f64> {
        (0..n).map(|i| i as f64 * 0.1).collect()
    }

    fn two_sections(first: f64, second: f64) -> Vec<f64> {
        [vec![first; 200], vec![second; 200]].concat()
    }

    #[test]
    fn slew_limit_caps_rate_of_change() {
        let out = slew_limit(&[0.0, 10.0, 10.0, -10.0], 1.0);
        assert_eq!(out, [0.0, 1.0, 2.0, 1.0]);
    }

    #[test]
    fn median_step_handles_odd_and_even_counts() {
        assert_eq!(median_step(&[0.0, 1.0, 3.0]), 1.5);
        assert_eq!(median_step(&[0.0, 1.0, 3.0, 4.0]), 1.0);
    }

    #[test]
    fn frames_rounds_and_never_returns_zero() {
        assert_eq!(frames(15.0, 0.1), 150);
        assert_eq!(frames(0.0, 0.1), 1);
    }

    #[test]
    fn masked_average_ignores_gated_frames() {
        let y = [-10.0, -50.0, -10.0, -50.0, -50.0, -50.0];
        let mask = [true, false, true, false, false, false];
        let out = masked_moving_average(&y, &mask, 3);
        assert_eq!(out[1], Some(-10.0));
        assert_eq!(out[4], None); // no foreground frame within the window
    }

    #[test]
    fn consecutive_true_resets_on_false() {
        assert_eq!(consecutive_true(&[true, true, false, true]), [1, 2, 0, 1]);
    }

    /// A short loud->quiet->loud dip (gated) must not be boosted.
    #[test]
    fn quiet_section_is_held_not_boosted() {
        let mut s = vec![-9.0; 300];
        s[140..160].fill(-45.0); // a breakdown well below program
        let g = compute_gain_db(
            &times(300),
            &s,
            &s,
            -9.0,
            &params(0.6, 0.6, 6.0, 0.5, 10.0, 10.0),
        );
        assert!(g[150] < 1.0);
    }

    /// Uniform loudness has no variance to correct -> ride ~0 (level is make-up's job).
    #[test]
    fn loudness_neutral_on_uniform_input() {
        let s = vec![-12.0; 400];
        let g = compute_gain_db(
            &times(400),
            &s,
            &s,
            -9.0,
            &params(0.6, 0.6, 6.0, 1.0, 10.0, 5.0),
        );
        assert!(g.iter().all(|v| v.abs() < 1e-6));
    }

    /// Quiet section boosted, loud section cut, toward the average (partial).
    /// Loud section first so the leading-intro exemption doesn't apply.
    #[test]
    fn reduces_variance_between_sections() {
        let s = two_sections(-6.0, -12.0);
        let g = compute_gain_db(
            &times(400),
            &s,
            &s,
            -9.0,
            &params(0.6, 0.6, 6.0, 2.0, 20.0, 3.0),
        );
        assert!(g[100] < -1.0); // loud half pulled down
        assert!(g[300] > 1.0); // quiet half pulled up
    }

    /// Asymmetric: quiet boosted fully, loud barely cut (preserve build).
    #[test]
    fn preserve_loud_lifts_quiet_but_keeps_loud() {
        let s = two_sections(-6.0, -12.0);
        let p = params(0.6, PRESERVE_LOUD_DOWN, 6.0, 2.0, 20.0, 3.0);
        let g = compute_gain_db(&times(400), &s, &s, -9.0, &p);
        assert!(g[300] > 1.0);
        assert!(-0.7 < g[100] && g[100] < 0.0);
    }

    #[test]
    fn respects_clamp() {
        let s = two_sections(-30.0, -5.0); // huge spread
        let g = compute_gain_db(
            &times(400),
            &s,
            &s,
            -17.0,
            &params(1.0, 1.0, 3.0, 2.0, 40.0, 3.0),
        );
        assert!(g.iter().all(|v| v.abs() <= 3.0 + 1e-9));
    }

    /// The opening build is left at 0 dB until the groove reaches program level.
    #[test]
    fn leading_intro_is_not_boosted() {
        let s = two_sections(-16.0, -9.0); // quiet intro, then the groove
        let g = compute_gain_db(
            &times(400),
            &s,
            &s,
            -10.0,
            &params(0.6, 0.6, 6.0, 2.0, 20.0, 3.0),
        );
        assert!(g[..150].iter().all(|v| v.abs() < 1e-9));
    }

    /// A mid-set drop (loud -> silence -> quiet) resumes at the pre-silence gain
    /// with onset hold, instead of jumping to a fresh boost.
    #[test]
    fn onset_after_midset_silence_holds_gain() {
        let s = [vec![-8.0; 100], vec![-120.0; 20], vec![-12.0; 180]].concat();
        let base = params(0.6, 0.6, 6.0, 2.0, 20.0, 3.0);
        let no_hold = LevelingParams {
            onset_hold_s: 0.0,
            ..base
        };
        let held = compute_gain_db(&times(300), &s, &s, -9.0, &base);
        let nohold = compute_gain_db(&times(300), &s, &s, -9.0, &no_hold);
        let pre = held[118]; // gain just before the silence
        assert!((held[130] - pre).abs() < 0.3);
        assert!(nohold[130] > held[130] + 0.8);
    }

    #[test]
    fn short_or_silent_timelines_get_unity_gain() {
        let p = Profile::Gentle.params();
        assert!(compute_gain_db(&[], &[], &[], -9.0, &p).is_empty());
        assert_eq!(compute_gain_db(&[0.0], &[-9.0], &[-9.0], -9.0, &p), [0.0]);
        let silent = vec![-120.0; 50];
        let g = compute_gain_db(&times(50), &silent, &silent, -70.0, &p);
        assert!(g.iter().all(|&v| v == 0.0));
    }

    #[test]
    fn gain_to_linear_converts_db() {
        let lin = gain_to_linear(&[0.0, 6.0206]);
        assert!((lin[0] - 1.0).abs() < 1e-3 && (lin[1] - 2.0).abs() < 1e-3);
    }

    #[test]
    fn strong_profile_corrects_more_than_gentle() {
        let (gentle, strong) = (Profile::Gentle.params(), Profile::Strong.params());
        assert!(strong.up_strength > gentle.up_strength);
        assert!(strong.max_gain_db > gentle.max_gain_db);
    }
}
