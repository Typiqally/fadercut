//! End-to-end tests: run the real binary against synthetic audio through ffmpeg.
//!
//! These need `ffmpeg`/`ffprobe`; each test is skipped (with a note) when they are
//! not installed, so the unit suite still runs on machines without them.

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use fadercut::ffutil::find_tool;
use tempfile::TempDir;

#[path = "support/aup_fixture.rs"]
mod aup_fixture;

const SAMPLE_RATE: u32 = 48000;
const DURATION_S: f64 = 12.0;

struct Fixture {
    ffmpeg: PathBuf,
    ffprobe: PathBuf,
    dir: TempDir,
    input: PathBuf,
}

impl Fixture {
    /// 12 s of uncorrelated stereo pink noise whose second half is 6 dB louder.
    fn new() -> Option<Self> {
        let (Some(ffmpeg), Some(ffprobe)) = (find_tool("ffmpeg"), find_tool("ffprobe")) else {
            eprintln!("skipping: ffmpeg/ffprobe not installed");
            return None;
        };
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("set.wav");
        let noise = |seed: u32| {
            format!(
                "anoisesrc=color=pink:seed={seed}:duration={DURATION_S}\
                 :sample_rate={SAMPLE_RATE}:amplitude=0.1"
            )
        };
        let graph = "[0:a][1:a]join=inputs=2:channel_layout=stereo,\
                     volume='if(lt(t,6),0.5,1)':eval=frame[a]";
        let status = Command::new(&ffmpeg)
            .args(["-hide_banner", "-loglevel", "error", "-y"])
            .args(["-f", "lavfi", "-i", &noise(1)])
            .args(["-f", "lavfi", "-i", &noise(2)])
            .args(["-filter_complex", graph, "-map", "[a]", "-c:a", "pcm_s24le"])
            .arg(&input)
            .stdin(Stdio::null())
            .status()
            .unwrap();
        assert!(status.success(), "could not generate the test input");
        Some(Self {
            ffmpeg,
            ffprobe,
            dir,
            input,
        })
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

    fn fadercut(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_fadercut"))
            .arg(&self.input)
            .args(args)
            .stdin(Stdio::null())
            .output()
            .unwrap()
    }

    fn probe(&self, file: &Path, entries: &str) -> Vec<String> {
        let out = Command::new(&self.ffprobe)
            .args(["-v", "error", "-select_streams", "a:0"])
            .args(["-show_entries", entries])
            .args(["-of", "default=noprint_wrappers=1:nokey=1"])
            .arg(file)
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout)
            .split_whitespace()
            .map(str::to_owned)
            .collect()
    }

    fn duration(&self, file: &Path) -> f64 {
        self.probe(file, "format=duration")[0].parse().unwrap()
    }

    /// Mean level (dB) below ~50 Hz of the mid (`+`) or side (`-`) signal.
    fn low_band_level(&self, file: &Path, sign: char) -> f64 {
        let filter =
            format!("pan=mono|c0=0.5*c0{sign}0.5*c1,lowpass=f=50,lowpass=f=50,volumedetect");
        let out = Command::new(&self.ffmpeg)
            .args(["-hide_banner", "-nostats", "-i"])
            .arg(file)
            .args(["-af", &filter, "-f", "null", "-"])
            .stdin(Stdio::null())
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&out.stderr);
        let line = stderr
            .lines()
            .find(|l| l.contains("mean_volume:"))
            .expect("volumedetect output");
        let value = line.split("mean_volume:").nth(1).unwrap();
        value.trim().trim_end_matches("dB").trim().parse().unwrap()
    }
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// Integrated loudness from a `before`/`after` line of the printed report.
fn reported_loudness(report: &str, label: &str) -> f64 {
    let line = report
        .lines()
        .find(|l| l.trim_start().starts_with(label))
        .unwrap_or_else(|| panic!("no {label:?} line in report:\n{report}"));
    let value = line.split("I=").nth(1).unwrap();
    value.split_whitespace().next().unwrap().parse().unwrap()
}

#[test]
fn transparent_render_hits_target_and_keeps_format() {
    let Some(fx) = Fixture::new() else { return };
    let output = fx.path("out.wav");
    // A target low enough that the limiter stays idle, so the gain alone decides.
    let run = fx.fadercut(&[
        "--preset",
        "streaming",
        "--target",
        "-24",
        "--output",
        output.to_str().unwrap(),
    ]);
    assert!(run.status.success(), "{}", stderr(&run));

    let report = stdout(&run);
    assert!(
        (reported_loudness(&report, "after") + 24.0).abs() <= 0.2,
        "{report}"
    );
    assert!(report.contains(&format!("Done: {}", output.display())));
    assert_eq!(
        fx.probe(&output, "stream=codec_name,sample_rate,channels"),
        ["pcm_s24le", "48000", "2"]
    );
    assert!((fx.duration(&output) - DURATION_S).abs() < 0.05);
}

