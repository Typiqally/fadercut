//! A clip's volume envelope: the gain curve drawn over it in Audacity.
//!
//! Evaluated the way Audacity's `Envelope::GetValues` does for a wave clip: the curve
//! holds the first point's gain before it and the last point's gain after it, and
//! moves between two points in a straight line in decibels (so exponentially in
//! gain). Two points at the same time are a step, taken from the first frame at or
//! after that time. Pure — no I/O.

#[derive(Debug, PartialEq, thiserror::Error)]
pub enum EnvelopeError {
    #[error("a volume envelope has no control points")]
    Empty,
    #[error("a volume envelope point has no usable time or gain (time {t}, gain {gain})")]
    BadPoint { t: f64, gain: f64 },
    #[error("a volume envelope's points are not in time order")]
    Unordered,
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct Point {
    t: f64,
    gain: f64,
    log_gain: f64,
}

/// A gain curve over the frames of one clip.
#[derive(Debug, Clone, PartialEq)]
pub struct Envelope {
    points: Vec<Point>,
    /// Envelope time of the clip's first frame, in seconds.
    origin_s: f64,
    sample_rate: f64,
}

/// Longest run evaluated from one starting gain. A shorter run is never wrong: the
/// next one starts from a freshly computed gain.
const MAX_RUN_FRAMES: u64 = 1 << 20;

/// The curve from one frame onwards, for as long as it keeps the same shape.
struct Span {
    gain: f64,
    /// Factor from one frame's gain to the next.
    ratio: f64,
    frames: u64,
}

impl Envelope {
    /// `points` are (time in seconds, linear gain) in time order; `origin_s` is the
    /// time, on the same clock, of the clip's first frame.
    pub fn new(
        points: &[(f64, f64)],
        origin_s: f64,
        sample_rate: f64,
    ) -> Result<Self, EnvelopeError> {
        if points.is_empty() {
            return Err(EnvelopeError::Empty);
        }
        if let Some(&(t, gain)) = points
            .iter()
            .find(|(t, gain)| !(t.is_finite() && gain.is_finite() && *gain > 0.0))
        {
            return Err(EnvelopeError::BadPoint { t, gain });
        }
        if points.windows(2).any(|pair| pair[1].0 < pair[0].0) {
            return Err(EnvelopeError::Unordered);
        }
        Ok(Self {
            points: points
                .iter()
                .map(|&(t, gain)| Point {
                    t,
                    gain,
                    log_gain: gain.log10(),
                })
                .collect(),
            origin_s,
            sample_rate,
        })
    }

    /// True when the curve never leaves unity, so applying it changes nothing.
    pub fn is_unity(points: &[(f64, f64)]) -> bool {
        points.iter().all(|(_, gain)| (gain - 1.0).abs() < 1e-9)
    }

    fn time_of(&self, frame: u64) -> f64 {
        self.origin_s + frame as f64 / self.sample_rate
    }

    /// Frames from `frame` up to, not including, the first one at or after `until`.
    fn frames_until(&self, frame: u64, until: f64) -> u64 {
        let estimate = ((until - self.time_of(frame)) * self.sample_rate).ceil();
        if estimate >= MAX_RUN_FRAMES as f64 {
            return MAX_RUN_FRAMES;
        }
        // Settle the rounding against the clock every frame is placed on, so that a
        // frame's gain does not depend on where the run containing it began.
        let frames = (estimate as u64).max(1);
        if frames > 1 && self.time_of(frame + frames - 1) >= until {
            frames - 1
        } else if self.time_of(frame + frames) < until {
            frames + 1
        } else {
            frames
        }
    }

    fn span_at(&self, frame: u64) -> Span {
        let t = self.time_of(frame);
        let after = self.points.partition_point(|p| p.t <= t);
        let held = |point: &Point, frames| Span {
            gain: point.gain,
            ratio: 1.0,
            frames,
        };
        let Some(next) = self.points.get(after) else {
            return held(&self.points[after - 1], MAX_RUN_FRAMES);
        };
        let frames = self.frames_until(frame, next.t);
        let Some(prev) = after.checked_sub(1).map(|i| &self.points[i]) else {
            return held(next, frames);
        };
        // log10 of gain per second; `next.t > prev.t` because `prev.t <= t < next.t`.
        let slope = (next.log_gain - prev.log_gain) / (next.t - prev.t);
        Span {
            gain: 10f64.powf(prev.log_gain + slope * (t - prev.t)),
            ratio: 10f64.powf(slope / self.sample_rate),
            frames,
        }
    }

    /// The gain at one frame of the clip.
    pub fn gain_at(&self, frame: u64) -> f64 {
        self.span_at(frame).gain
    }

