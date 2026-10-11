//! Landing a render on its targets.
//!
//! The gain needed to hit a loudness target cannot be computed up front: whatever the
//! limiter (or the low-end filters, or the gain ride) removes is only known once the
//! audio has been rendered. So each render is measured, and the next attempt is
//! corrected by the measured error until loudness and true peak are both on target.
//!
//! Pure decision logic — the renderer is injected, so this is testable without audio.

use crate::analysis::LoudnessStats;
use crate::chain::RenderPlan;

/// Headroom between the limiter threshold and the ceiling. Resampling back down from
/// the limiter's oversampled rate re-introduces ~0.1 dB of overshoot.
pub const LIMITER_MARGIN_DB: f64 = 0.2;

/// A render within this of the loudness target counts as landed.
pub const LOUDNESS_TOLERANCE_LU: f64 = 0.3;

/// Measured true peak may exceed the ceiling by at most this (meter resolution).
pub const PEAK_TOLERANCE_DB: f64 = 0.05;

/// Extra amount the limiter is lowered by, on top of a measured overshoot.
const PEAK_RETRY_MARGIN_DB: f64 = 0.1;

/// `alimiter` accepts thresholds down to 0.0625 (-24.08 dBFS).
const MIN_LIMIT_DB: f64 = -24.0;

/// Renders are expensive; the correction converges geometrically, so a few suffice.
pub const MAX_ATTEMPTS: usize = 4;

/// What the finished master must measure.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Goal {
    /// Integrated loudness (LUFS).
    pub loudness: f64,
    /// True-peak ceiling (dBTP).
    pub ceiling_db: f64,
}

impl Goal {
    fn default_limit_db(&self) -> f64 {
        self.ceiling_db - LIMITER_MARGIN_DB
    }

    pub fn loudness_ok(&self, measured: &LoudnessStats) -> bool {
        (self.loudness - measured.integrated).abs() <= LOUDNESS_TOLERANCE_LU
    }

    pub fn peak_ok(&self, measured: &LoudnessStats) -> bool {
        measured.true_peak <= self.ceiling_db + PEAK_TOLERANCE_DB
    }

    /// The first attempt: the gain that would land the target if nothing were lost,
    /// with the limiter only if the gained peaks would reach its threshold.
    ///
    /// `extra_peak_db` is how much later stages may raise peaks beyond the constant
    /// gain (the gain ride's largest boost, phase rotation in the low-end filters).
    pub fn first_plan(&self, before: &LoudnessStats, extra_peak_db: f64) -> RenderPlan {
        let gain_db = self.loudness - before.integrated;
        let predicted_peak = before.true_peak + gain_db + extra_peak_db;
        let limit_db = self.default_limit_db();
        RenderPlan {
            gain_db,
            limit_db: (predicted_peak > limit_db).then_some(limit_db),
        }
    }

    /// Judge a rendered attempt: `None` if it is on target, otherwise the corrected
    /// plan for the next attempt.
    pub fn review(&self, plan: &RenderPlan, measured: &LoudnessStats) -> Option<RenderPlan> {
        let (loudness_ok, peak_ok) = (self.loudness_ok(measured), self.peak_ok(measured));
        if loudness_ok && peak_ok {
            return None;
        }
        let gain_step = if loudness_ok {
            0.0
        } else {
            self.loudness - measured.integrated
        };
        let limit_db = match plan.limit_db {
            // The limiter let too much through: lower it by the overshoot.
            Some(limit) if !peak_ok => {
                let overshoot = measured.true_peak - self.ceiling_db;
                Some((limit - overshoot - PEAK_RETRY_MARGIN_DB).max(MIN_LIMIT_DB))
            }
            Some(limit) => Some(limit),
            // No limiter yet: add one if the corrected gain brings peaks up to it.
            None => {
                let limit = self.default_limit_db();
                (measured.true_peak + gain_step > limit).then_some(limit)
            }
        };
        Some(RenderPlan {
            gain_db: plan.gain_db + gain_step,
            limit_db,
        })
    }
}