#[test]
fn default_output_is_written_next_to_the_input() {
    let Some(fx) = Fixture::new() else { return };
    let run = fx.fadercut(&["--preset", "even-only"]);
    assert!(run.status.success(), "{}", stderr(&run));
    assert!(fx.path("set-mastered.wav").is_file());
    // even-only keeps the measured loudness.
    let report = stdout(&run);
    let drift = reported_loudness(&report, "after") - reported_loudness(&report, "before");
    assert!(drift.abs() <= 0.2, "{report}");
}

#[test]
fn leveling_narrows_the_gap_between_sections() {
    let Some(fx) = Fixture::new() else { return };
    let (flat, levelled) = (fx.path("flat.wav"), fx.path("levelled.wav"));
    let common = ["--preset", "streaming", "--target", "-24", "--output"];
    let run = fx.fadercut(&[&common[..], &[flat.to_str().unwrap()]].concat());
    assert!(run.status.success(), "{}", stderr(&run));
    let run = fx.fadercut(
        &[
            &common[..],
            &[levelled.to_str().unwrap(), "--leveling", "strong"],
        ]
        .concat(),
    );
    assert!(run.status.success(), "{}", stderr(&run));
    assert!((fx.duration(&levelled) - DURATION_S).abs() < 0.05);

    // Loudness of the last 3 s minus the first 3 s: 6 dB in the source.
    let step = |file: &Path| {
        let section = |start: &str| {
            let out = Command::new(&fx.ffmpeg)
                .args(["-hide_banner", "-nostats", "-ss", start, "-t", "3", "-i"])
                .arg(file)
                .args(["-af", "volumedetect", "-f", "null", "-"])
                .stdin(Stdio::null())
                .output()
                .unwrap();
            let text = stderr(&out);
            let line = text.lines().find(|l| l.contains("mean_volume:")).unwrap();
            let value = line.split("mean_volume:").nth(1).unwrap();
            value
                .trim()
                .trim_end_matches("dB")
                .trim()
                .parse::<f64>()
                .unwrap()
        };
        section("9") - section("0")
    };
    let (before, after) = (step(&flat), step(&levelled));
    assert!((before - 6.0).abs() < 0.5, "source step was {before} dB");
    // Only ~5 s of ride time at 0.4 dB/s, so expect a partial correction.
    assert!(after < before - 0.5, "step {before} dB -> {after} dB");
}

#[test]
fn bass_mono_removes_low_frequency_side_energy() {
    let Some(fx) = Fixture::new() else { return };
    let output = fx.path("mono-bass.wav");
    let run = fx.fadercut(&[
        "--preset",
        "streaming",
        "--target",
        "-24",
        "--bass-mono",
        "--subsonic",
        "--output",
        output.to_str().unwrap(),
    ]);
    assert!(run.status.success(), "{}", stderr(&run));
    assert!(stderr(&run).contains("Low-end: subsonic HPF 28Hz, bass-mono <120Hz"));

    let side_vs_mid = |file: &Path| fx.low_band_level(file, '-') - fx.low_band_level(file, '+');
    let (before, after) = (side_vs_mid(&fx.input), side_vs_mid(&output));
    assert!(after < before - 10.0, "side/mid {before} dB -> {after} dB");
}

#[test]
fn preview_renders_only_the_requested_range() {
    let Some(fx) = Fixture::new() else { return };
    let run = fx.fadercut(&["--preview", "0:02-0:05", "--leveling", "gentle"]);
    assert!(run.status.success(), "{}", stderr(&run));
    assert!(stderr(&run).contains("Preview mode: 2s for 3s"));
    let output = fx.path("set-preview.wav");
    assert!((fx.duration(&output) - 3.0).abs() < 0.05);
}

#[test]
fn dry_run_renders_nothing() {
    let Some(fx) = Fixture::new() else { return };
    for leveling in ["off", "gentle"] {
        let run = fx.fadercut(&["--dry-run", "--leveling", leveling]);
        assert!(run.status.success(), "{}", stderr(&run));
        assert!(stderr(&run).contains("[dry-run]"));
        assert!(stdout(&run).is_empty());
    }
    assert!(!fx.path("set-mastered.wav").exists());
}

