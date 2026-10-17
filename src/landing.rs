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

/// The limiter threshold that holds a true-peak ceiling.
fn default_limit_db(ceiling_db: f64) -> f64 {
    ceiling_db - LIMITER_MARGIN_DB
}

fn peak_within(ceiling_db: f64, measured: &LoudnessStats) -> bool {
    measured.true_peak <= ceiling_db + PEAK_TOLERANCE_DB
}

impl Goal {
    fn default_limit_db(&self) -> f64 {
        default_limit_db(self.ceiling_db)
    }

    pub fn loudness_ok(&self, measured: &LoudnessStats) -> bool {
        (self.loudness - measured.integrated).abs() <= LOUDNESS_TOLERANCE_LU
    }

    pub fn peak_ok(&self, measured: &LoudnessStats) -> bool {
        peak_within(self.ceiling_db, measured)
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

/// Loudness the limiter may remove when the target is "as loud as stays clean".
/// On hard-groove material a cost of ~0.3 LU measured clean and ~1.2 LU pumped
/// (docs/2026-10-11-mastering-fix-plan.md); this sits between the two, below the
/// heavy-limiting warning.
pub const CLEAN_LIMITING_LU: f64 = 0.5;

/// A loudest-clean search within this of its limiter budget has landed.
const BUDGET_TOLERANCE_LU: f64 = 0.1;

/// Renders allowed while searching, not counting a final re-render of the winner.
pub const MAX_SEARCH_ATTEMPTS: usize = 6;

/// The first probe sits this far above the gain at which the limiter idles.
const FIRST_STEP_DB: f64 = 4.0;

/// The search never goes further than this above that idle gain.
const MAX_STEP_DB: f64 = 20.0;

/// Bracket width at which the search stops refining (dB of gain).
const MIN_BRACKET_DB: f64 = 0.1;

/// One rendered attempt as measured: the file, and what the limiter took off its
/// loudness (0 when the limiter was not in the chain).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Pass {
    pub after: LoudnessStats,
    pub limiter_cost_lu: f64,
}

/// Aim for the loudest master whose limiter cost stays within a budget.
///
/// The cost only grows with gain, so the search brackets the gain at which it
/// meets the budget and narrows in by interpolation. Below the gain at which the
/// source's peaks first reach the limiter, the cost is zero, which gives the lower
/// end of the bracket without a render.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Loudest {
    /// True-peak ceiling (dBTP).
    pub ceiling_db: f64,
    /// Loudness the limiter may remove (LU).
    pub budget_lu: f64,
}

/// A probed gain and its limiter cost.
#[derive(Debug, Clone, Copy)]
struct Probe {
    gain_db: f64,
    cost_lu: f64,
}

impl Loudest {
    /// The highest gain at which the limiter is certainly idle.
    fn idle_gain_db(&self, before: &LoudnessStats, extra_peak_db: f64) -> f64 {
        default_limit_db(self.ceiling_db) - before.true_peak - extra_peak_db
    }

    /// The first probe; `extra_peak_db` as for [`Goal::first_plan`].
    pub fn first_plan(&self, before: &LoudnessStats, extra_peak_db: f64) -> RenderPlan {
        RenderPlan {
            gain_db: self.idle_gain_db(before, extra_peak_db) + FIRST_STEP_DB,
            limit_db: Some(default_limit_db(self.ceiling_db)),
        }
    }

    fn within_budget(&self, cost_lu: f64) -> bool {
        cost_lu <= self.budget_lu + BUDGET_TOLERANCE_LU
    }