/// The accepted render and how it measured.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Landing {
    pub plan: RenderPlan,
    pub after: LoudnessStats,
    pub attempts: usize,
    pub loudness_ok: bool,
    pub peak_ok: bool,
}

/// Render, measure and correct until the goal is met or attempts run out.
///
/// `render` receives the 1-based attempt number and the plan, renders it, and returns
/// the measurement of what it rendered. The last attempt is returned even if it missed;
/// the caller decides what a miss means.
pub fn land<E>(
    goal: &Goal,
    first: RenderPlan,
    mut render: impl FnMut(usize, &RenderPlan) -> Result<LoudnessStats, E>,
) -> Result<Landing, E> {
    let mut plan = first;
    let mut attempts = 1;
    loop {
        let after = render(attempts, &plan)?;
        match goal.review(&plan, &after) {
            Some(next) if attempts < MAX_ATTEMPTS => {
                plan = next;
                attempts += 1;
            }
            _ => {
                return Ok(Landing {
                    plan,
                    after,
                    attempts,
                    loudness_ok: goal.loudness_ok(&after),
                    peak_ok: goal.peak_ok(&after),
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SOURCE: LoudnessStats = LoudnessStats {
        integrated: -15.2,
        true_peak: -5.1,
        lra: 4.5,
    };
    const CLUB: Goal = Goal {
        loudness: -9.0,
        ceiling_db: -1.0,
    };
    const STREAMING: Goal = Goal {
        loudness: -14.0,
        ceiling_db: -1.0,
    };

    fn stats(integrated: f64, true_peak: f64) -> LoudnessStats {
        LoudnessStats {
            integrated,
            true_peak,
            lra: 4.0,
        }
    }

    /// A crude limiter: peaks above the threshold are pulled down to it (plus some
    /// overshoot), and a fraction of that reduction is lost from the loudness.
    fn simulate(plan: &RenderPlan, overshoot_db: f64, loss_ratio: f64) -> LoudnessStats {
        let peak = SOURCE.true_peak + plan.gain_db;
        let loud = SOURCE.integrated + plan.gain_db;
        match plan.limit_db {
            Some(limit) if peak > limit => {
                stats(loud - (peak - limit) * loss_ratio, limit + overshoot_db)
            }
            _ => stats(loud, peak),
        }
    }

    fn never_fails(stats: LoudnessStats) -> Result<LoudnessStats, ()> {
        Ok(stats)
    }

    #[test]
    fn first_plan_adds_the_limiter_only_when_peaks_would_reach_it() {
        let loud = CLUB.first_plan(&SOURCE, 0.0);
        assert!((loud.gain_db - 6.2).abs() < 1e-9);
        assert_eq!(loud.limit_db, Some(-1.2));

        let quiet = STREAMING.first_plan(&SOURCE, 0.0);
        assert!((quiet.gain_db - 1.2).abs() < 1e-9);
        assert_eq!(quiet.limit_db, None);
    }

    #[test]
    fn first_plan_accounts_for_peaks_raised_by_later_stages() {
        // -5.1 + 1.2 = -3.9 dBTP is clear of the limiter, but not with +3 dB on top.
        assert_eq!(STREAMING.first_plan(&SOURCE, 3.0).limit_db, Some(-1.2));
    }

    #[test]
    fn review_accepts_a_render_within_tolerance() {
        let plan = CLUB.first_plan(&SOURCE, 0.0);
        assert_eq!(CLUB.review(&plan, &stats(-9.25, -1.0)), None);
        assert_eq!(CLUB.review(&plan, &stats(-8.75, -0.96)), None);
    }

    #[test]
    fn review_adds_the_missing_loudness_to_the_gain() {
        let plan = CLUB.first_plan(&SOURCE, 0.0);
        let next = CLUB.review(&plan, &stats(-9.8, -1.1)).unwrap();
        assert!((next.gain_db - (plan.gain_db + 0.8)).abs() < 1e-9);
        assert_eq!(next.limit_db, plan.limit_db);
    }

    #[test]
    fn review_lowers_the_limiter_by_a_measured_overshoot() {
        let plan = CLUB.first_plan(&SOURCE, 0.0);
        let next = CLUB.review(&plan, &stats(-9.0, -0.7)).unwrap();
        assert_eq!(next.gain_db, plan.gain_db); // loudness was fine
        assert!((next.limit_db.unwrap() - (-1.2 - 0.3 - 0.1)).abs() < 1e-9);
    }

    #[test]
    fn review_adds_a_limiter_when_an_unlimited_render_overshoots() {
        let plan = RenderPlan {
            gain_db: 1.2,
            limit_db: None,
        };
        let next = STREAMING.review(&plan, &stats(-14.0, -0.4)).unwrap();
        assert_eq!(next.limit_db, Some(-1.2));
    }

    #[test]
    fn review_never_asks_for_a_threshold_the_limiter_rejects() {
        let goal = Goal {
            loudness: -30.0,
            ceiling_db: -20.0,
        };
        let plan = RenderPlan {
            gain_db: 0.0,
            limit_db: Some(-23.9),
        };
        let next = goal.review(&plan, &stats(-30.0, -18.0)).unwrap();
        assert_eq!(next.limit_db, Some(MIN_LIMIT_DB));
    }

    #[test]
    fn land_accepts_an_unlimited_render_first_time() {
        let first = STREAMING.first_plan(&SOURCE, 0.0);
        let landing = land(&STREAMING, first, |_, plan| {
            never_fails(simulate(plan, 0.0, 0.3))
        })
        .unwrap();
        assert_eq!(landing.attempts, 1);
        assert_eq!(landing.plan.limit_db, None);
        assert!(landing.loudness_ok && landing.peak_ok);
    }

    #[test]
    fn land_recovers_the_loudness_the_limiter_removed() {
        let first = CLUB.first_plan(&SOURCE, 0.0);
        let mut seen = Vec::new();
        let landing = land(&CLUB, first, |attempt, plan| {
            seen.push(attempt);
            never_fails(simulate(plan, 0.1, 0.35))
        })
        .unwrap();
        // First pass: 2.3 dB over the threshold costs 0.8 LU, so one correction.
        assert_eq!(seen, [1, 2]);
        assert!(landing.plan.gain_db > first.gain_db);
        assert!(landing.loudness_ok && landing.peak_ok);
    }

    #[test]
    fn land_tightens_a_limiter_that_overshoots_the_ceiling() {
        let first = CLUB.first_plan(&SOURCE, 0.0);
        let landing = land(&CLUB, first, |_, plan| {
            never_fails(simulate(plan, 0.5, 0.0))
        })
        .unwrap();
        assert!(landing.attempts > 1);
        assert!(landing.plan.limit_db.unwrap() < first.limit_db.unwrap());
        assert!(landing.peak_ok);
    }

    #[test]
    fn land_reports_a_target_the_material_cannot_reach() {
        // Every dB over the threshold is lost: loudness cannot rise past the limiter.
        let first = CLUB.first_plan(&SOURCE, 0.0);
        let landing = land(&CLUB, first, |_, plan| {
            never_fails(simulate(plan, 0.0, 1.0))
        })
        .unwrap();
        assert_eq!(landing.attempts, MAX_ATTEMPTS);
        assert!(!landing.loudness_ok);
        assert!(landing.peak_ok);
    }

    #[test]
    fn land_stops_at_the_first_render_error() {
        let first = CLUB.first_plan(&SOURCE, 0.0);
        let mut calls = 0;
        let result = land(&CLUB, first, |_, _| {
            calls += 1;
            Err::<LoudnessStats, _>("render failed")
        });
        assert_eq!(result, Err("render failed"));
        assert_eq!(calls, 1);
    }
}