#[test]
fn refuses_to_clobber_existing_output_without_force() {
    let Some(fx) = Fixture::new() else { return };
    let output = fx.path("set-mastered.wav");
    std::fs::write(&output, b"keep me").unwrap();

    let run = fx.fadercut(&[]);
    assert_eq!(run.status.code(), Some(2));
    assert!(stderr(&run).contains("output exists"));
    assert_eq!(std::fs::read(&output).unwrap(), b"keep me");

    let run = fx.fadercut(&["--force", "--preset", "even-only"]);
    assert!(run.status.success(), "{}", stderr(&run));
    assert_ne!(std::fs::read(&output).unwrap(), b"keep me");
}

#[test]
fn refuses_to_overwrite_the_input() {
    let Some(fx) = Fixture::new() else { return };
    let original = std::fs::read(&fx.input).unwrap();
    let alias = fx.dir.path().join(".").join("set.wav");
    let run = fx.fadercut(&["--force", "--output", alias.to_str().unwrap()]);
    assert_eq!(run.status.code(), Some(2));
    assert!(stderr(&run).contains("would overwrite the input"));
    assert_eq!(std::fs::read(&fx.input).unwrap(), original);
}

#[test]
fn missing_input_is_a_usage_error() {
    if find_tool("ffmpeg").is_none() || find_tool("ffprobe").is_none() {
        eprintln!("skipping: ffmpeg/ffprobe not installed");
        return;
    }
    let run = Command::new(env!("CARGO_BIN_EXE_fadercut"))
        .arg("/nonexistent/set.wav")
        .output()
        .unwrap();
    assert_eq!(run.status.code(), Some(2));
    assert!(stderr(&run).contains("error: input not found: /nonexistent/set.wav"));
}

#[test]
fn undecodable_input_is_rejected_before_any_work() {
    let Some(fx) = Fixture::new() else { return };
    let bogus = fx.path("notes.wav");
    std::fs::write(&bogus, b"this is not audio").unwrap();
    for leveling in ["off", "gentle"] {
        let run = Command::new(env!("CARGO_BIN_EXE_fadercut"))
            .arg(&bogus)
            .args(["--leveling", leveling])
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert_eq!(run.status.code(), Some(2), "{}", stderr(&run));
        assert!(stderr(&run).contains("error: no readable audio stream"));
        assert!(stdout(&run).is_empty());
        assert!(!fx.path("notes-mastered.wav").exists());
    }
}

#[test]
fn invalid_arguments_are_rejected_before_any_work() {
    let run = Command::new(env!("CARGO_BIN_EXE_fadercut"))
        .args(["set.wav", "--preview", "1:30-0:30"])
        .output()
        .unwrap();
    assert_eq!(run.status.code(), Some(2));
    assert!(stderr(&run).contains("must be after start"));
}

// ---------------------------------------------------------------------------
// Audio-level checks: what the rendered file measures, independently of the
// tool's own report. Loudness and true peak come from ffmpeg's ebur128 summary
// (the tool reads the per-frame metadata instead); distortion is computed here
// from the decoded samples.
// ---------------------------------------------------------------------------

impl Fixture {
    /// Replace the default input with audio generated from a lavfi source.
    fn with_source(mut self, name: &str, lavfi: &str) -> Self {
        let input = self.path(name);
        let status = Command::new(&self.ffmpeg)
            .args(["-hide_banner", "-loglevel", "error", "-y"])
            .args(["-f", "lavfi", "-i", lavfi, "-c:a", "pcm_s24le"])
            .arg(&input)
            .stdin(Stdio::null())
            .status()
            .unwrap();
        assert!(status.success(), "could not generate {name}");
        self.input = input;
        self
    }

    /// (integrated LUFS, true peak dBTP) from ffmpeg's ebur128 summary.
    fn r128(&self, file: &Path) -> (f64, f64) {
        let out = Command::new(&self.ffmpeg)
            .args(["-hide_banner", "-nostats", "-i"])
            .arg(file)
            .args(["-af", "ebur128=peak=true", "-f", "null", "-"])
            .stdin(Stdio::null())
            .output()
            .unwrap();
        let text = stderr(&out);
        let summary = &text[text.rfind("Summary:").expect("ebur128 summary")..];
        let field = |label: &str| -> f64 {
            let line = summary
                .lines()
                .find(|l| l.trim_start().starts_with(label))
                .unwrap_or_else(|| panic!("no {label:?} in:\n{summary}"));
            let value = line.split(':').nth(1).unwrap();
            value.split_whitespace().next().unwrap().parse().unwrap()
        };
        (field("I:"), field("Peak:"))
    }

