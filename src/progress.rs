//! Live progress line on stderr for long audio passes.

use std::time::Instant;

fn fmt_mmss(seconds: f64) -> String {
    let s = seconds.max(0.0) as u64;
    format!("{}:{:02}", s / 60, s % 60)
}

/// Percentage / speed / ETA of a pass over `total_frames` of audio.
pub struct Progress {
    label: &'static str,
    total_frames: u64,
    sample_rate: u32,
    start: Instant,
    next_report: u64,
}

impl Progress {
    pub fn new(label: &'static str, total_frames: u64, sample_rate: u32) -> Self {
        Self {
            label,
            total_frames,
            sample_rate,
            start: Instant::now(),
            next_report: 0,
        }
    }

    fn secs(&self, frames: u64) -> f64 {
        frames as f64 / f64::from(self.sample_rate)
    }

    /// Report that `pos` frames are done (throttled to about once per percent).
    pub fn update(&mut self, pos: u64) {
        if self.total_frames == 0 || pos < self.next_report {
            return;
        }
        let elapsed = self.start.elapsed().as_secs_f64();
        let (done, total) = (self.secs(pos), self.secs(self.total_frames));
        let speed = if elapsed > 0.0 { done / elapsed } else { 0.0 };
        let eta = if speed > 0.0 {
            (total - done) / speed
        } else {
            0.0
        };
        eprint!(
            "\r  {}: {:5.1}%  [{}/{} audio]  {:4.1}x  ETA {}   ",
            self.label,
            100.0 * pos as f64 / self.total_frames as f64,
            fmt_mmss(done),
            fmt_mmss(total),
            speed,
            fmt_mmss(eta)
        );
        self.next_report = pos + self.total_frames / 100;
    }

    pub fn finish(&self) {
        if self.total_frames == 0 {
            return;
        }
        let total = fmt_mmss(self.secs(self.total_frames));
        eprintln!(
            "\r  {}: 100.0%  [{total}/{total} audio] done in {}            ",
            self.label,
            fmt_mmss(self.start.elapsed().as_secs_f64())
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fmt_mmss_formats_minutes_and_seconds() {
        assert_eq!(fmt_mmss(0.0), "0:00");
        assert_eq!(fmt_mmss(125.9), "2:05");
        assert_eq!(fmt_mmss(-3.0), "0:00");
    }
}