    /// Where to probe next, from the highest probe within budget (`lo`) and the
    /// lowest one over it (`hi`), if there is one yet.
    fn next_gain(&self, idle_db: f64, lo: Probe, hi: Option<Probe>) -> f64 {
        match hi {
            // Interpolate between the two; keep clear of the ends so a flat or
            // steep stretch of the curve still shrinks the bracket.
            Some(hi) => {
                let span = hi.gain_db - lo.gain_db;
                let t = (self.budget_lu - lo.cost_lu) / (hi.cost_lu - lo.cost_lu);
                lo.gain_db + span * t.clamp(0.2, 0.8)
            }
            // Nothing over budget yet: the cost grows faster than linearly with
            // gain, so a straight line through the origin of the cost overshoots
            // the budget and gives an upper end to the bracket.
            None => {
                let step = if lo.cost_lu > 0.01 {
                    (lo.gain_db - idle_db) * (self.budget_lu / lo.cost_lu - 1.0)
                } else {
                    FIRST_STEP_DB
                };
                (lo.gain_db + step.clamp(MIN_BRACKET_DB, FIRST_STEP_DB)).min(idle_db + MAX_STEP_DB)
            }
        }
    }

    /// Render, measure and narrow in on the loudest gain within budget.
    ///
    /// `render` is as for [`land`], and returns the limiter cost with the
    /// measurement. The file of the returned landing is always the last one
    /// rendered: if the best attempt was an earlier one, it is rendered again.
    pub fn land<E>(
        &self,
        before: &LoudnessStats,
        extra_peak_db: f64,
        mut render: impl FnMut(usize, &RenderPlan) -> Result<Pass, E>,
    ) -> Result<Landing, E> {
        let idle = self.idle_gain_db(before, extra_peak_db);
        let cap_db = idle + MAX_STEP_DB;
        let mut limit_db = default_limit_db(self.ceiling_db);
        let mut lo = Probe {
            gain_db: idle,
            cost_lu: 0.0,
        };
        let mut hi: Option<Probe> = None;
        // The best attempt within budget and under the ceiling, and its number.
        let mut best: Option<(RenderPlan, Pass, usize)> = None;
        let mut gain_db = self.first_plan(before, extra_peak_db).gain_db;
        let mut attempts = 0;

        while attempts < MAX_SEARCH_ATTEMPTS {
            attempts += 1;
            let plan = RenderPlan {
                gain_db,
                limit_db: Some(limit_db),
            };
            let pass = render(attempts, &plan)?;
            if !peak_within(self.ceiling_db, &pass.after) {
                // The limiter let a peak through: tighten it and retry this gain.
                let overshoot = pass.after.true_peak - self.ceiling_db;
                limit_db = (limit_db - overshoot - PEAK_RETRY_MARGIN_DB).max(MIN_LIMIT_DB);
                continue;
            }
            let probe = Probe {
                gain_db,
                cost_lu: pass.limiter_cost_lu,
            };
            let better = best.map_or(true, |(b, _, _)| gain_db > b.gain_db);
            if self.within_budget(probe.cost_lu) && better {
                best = Some((plan, pass, attempts));
            }
            if (probe.cost_lu - self.budget_lu).abs() <= BUDGET_TOLERANCE_LU {
                break;
            }
            if probe.cost_lu < self.budget_lu {
                lo = probe;
            } else {
                hi = Some(probe);
            }
            if lo.gain_db >= cap_db || hi.is_some_and(|hi| hi.gain_db - lo.gain_db < MIN_BRACKET_DB)
            {
                break;
            }
            gain_db = self.next_gain(idle, lo, hi);
        }

        let plan = match best {
            Some((plan, pass, attempt)) if attempt == attempts => {
                return Ok(self.landed(plan, pass.after, attempts));
            }
            Some((plan, _, _)) => plan,
            // Every attempt went over budget: fall back to the highest gain known
            // to be within it.
            None => RenderPlan {
                gain_db: lo.gain_db,
                limit_db: Some(limit_db),
            },
        };
        attempts += 1;
        let after = render(attempts, &plan)?.after;
        Ok(self.landed(plan, after, attempts))
    }