    /// Decoded samples, interleaved, as f64.
    fn samples(&self, file: &Path, channels: u8) -> Vec<f64> {
        let out = Command::new(&self.ffmpeg)
            .args(["-v", "error", "-i"])
            .arg(file)
            .args(["-f", "f32le", "-ac", &channels.to_string(), "-"])
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert!(out.status.success(), "{}", stderr(&out));
        out.stdout
            .chunks_exact(4)
            .map(|b| f64::from(f32::from_le_bytes([b[0], b[1], b[2], b[3]])))
            .collect()
    }

    /// Null test: energy of (output - best-fit gain * source) relative to the output,
    /// in dB. No time alignment is searched, so any added delay shows up as residual.
    fn null_residual_db(&self, output: &Path) -> f64 {
        let (src, out) = (self.samples(&self.input, 2), self.samples(output, 2));
        let n = src.len().min(out.len());
        assert!(
            n > 0 && src.len().abs_diff(out.len()) < 64,
            "length changed"
        );
        let (src, out) = (&src[..n], &out[..n]);
        let dot: f64 = src.iter().zip(out).map(|(s, o)| s * o).sum();
        let gain = dot / src.iter().map(|s| s * s).sum::<f64>();
        let residual: f64 = src
            .iter()
            .zip(out)
            .map(|(s, o)| (o - gain * s).powi(2))
            .sum();
        10.0 * (residual / out.iter().map(|o| o * o).sum::<f64>()).log10()
    }

    /// THD+N of a steady tone at `freq` (which must divide the sample rate): everything
    /// that is not the fundamental, relative to the fundamental, in dB.
    fn thd_n_db(&self, file: &Path, freq: f64) -> f64 {
        let rate = f64::from(SAMPLE_RATE);
        let all = self.samples(file, 1);
        // The steady middle, a whole number of cycles long.
        let window = &all[SAMPLE_RATE as usize..4 * SAMPLE_RATE as usize];
        let phase = |i: usize| 2.0 * std::f64::consts::PI * freq * i as f64 / rate;
        let n = window.len() as f64;
        let a = window
            .iter()
            .enumerate()
            .map(|(i, x)| x * phase(i).sin())
            .sum::<f64>()
            * 2.0
            / n;
        let b = window
            .iter()
            .enumerate()
            .map(|(i, x)| x * phase(i).cos())
            .sum::<f64>()
            * 2.0
            / n;
        let rest: f64 = window
            .iter()
            .enumerate()
            .map(|(i, x)| (x - a * phase(i).sin() - b * phase(i).cos()).powi(2))
            .sum::<f64>()
            / n;
        10.0 * (rest / ((a * a + b * b) / 2.0)).log10()
    }

    fn master(&self, name: &str, args: &[&str]) -> (PathBuf, Output) {
        let output = self.path(name);
        let run = self.fadercut(&[args, &["--output", output.to_str().unwrap()]].concat());
        (output, run)
    }
}

const TONE_1K: &str = "aevalsrc=0.2512*sin(2*PI*1000*t)|0.2512*sin(2*PI*1000*t):s=48000:d=5";

#[test]
fn a_gain_only_master_is_the_source_times_a_constant() {
    let Some(fx) = Fixture::new() else { return };
    let (output, run) = fx.master("quiet.wav", &["--preset", "streaming", "--target", "-24"]);
    assert!(run.status.success(), "{}", stderr(&run));
    assert!(stdout(&run).contains("limiter: not needed"));
    let residual = fx.null_residual_db(&output);
    assert!(residual < -100.0, "residual {residual} dB");
}

#[test]
fn even_only_leaves_audio_under_the_ceiling_untouched() {
    let Some(fx) = Fixture::new() else { return };
    let (output, run) = fx.master("same.wav", &["--preset", "even-only"]);
    assert!(run.status.success(), "{}", stderr(&run));
    assert_eq!(fx.samples(&fx.input, 2), fx.samples(&output, 2));
}

#[test]
fn a_limited_master_lands_on_target_under_the_ceiling() {
    let Some(fx) = Fixture::new() else { return };
    let (source_i, source_tp) = fx.r128(&fx.input);
    // Ask for 2 dB more than the peaks allow, so the limiter has real work to do.
    let ceiling = -1.0;
    let target = source_i + (ceiling - source_tp) + 2.0;
    let (output, run) = fx.master(
        "loud.wav",
        &["--preset", "club", "--target", &format!("{target:.2}")],
    );
    assert!(run.status.success(), "{}", stderr(&run));
    assert!(stdout(&run).contains("limiter: took"), "{}", stdout(&run));

    let (i, tp) = fx.r128(&output);
    assert!((i - target).abs() <= 0.4, "landed at {i}, target {target}");
    assert!(
        tp <= ceiling + 0.1,
        "true peak {tp} dBTP over the {ceiling} ceiling"
    );
    // The limiter only shaves peaks: what it changed is small next to the programme.
    // Noise has far denser peaks than music (about -24 dB here; -27 dB on real tracks
    // at the same depth), so this bound guards against gross distortion or added delay.
    let residual = fx.null_residual_db(&output);
    assert!(residual < -20.0, "residual {residual} dB");
}

