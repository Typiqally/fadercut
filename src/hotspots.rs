//! Where in a master the limiter worked hardest, so a listener knows where to check.
//!
//! Compares the short-term loudness of the signal entering the limiter with that of
//! the rendered file, frame by frame: the difference is what the limiter removed at
//! that moment. Pure — works on two loudness timelines.

use crate::analysis::Timeline;

/// Length of each reported stretch.
const WINDOW_S: f64 = 30.0;
/// How many of the hardest-hit stretches to report.
const MAX_STRETCHES: usize = 5;
/// Reported stretches start at least this far apart, so they cover the set rather
/// than five views of the same drop.
const MIN_SEPARATION_S: f64 = 180.0;
/// Frames this far below the programme loudness are breakdowns or silence, where
/// there is nothing to hear the limiter on.
const GATE_LU: f64 = 20.0;
/// Short-term loudness looks back 3 s, so a reading describes audio centred this
/// much earlier than its timestamp.
const SHORT_TERM_LAG_S: f64 = 1.5;

/// A stretch of the output and the loudness the limiter took off it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Stretch {
    pub start_s: f64,
    pub end_s: f64,
    pub cost_lu: f64,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Hotspots {
    /// The hardest-limited stretches, in playing order.
    pub heaviest: Vec<Stretch>,
    /// The least-limited stretch, as a reference to compare against.
    pub lightest: Option<Stretch>,
}

struct Window {
    start: usize,
    cost_lu: f64,
}