    fn landed(&self, plan: RenderPlan, after: LoudnessStats, attempts: usize) -> Landing {
        Landing {
            plan,
            after,
            attempts,
            // There is no loudness target to miss; the measured result is the answer.
            loudness_ok: true,
            peak_ok: peak_within(self.ceiling_db, &after),
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

    fn never_fails<T>(value: T) -> Result<T, ()> {
        Ok(value)
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

    const LOUDEST: Loudest = Loudest {
        ceiling_db: -1.0,
        budget_lu: CLEAN_LIMITING_LU,
    };

    /// Limiting cost that grows with the square of the drive into the limiter, as
    /// measured limiter cost does: rare peaks first, then the body of the music.
    fn convex(plan: &RenderPlan, steepness: f64, overshoot_db: f64) -> Pass {
        let peak = SOURCE.true_peak + plan.gain_db;
        let limit = plan.limit_db.unwrap_or(f64::INFINITY);
        let drive = (peak - limit).max(0.0);
        let cost = steepness * drive * drive;
        Pass {
            after: stats(
                SOURCE.integrated + plan.gain_db - cost,
                if drive > 0.0 {
                    limit + overshoot_db
                } else {
                    peak
                },
            ),
            limiter_cost_lu: cost,
        }
    }

    /// The gain at which `convex` costs exactly the budget, with the default limiter.
    fn ideal_gain(steepness: f64) -> f64 {
        LOUDEST.idle_gain_db(&SOURCE, 0.0) + (LOUDEST.budget_lu / steepness).sqrt()
    }

    #[test]
    fn loudest_lands_on_the_limiter_budget() {
        for steepness in [0.02, 0.1, 0.5] {
            let mut plans = Vec::new();
            let landing = LOUDEST
                .land(&SOURCE, 0.0, |_, plan| {
                    plans.push(*plan);
                    never_fails(convex(plan, steepness, 0.0))
                })
                .unwrap();
            let cost = convex(&landing.plan, steepness, 0.0).limiter_cost_lu;
            assert!(
                LOUDEST.within_budget(cost) && cost > LOUDEST.budget_lu - 0.2,
                "steepness {steepness}: cost {cost}"
            );
            assert!((landing.plan.gain_db - ideal_gain(steepness)).abs() < 0.5);
            assert!(landing.attempts <= MAX_SEARCH_ATTEMPTS + 1);
            // What is reported is what was rendered last, i.e. the file on disk.
            assert_eq!(plans.last(), Some(&landing.plan));
            assert_eq!(plans.len(), landing.attempts);
            assert!(landing.loudness_ok && landing.peak_ok);
        }
    }

    #[test]
    fn loudest_never_keeps_a_render_over_budget() {
        // So steep that any limiting at all blows the budget.
        let landing = LOUDEST
            .land(&SOURCE, 0.0, |_, plan| never_fails(convex(plan, 50.0, 0.0)))
            .unwrap();
        let cost = convex(&landing.plan, 50.0, 0.0).limiter_cost_lu;
        assert!(LOUDEST.within_budget(cost), "cost {cost}");
        assert!(landing.plan.gain_db >= LOUDEST.idle_gain_db(&SOURCE, 0.0));
    }

    #[test]
    fn loudest_tightens_a_limiter_that_overshoots_the_ceiling() {
        let landing = LOUDEST
            .land(&SOURCE, 0.0, |_, plan| {
                // This limiter lets 0.4 dB through at its default threshold only.
                let leaky = plan.limit_db == Some(default_limit_db(LOUDEST.ceiling_db));
                never_fails(convex(plan, 0.1, if leaky { 0.4 } else { 0.0 }))
            })
            .unwrap();
        assert!(landing.peak_ok);
        assert!(landing.plan.limit_db.unwrap() < default_limit_db(LOUDEST.ceiling_db));
    }

    #[test]
    fn loudest_reserves_headroom_for_peaks_raised_later() {
        let plain = LOUDEST.first_plan(&SOURCE, 0.0);
        let filtered = LOUDEST.first_plan(&SOURCE, 3.0);
        assert!((plain.gain_db - filtered.gain_db - 3.0).abs() < 1e-9);
    }

    #[test]
    fn loudest_stops_at_the_first_render_error() {
        let mut calls = 0;
        let result = LOUDEST.land(&SOURCE, 0.0, |_, _| {
            calls += 1;
            Err::<Pass, _>("render failed")
        });
        assert_eq!(result, Err("render failed"));
        assert_eq!(calls, 1);
    }
}