#[test]
fn heavy_limiting_is_reported_with_where_to_listen() {
    let Some(fx) = Fixture::new() else { return };
    let (source_i, source_tp) = fx.r128(&fx.input);
    // 8 dB more than the peaks allow: the limiter is working nearly all the time.
    let target = source_i + (-1.0 - source_tp) + 8.0;
    let (_, run) = fx.master("squashed.wav", &["--target", &format!("{target:.2}")]);
    assert!(run.status.success(), "{}", stderr(&run));

    let log = stderr(&run);
    assert!(log.contains("warning: heavy limiting"), "{log}");
    assert!(log.contains("Where to listen"), "{log}");
    // The second half of the fixture is 6 dB louder, so that is where it is hit.
    let stretch = log
        .lines()
        .skip_while(|l| !l.contains("Where to listen"))
        .nth(1)
        .unwrap_or_else(|| panic!("no stretch listed:\n{log}"));
    assert!(stretch.trim_start().starts_with("0:0"), "{stretch}");
    assert!(stretch.trim_end().ends_with(" LU"), "{stretch}");
    assert!(log.contains("Times are positions in the output file."));
}

#[test]
fn a_clean_master_has_no_listening_guide() {
    let Some(fx) = Fixture::new() else { return };
    let (_, run) = fx.master("clean.wav", &["--preset", "streaming", "--target", "-24"]);
    assert!(run.status.success(), "{}", stderr(&run));
    assert!(!stderr(&run).contains("warning"), "{}", stderr(&run));
    assert!(!stderr(&run).contains("Where to listen"));
}

#[test]
fn a_tone_under_the_ceiling_comes_out_undistorted() {
    let Some(fx) = Fixture::new() else { return };
    let fx = fx.with_source("tone.wav", TONE_1K);
    // -12 dBFS peaks raised by ~4 dB stay well under the ceiling of the loud preset.
    let (output, run) = fx.master("tone-out.wav", &["--preset", "club", "--target", "-8"]);
    assert!(run.status.success(), "{}", stderr(&run));
    let thd = fx.thd_n_db(&output, 1000.0);
    assert!(thd < -90.0, "THD+N {thd} dB");
}

#[test]
fn a_tone_grazing_the_limiter_stays_clean() {
    let Some(fx) = Fixture::new() else { return };
    let fx = fx.with_source("tone.wav", TONE_1K);
    let (source_i, source_tp) = fx.r128(&fx.input);
    // Peaks land 0.1 dB above the limiter threshold: it is in the chain, barely working.
    let target = source_i + (-1.1 - source_tp);
    let (output, run) = fx.master(
        "tone-hot.wav",
        &["--preset", "club", "--target", &format!("{target:.2}")],
    );
    assert!(run.status.success(), "{}", stderr(&run));
    assert!(stdout(&run).contains("limiter: took"), "{}", stdout(&run));
    let (_, tp) = fx.r128(&output);
    assert!(tp <= -0.9, "true peak {tp} dBTP");
    let thd = fx.thd_n_db(&output, 1000.0);
    assert!(thd < -60.0, "THD+N {thd} dB");
}

#[test]
fn the_ceiling_holds_for_peaks_that_fall_between_samples() {
    let Some(fx) = Fixture::new() else { return };
    // A quarter-rate tone sampled 45 degrees off its crests: every sample sits 3 dB
    // below the true peak, so a sample-peak limiter would see nothing to do.
    let tone = "aevalsrc=0.9*sin(2*PI*12000*t+PI/4)|0.9*sin(2*PI*12000*t+PI/4):s=48000:d=5";
    let fx = fx.with_source("intersample.wav", tone);
    let (_, source_tp) = fx.r128(&fx.input);
    let sample_peak = fx
        .samples(&fx.input, 2)
        .iter()
        .fold(0.0_f64, |peak, s| peak.max(s.abs()));
    assert!(source_tp > -1.5 && 20.0 * sample_peak.log10() < -3.5);

    let ceiling = -3.0;
    let (output, run) = fx.master("held.wav", &["--preset", "even-only", "--tp", "-3"]);
    assert!(run.status.success(), "{}", stderr(&run));
    let (_, tp) = fx.r128(&output);
    assert!(
        tp <= ceiling + 0.1,
        "true peak {tp} dBTP over the {ceiling} ceiling"
    );
    // A steady tone cannot keep its loudness under a lower ceiling; the tool says so.
    assert!(stderr(&run).contains("short of the"), "{}", stderr(&run));
}