    /// Write `samples`, which start at clip frame `first_frame`, into `out` with the
    /// curve applied. Replaces the contents of `out`.
    pub fn apply(&self, first_frame: u64, samples: &[f32], out: &mut Vec<f32>) {
        out.clear();
        out.reserve(samples.len());
        let mut frame = first_frame;
        let mut rest = samples;
        while !rest.is_empty() {
            let span = self.span_at(frame);
            let run = usize::try_from(span.frames).map_or(rest.len(), |n| n.min(rest.len()));
            let (now, later) = rest.split_at(run);
            // Stepping by a constant factor is exact for a line in decibels; the
            // rounding it accumulates over one run is far below a 32-bit float's.
            let mut gain = span.gain;
            for &sample in now {
                out.push((f64::from(sample) * gain) as f32);
                gain *= span.ratio;
            }
            frame += now.len() as u64;
            rest = later;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: f64 = 1000.0;

    fn envelope(points: &[(f64, f64)]) -> Envelope {
        Envelope::new(points, 0.0, RATE).unwrap()
    }

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() <= 1e-9 * b.abs().max(1.0)
    }

    #[test]
    fn holds_the_end_points_outside_the_curve() {
        let env = envelope(&[(1.0, 0.5), (2.0, 2.0)]);
        assert_eq!(env.gain_at(0), 0.5);
        assert_eq!(env.gain_at(999), 0.5);
        assert_eq!(env.gain_at(2000), 2.0);
        assert_eq!(env.gain_at(50_000), 2.0);
    }

    #[test]
    fn moves_between_points_in_a_straight_line_in_decibels() {
        // 0.1 -> 1.0 is +20 dB over one second: halfway is +10 dB, not 0.55.
        let env = envelope(&[(1.0, 0.1), (2.0, 1.0)]);
        assert!(close(env.gain_at(1000), 0.1));
        assert!(close(env.gain_at(1500), 10f64.powf(-0.5)));
        assert!(close(env.gain_at(1250), 10f64.powf(-0.75)));
    }

    #[test]
    fn a_single_point_is_a_constant_gain() {
        let env = envelope(&[(5.0, 0.25)]);
        assert_eq!((env.gain_at(0), env.gain_at(9000)), (0.25, 0.25));
    }

    #[test]
    fn two_points_at_one_time_are_a_step() {
        let env = envelope(&[(0.0, 1.0), (1.0, 1.0), (1.0, 0.5), (2.0, 0.5)]);
        assert_eq!(env.gain_at(999), 1.0);
        assert_eq!(env.gain_at(1000), 0.5);
    }

    #[test]
    fn the_origin_places_the_clip_on_the_envelopes_clock() {
        // The clip's first frame is 1.5 s into the envelope: halfway up the ramp.
        let env = Envelope::new(&[(1.0, 0.1), (2.0, 1.0)], 1.5, RATE).unwrap();
        assert!(close(env.gain_at(0), 10f64.powf(-0.5)));
        assert_eq!(env.gain_at(500), 1.0);
    }

    #[test]
    fn applying_matches_the_gain_at_every_frame() {
        let env = envelope(&[
            (0.1, 0.76),
            (0.4, 1.7),
            (0.55, 0.58),
            (0.9, 0.07),
            (5.0, 1.0),
        ]);
        let samples = vec![0.5f32; 1200];
        let mut out = Vec::new();
        env.apply(0, &samples, &mut out);
        assert_eq!(out.len(), samples.len());
        for (frame, &shaped) in out.iter().enumerate() {
            let expected = 0.5 * env.gain_at(frame as u64);
            assert!(
                (f64::from(shaped) - expected).abs() < 1e-7,
                "frame {frame}: {shaped} vs {expected}"
            );
        }
    }

    #[test]
    fn applying_in_pieces_gives_the_same_curve() {
        let env = envelope(&[(0.1, 0.5), (0.3, 2.0), (0.3, 0.25), (0.8, 1.0)]);
        let samples: Vec<f32> = (0..1000).map(|i| (i as f32 * 0.37).sin()).collect();
        let mut whole = Vec::new();
        env.apply(0, &samples, &mut whole);

        let (mut pieces, mut out) = (Vec::new(), vec![9.0]);
        for (i, chunk) in samples.chunks(77).enumerate() {
            env.apply(i as u64 * 77, chunk, &mut out);
            pieces.extend_from_slice(&out);
        }
        assert_eq!(pieces.len(), whole.len());
        for (a, b) in pieces.iter().zip(&whole) {
            assert!((a - b).abs() < 1e-6, "{a} vs {b}");
        }
    }

    #[test]
    fn unusable_points_are_rejected() {
        assert_eq!(Envelope::new(&[], 0.0, RATE), Err(EnvelopeError::Empty));
        for gain in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            assert!(matches!(
                Envelope::new(&[(0.0, gain)], 0.0, RATE),
                Err(EnvelopeError::BadPoint { .. })
            ));
        }
        assert!(matches!(
            Envelope::new(&[(f64::NAN, 1.0)], 0.0, RATE),
            Err(EnvelopeError::BadPoint { .. })
        ));
        assert_eq!(
            Envelope::new(&[(2.0, 1.0), (1.0, 0.5)], 0.0, RATE),
            Err(EnvelopeError::Unordered)
        );
    }

    #[test]
    fn unity_is_recognised() {
        assert!(Envelope::is_unity(&[]));
        assert!(Envelope::is_unity(&[(0.0, 1.0), (9.0, 1.0)]));
        assert!(!Envelope::is_unity(&[(0.0, 1.0), (9.0, 0.5)]));
    }
}