/// Find the stretches where the limiter removed the most, and the least, loudness.
///
/// `entering` is the timeline of the signal going into the limiter and `rendered`
/// that of the finished file; both must cover the same audio at the same frame rate.
pub fn find(entering: &Timeline, rendered: &Timeline) -> Hotspots {
    let frames = entering.times.len().min(rendered.times.len());
    if frames == 0 {
        return Hotspots::default();
    }
    let dt = match entering.times.get(..2) {
        Some([a, b]) if b > a => b - a,
        _ => 0.1,
    };
    let in_frames = |seconds: f64| ((seconds / dt).round() as usize).max(1);
    let width = in_frames(WINDOW_S).min(frames);

    // Prefix sums of the per-frame cost over frames loud enough to count.
    let gate = entering.integrated - GATE_LU;
    let mut sum = vec![0.0; frames + 1];
    let mut count = vec![0usize; frames + 1];
    for i in 0..frames {
        let (before, after) = (entering.short_term[i], rendered.short_term[i]);
        let counted = before > gate;
        sum[i + 1] = sum[i] + if counted { before - after } else { 0.0 };
        count[i + 1] = count[i] + usize::from(counted);
    }

    // Candidate windows, one per second, that are mostly programme material.
    let mut windows: Vec<Window> = (0..=frames - width)
        .step_by(in_frames(1.0))
        .filter_map(|start| {
            let n = count[start + width] - count[start];
            (n * 2 >= width).then(|| Window {
                start,
                cost_lu: (sum[start + width] - sum[start]) / n as f64,
            })
        })
        .collect();
    windows.sort_by(|a, b| b.cost_lu.total_cmp(&a.cost_lu));

    let separation = in_frames(MIN_SEPARATION_S).max(width);
    let mut picked: Vec<&Window> = Vec::new();
    for window in windows.iter().filter(|w| w.cost_lu > 0.0) {
        if picked.len() == MAX_STRETCHES {
            break;
        }
        if picked
            .iter()
            .all(|p| p.start.abs_diff(window.start) >= separation)
        {
            picked.push(window);
        }
    }
    // The reference must not overlap anything it is meant to be compared with.
    let lightest = windows
        .iter()
        .rev()
        .find(|w| picked.iter().all(|p| p.start.abs_diff(w.start) >= width));

    let stretch = |w: &Window| {
        let start_s = (entering.times[w.start] - SHORT_TERM_LAG_S).max(0.0);
        Stretch {
            start_s,
            end_s: start_s + width as f64 * dt,
            cost_lu: w.cost_lu,
        }
    };
    let lightest = lightest.map(stretch);
    picked.sort_by_key(|w| w.start);
    Hotspots {
        heaviest: picked.into_iter().map(stretch).collect(),
        lightest,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LEVEL: f64 = -10.0;

    /// A timeline at 10 Hz whose short-term loudness is `LEVEL - cost(seconds)`.
    fn timeline(seconds: usize, cost: impl Fn(f64) -> f64) -> Timeline {
        let times: Vec<f64> = (0..seconds * 10).map(|i| i as f64 * 0.1).collect();
        let short_term: Vec<f64> = times.iter().map(|&t| LEVEL - cost(t)).collect();
        Timeline {
            momentary: short_term.clone(),
            short_term,
            times,
            integrated: LEVEL,
            true_peak: -1.0,
            lra: 0.0,
        }
    }

    fn untouched(seconds: usize) -> Timeline {
        timeline(seconds, |_| 0.0)
    }

    /// Limiter cost that is `depth` LU inside `[from, to)` seconds and 0.2 elsewhere.
    fn bump(t: f64, spans: &[(f64, f64, f64)]) -> f64 {
        spans
            .iter()
            .find(|(from, to, _)| (*from..*to).contains(&t))
            .map_or(0.2, |&(_, _, depth)| depth)
    }

    #[test]
    fn finds_the_hardest_limited_stretches_in_playing_order() {
        let spans = [
            (900.0, 960.0, 3.0),
            (300.0, 360.0, 2.0),
            (1500.0, 1560.0, 1.0),
        ];
        let rendered = timeline(1800, |t| bump(t, &spans));
        let found = find(&untouched(1800), &rendered);

        let starts: Vec<f64> = found.heaviest.iter().map(|s| s.start_s).collect();
        assert!(starts.windows(2).all(|w| w[0] < w[1]), "{starts:?}");
        // The three bumps are among the picks, each reported at its full depth.
        for (from, to, depth) in spans {
            let hit = found
                .heaviest
                .iter()
                .find(|s| s.start_s >= from - 2.0 && s.end_s <= to + 2.0)
                .unwrap_or_else(|| panic!("no stretch inside {from}..{to}: {starts:?}"));
            assert!((hit.cost_lu - depth).abs() < 0.05, "{hit:?}");
            assert!((hit.end_s - hit.start_s - 30.0).abs() < 1e-6);
        }
    }

    #[test]
    fn stretches_are_spread_out_and_capped() {
        // Heavy limiting everywhere: without spacing, all picks would be neighbours.
        let found = find(&untouched(3600), &timeline(3600, |t| 2.0 + t / 3600.0));
        assert_eq!(found.heaviest.len(), MAX_STRETCHES);
        for pair in found.heaviest.windows(2) {
            assert!(pair[1].start_s - pair[0].start_s >= MIN_SEPARATION_S - 1.0);
        }
    }

    #[test]
    fn the_reference_is_the_least_limited_stretch_away_from_the_picks() {
        let spans = [(100.0, 400.0, 3.0), (500.0, 560.0, 0.0)];
        let found = find(&untouched(900), &timeline(900, |t| bump(t, &spans)));
        let lightest = found.lightest.unwrap();
        assert!(
            lightest.start_s >= 498.0 && lightest.end_s <= 562.0,
            "{lightest:?}"
        );
        assert!(lightest.cost_lu.abs() < 0.05);
    }

    #[test]
    fn quiet_passages_are_not_reported() {
        // A breakdown 30 LU down "loses" a lot on paper, but there is nothing to hear.
        let mut entering = untouched(900);
        let mut rendered = timeline(900, |t| bump(t, &[(600.0, 660.0, 1.5)]));
        for i in 2000..2600 {
            entering.short_term[i] = LEVEL - 30.0;
            rendered.short_term[i] = LEVEL - 40.0;
        }
        let found = find(&entering, &rendered);
        let worst = found.heaviest.iter().map(|s| s.cost_lu).fold(0.0, f64::max);
        assert!(
            (worst - 1.5).abs() < 0.05,
            "worst reported cost was {worst} LU"
        );
    }

    #[test]
    fn audio_shorter_than_a_window_is_one_stretch() {
        let found = find(&untouched(12), &timeline(12, |_| 1.0));
        assert_eq!(found.heaviest.len(), 1);
        let only = found.heaviest[0];
        assert_eq!(only.start_s, 0.0);
        assert!((only.end_s - 12.0).abs() < 1e-6);
        assert!((only.cost_lu - 1.0).abs() < 1e-9);
        assert_eq!(found.lightest, None);
    }

    #[test]
    fn an_idle_limiter_has_no_hotspots() {
        let found = find(&untouched(600), &untouched(600));
        assert!(found.heaviest.is_empty());
        assert_eq!(find(&untouched(0), &untouched(0)), Hotspots::default());
    }

    #[test]
    fn timelines_of_slightly_different_length_are_compared_over_their_overlap() {
        let found = find(&untouched(600), &timeline(601, |_| 1.0));
        assert!(!found.heaviest.is_empty());
        assert!(found.heaviest.iter().all(|s| s.end_s <= 600.1));
    }
}