#[test]
fn a_failed_render_leaves_no_file_behind() {
    let Some(fx) = Fixture::new() else { return };
    let missing_dir = fx.path("no-such-dir").join("out.wav");
    for leveling in ["off", "gentle"] {
        let run = fx.fadercut(&[
            "--leveling",
            leveling,
            "--output",
            missing_dir.to_str().unwrap(),
        ]);
        assert_eq!(run.status.code(), Some(1), "{}", stderr(&run));
    }
    let leftovers: Vec<_> = std::fs::read_dir(fx.dir.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| name.contains("partial"))
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");
}

// ---------------------------------------------------------------------------
// Audacity projects as input. The fixtures are written by `aup_fixture` in the
// same on-disk format Audacity uses.
// ---------------------------------------------------------------------------

use aup_fixture::{Block, ClipSpec, TrackSpec};

impl Fixture {
    /// Decoded samples as 24-bit integers, interleaved.
    fn samples_i24(&self, file: &Path) -> Vec<i32> {
        let out = Command::new(&self.ffmpeg)
            .args(["-v", "error", "-i"])
            .arg(file)
            .args(["-f", "s32le", "-"])
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert!(out.status.success(), "{}", stderr(&out));
        out.stdout
            .chunks_exact(4)
            .map(|b| i32::from_le_bytes([b[0], b[1], b[2], b[3]]) >> 8)
            .collect()
    }

    /// An Audacity project holding the same audio as the default WAV input.
    fn as_project(&self, name: &str) -> PathBuf {
        let interleaved = self.samples_i24(&self.input);
        let left: Vec<i32> = interleaved.iter().step_by(2).copied().collect();
        let right: Vec<i32> = interleaved.iter().skip(1).step_by(2).copied().collect();
        let path = self.path(name);
        // A block length that divides nothing evenly, as in a real recording.
        aup_fixture::stereo_project(&path, SAMPLE_RATE, &left, &right, 70_001);
        path
    }

    fn run_on(&self, input: &Path, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_fadercut"))
            .arg(input)
            .args(args)
            .stdin(Stdio::null())
            .output()
            .unwrap()
    }

    fn files(&self) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(self.dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }
}

/// Deterministic noise on the 24-bit grid at about -18 dBFS, in even numbers so
/// that halving a sample is exact.
fn noise(seed: u32, len: usize) -> Vec<i32> {
    let mut state = seed;
    (0..len)
        .map(|_| {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            ((state >> 11) as i32 % (1 << 20) - (1 << 19)) * 2
        })
        .collect()
}

#[test]
fn a_project_masters_exactly_like_its_exported_audio() {
    let Some(fx) = Fixture::new() else { return };
    let project = fx.as_project("set.aup4");
    let project_bytes = std::fs::read(&project).unwrap();

    for (name, args) in [
        ("loud", vec!["--preset", "club", "--target", "-16"]),
        (
            "ride",
            vec!["--leveling", "gentle", "--target", "-24", "--bass-mono"],
        ),
        (
            "clip",
            vec!["--preset", "even-only", "--preview", "0:02-0:05"],
        ),
    ] {
        let from_wav = fx.path(&format!("{name}-wav.wav"));
        let from_project = fx.path(&format!("{name}-aup.wav"));
        let mut wav_args = args.clone();
        wav_args.extend(["--output", from_wav.to_str().unwrap()]);
        let mut project_args = args.clone();
        project_args.extend(["--output", from_project.to_str().unwrap()]);

        let run = fx.fadercut(&wav_args);
        assert!(run.status.success(), "{name}: {}", stderr(&run));
        let run = fx.run_on(&project, &project_args);
        assert!(run.status.success(), "{name}: {}", stderr(&run));
        assert!(stderr(&run).contains("Audacity project: 1 audible track, 48000 Hz"));

        assert_eq!(
            fx.samples_i24(&from_project),
            fx.samples_i24(&from_wav),
            "{name}: project and WAV masters differ"
        );
    }

    // The project is a source only: same bytes, and nothing was left next to it.
    assert_eq!(std::fs::read(&project).unwrap(), project_bytes);
    let stray: Vec<String> = fx
        .files()
        .into_iter()
        .filter(|f| f.starts_with("set.aup4-") || f.contains(".source-") || f.contains("journal"))
        .collect();
    assert!(stray.is_empty(), "{stray:?}");
}

#[test]
fn a_project_defaults_to_an_output_named_after_it() {
    let Some(fx) = Fixture::new() else { return };
    let project = fx.as_project("friday night.aup4");
    let run = fx.run_on(&project, &["--preset", "even-only"]);
    assert!(run.status.success(), "{}", stderr(&run));
    assert!(stderr(&run).contains("Analyzing loudness of friday night.aup4"));
    assert!(fx.path("friday night-mastered.wav").is_file());
}

#[test]
fn cuts_and_multiple_tracks_mix_down_to_one_file() {
    let Some(fx) = Fixture::new() else { return };
    let rate = 48_000usize;
    // A stereo recording cut in two, with the second part trimmed on both sides and
    // moved, plus a quieter mono track laid over the join. Everything sits one second
    // into the timeline.
    let (a_left, a_right) = (noise(1, 2 * rate), noise(2, 2 * rate));
    let (b_left, b_right) = (noise(3, 3 * rate), noise(4, 3 * rate));
    let over = noise(5, 2 * rate);

    let block = |id: i64, samples: &[i32]| Block::int24(id, samples);
    let blocks = vec![
        block(11, &a_left[..rate]),
        block(12, &a_left[rate..]),
        block(21, &a_right[..rate]),
        block(22, &a_right[rate..]),
        block(31, &b_left),
        block(41, &b_right),
        block(51, &over),
    ];
    let second_part = |id: i64| ClipSpec {
        // Stored audio starts at 3.5 s; half a second is trimmed off each end, so it
        // is heard from 4.0 s to 6.0 s.
        offset: 3.5,
        trim_left: 0.5,
        trim_right: 0.5,
        blocks: vec![(id, 3 * rate as i64)],
        envelope: vec![],
    };
    let first_part = |ids: [i64; 2]| ClipSpec {
        offset: 1.0,
        trim_left: 0.0,
        trim_right: 0.0,
        blocks: vec![(ids[0], rate as i64), (ids[1], rate as i64)],
        envelope: vec![],
    };
    let mut left = TrackSpec::new(48_000.0, vec![first_part([11, 12]), second_part(31)]);
    left.linked = true;
    let right = TrackSpec::new(48_000.0, vec![first_part([21, 22]), second_part(41)]);
    let mut overlay = TrackSpec::new(
        48_000.0,
        vec![ClipSpec {
            offset: 2.5,
            trim_left: 0.0,
            trim_right: 0.0,
            blocks: vec![(51, 2 * rate as i64)],
            envelope: vec![],
        }],
    );
    overlay.volume = 0.5;
    let mut muted = TrackSpec::new(
        48_000.0,
        vec![ClipSpec::at_zero(vec![(51, 2 * rate as i64)])],
    );
    muted.mute = true;

    let project = fx.path("edit.aup4");
    let (dict, doc) = aup_fixture::project_document(48_000.0, &[left, right, overlay, muted]);
    aup_fixture::write_project(&project, &dict, &doc, &blocks);

    // What Audacity would play, from the first clip (1.0 s) to the last (6.0 s).
    let mut expected = vec![0i32; 5 * rate * 2];
    let mut add = |at_s: f64, left: &[i32], right: &[i32], halve: bool| {
        let first = ((at_s - 1.0) * rate as f64) as usize;
        for (i, (l, r)) in left.iter().zip(right).enumerate() {
            let scale = if halve { 2 } else { 1 };
            expected[(first + i) * 2] += l / scale;
            expected[(first + i) * 2 + 1] += r / scale;
        }
    };
    add(1.0, &a_left, &a_right, false);
    add(
        4.0,
        &b_left[rate / 2..5 * rate / 2],
        &b_right[rate / 2..5 * rate / 2],
        false,
    );
    add(2.5, &over, &over, true);

    let output = fx.path("edit-mastered.wav");
    let run = fx.run_on(&project, &["--preset", "even-only"]);
    assert!(run.status.success(), "{}", stderr(&run));
    assert!(
        stderr(&run).contains("2 audible tracks, 48000 Hz"),
        "{}",
        stderr(&run)
    );
    assert!(stderr(&run).contains("leaving out 1.0s of empty timeline"));
    assert_eq!(
        fx.probe(&output, "stream=sample_rate,channels"),
        ["48000", "2"]
    );
    assert_eq!(fx.samples_i24(&output), expected);
}

#[test]
fn a_clips_volume_envelope_is_applied() {
    let Some(fx) = Fixture::new() else { return };
    let rate = 48_000usize;
    let (left, right) = (noise(6, 3 * rate), noise(7, 3 * rate));
    // Half a second of the stored audio is trimmed away. The envelope, timed from the
    // start of the stored audio, holds unity until 1.0 s, fades by 20 dB over the
    // next second, and stays there.
    let faded = |id: i64| ClipSpec {
        offset: 0.0,
        trim_left: 0.5,
        trim_right: 0.0,
        blocks: vec![(id, 3 * rate as i64)],
        envelope: vec![(1.0, 1.0), (2.0, 0.1)],
    };
    let mut left_track = TrackSpec::new(48_000.0, vec![faded(1)]);
    left_track.linked = true;
    let right_track = TrackSpec::new(48_000.0, vec![faded(2)]);
    let project = fx.path("fade.aup4");
    let (dict, doc) = aup_fixture::project_document(48_000.0, &[left_track, right_track]);
    let blocks = [Block::int24(1, &left), Block::int24(2, &right)];
    aup_fixture::write_project(&project, &dict, &doc, &blocks);

    let run = fx.run_on(&project, &["--preset", "even-only"]);
    assert!(run.status.success(), "{}", stderr(&run));
    let mastered = fx.samples_i24(&fx.path("fade-mastered.wav"));
    assert_eq!(mastered.len(), 5 * rate);

    // Audacity moves between envelope points in a straight line in decibels.
    let gain_at = |stored_frame: usize| {
        let t = stored_frame as f64 / rate as f64;
        10f64.powf(-(t - 1.0).clamp(0.0, 1.0))
    };
    for (i, frame) in mastered.chunks_exact(2).enumerate() {
        let stored = i + rate / 2;
        for (got, source) in frame.iter().zip([left[stored], right[stored]]) {
            let expected = f64::from(source) * gain_at(stored);
            assert!(
                (f64::from(*got) - expected).abs() <= 1.0,
                "frame {i}: {got} vs {expected}"
            );
        }
    }
}

#[test]
fn project_features_that_cannot_be_reproduced_are_refused() {
    let Some(fx) = Fixture::new() else { return };
    let project = fx.path("stretched.aup4");
    let mut w = aup_fixture::XmlWriter::new();
    w.start("project");
    w.start("wavetrack");
    w.double("rate", 48_000.0);
    w.start("waveclip");
    w.double("clipStretchRatio", 1.1);
    w.start("sequence");
    w.long_long("numsamples", 48_000);
    w.start("waveblock");
    w.long_long("start", 0);
    w.long_long("blockid", 1);
    w.end("waveblock");
    w.end("sequence");
    w.end("waveclip");
    w.end("wavetrack");
    w.end("project");
    let (dict, doc) = w.finish();
    aup_fixture::write_project(&project, &dict, &doc, &[Block::int24(1, &noise(1, 48_000))]);

    let run = fx.run_on(&project, &[]);
    assert_eq!(run.status.code(), Some(2), "{}", stderr(&run));
    let message = stderr(&run);
    assert!(
        message.contains("a stretched or pitch-shifted clip"),
        "{message}"
    );
    assert!(
        message.contains("Export the audio from Audacity"),
        "{message}"
    );
    assert_eq!(fx.files(), ["set.wav", "stretched.aup4"]);
}

#[test]
fn a_project_open_in_audacity_is_refused_and_left_alone() {
    let Some(fx) = Fixture::new() else { return };
    let project = fx.as_project("live.aup4");
    let wal = fx.path("live.aup4-wal");
    std::fs::write(&wal, b"pending writes").unwrap();

    let run = fx.run_on(&project, &["--force"]);
    assert_eq!(run.status.code(), Some(2), "{}", stderr(&run));
    assert!(
        stderr(&run).contains("open in Audacity"),
        "{}",
        stderr(&run)
    );
    assert_eq!(std::fs::read(&wal).unwrap(), b"pending writes");
    assert_eq!(fx.files(), ["live.aup4", "live.aup4-wal", "set.wav"]);
}

#[test]
fn a_preview_past_the_end_of_a_project_is_a_usage_error() {
    let Some(fx) = Fixture::new() else { return };
    let project = fx.as_project("short.aup4");
    let run = fx.run_on(&project, &["--preview", "5:00-6:00"]);
    assert_eq!(run.status.code(), Some(2), "{}", stderr(&run));
    assert!(
        stderr(&run).contains("after the end of the project"),
        "{}",
        stderr(&run)
    );
    assert_eq!(fx.files(), ["set.wav", "short.aup4"]);
}
