//! Orchestration: preflight checks, measure, render until on target, report.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

use crate::analysis::{measure_timeline, LoudnessStats, MeterFile, Timeline};
use crate::apply::{apply_envelope, Envelope};
use crate::aup;
use crate::chain::{self, Filter, LowEnd, RenderPlan};
use crate::cli::{Args, Target};
use crate::error::Error;
use crate::ffutil::{clip_args, describe_status, find_tool, probe_audio, tail};
use crate::ffutil::{AudioFormat, Clip};
use crate::hotspots::{self, Hotspots, Stretch};
use crate::landing::{land, Goal, Landing, Loudest, Pass, CLEAN_LIMITING_LU};
use crate::leveler::{compute_gain_db, gain_to_linear, Profile, PRESERVE_LOUD_DOWN};
use crate::presets::Preset;

const ERROR_TAIL_CHARS: usize = 2000;

/// ebur128 reports -70 LUFS when nothing in the programme passes its gate.
const SILENCE_LUFS: f64 = -70.0;

/// The low-end filters rotate phase, which can raise peaks by a few dB even though
/// they only remove energy. Assume this much when deciding whether to limit.
const LOW_END_PEAK_ALLOWANCE_DB: f64 = 3.0;

/// Loudness the limiter may remove before it is audibly working. On the audit's
/// hard-groove test set a cost of ~0.3 LU was clean and ~1.2 LU pumped (see the fix
/// plan); rare spikes cost next to nothing however deeply they are limited.
const HEAVY_LIMITING_LU: f64 = 0.7;

/// Progress goes to stderr; stdout carries only the report and the result path.
fn log(msg: impl AsRef<str>) {
    eprintln!("{}", msg.as_ref());
}

struct Tools {
    ffmpeg: PathBuf,
    ffprobe: PathBuf,
}

impl Tools {
    fn locate() -> Result<Self, Error> {
        match (find_tool("ffmpeg"), find_tool("ffprobe")) {
            (Some(ffmpeg), Some(ffprobe)) => Ok(Self { ffmpeg, ffprobe }),
            _ => Err(Error::Preflight(
                "ffmpeg/ffprobe not found. Install with: brew install ffmpeg".to_owned(),
            )),
        }
    }
}

/// Everything a render path needs, resolved from the command line.
struct Job<'a> {
    ffmpeg: &'a Path,
    /// What the user asked to master, for messages.
    label: &'a str,
    /// The audio file to read: the input itself, or a project's mixdown.
    input: &'a Path,
    output: &'a Path,
    preset: Preset,
    /// Set when the target is the loudest clean master rather than a fixed level.
    loudest: Option<Loudest>,
    clip: Option<Clip>,
    low_end: LowEnd,
    format: AudioFormat,
    dry_run: bool,
}

impl Job<'_> {
    /// What the master must measure; even-only presets keep the measured loudness.
    fn goal(&self, before: &LoudnessStats) -> Result<Goal, Error> {
        reject_silence(before)?;
        let loudness = self.preset.target_i.unwrap_or_else(|| {
            log(format!(
                "      even-only: target set to measured {:+.1} LUFS",
                before.integrated
            ));
            before.integrated
        });
        Ok(Goal {
            loudness,
            ceiling_db: self.preset.target_tp,
        })
    }

    /// The first render the landing will make.
    fn first_plan(&self, before: &LoudnessStats, extra_peak_db: f64) -> Result<RenderPlan, Error> {
        match &self.loudest {
            Some(loudest) => {
                reject_silence(before)?;
                Ok(loudest.first_plan(before, extra_peak_db))
            }
            None => Ok(self.goal(before)?.first_plan(before, extra_peak_db)),
        }
    }

    /// Render until on target. Returns what the master was held to (for a loudest
    /// search, the loudness it landed at) and the accepted render.
    fn land(
        &self,
        before: &LoudnessStats,
        extra_peak_db: f64,
        mut render: impl FnMut(usize, &RenderPlan) -> Result<Pass, Error>,
    ) -> Result<(Goal, Landing), Error> {
        match &self.loudest {
            Some(loudest) => {
                reject_silence(before)?;
                let landing = loudest.land(before, extra_peak_db, render)?;
                let goal = Goal {
                    loudness: landing.after.integrated,
                    ceiling_db: loudest.ceiling_db,
                };
                Ok((goal, landing))
            }
            None => {
                let goal = self.goal(before)?;
                let first = goal.first_plan(before, extra_peak_db);
                let landing = land(&goal, first, |attempt, plan| {
                    render(attempt, plan).map(|pass| pass.after)
                })?;
                Ok((goal, landing))
            }
        }
    }

    fn low_end_peak_allowance(&self) -> f64 {
        if self.low_end.enabled() {
            LOW_END_PEAK_ALLOWANCE_DB
        } else {
            0.0
        }
    }

    fn filter(&self, plan: &RenderPlan, tap_file: Option<&str>) -> Filter {
        chain::master_filter(
            &self.preset,
            plan,
            self.low_end,
            self.format.sample_rate,
            tap_file,
        )
    }

    fn describe(&self, plan: &RenderPlan) -> String {
        let limiter = match plan.limit_db {
            Some(limit) => format!(
                "limiter at {limit:+.2} dBFS ({}x oversampled)",
                chain::oversample_factor(self.format.sample_rate)
            ),
            None => "no limiter needed".to_owned(),
        };
        format!("gain {:+.2} dB, {limiter}", plan.gain_db)
    }

    fn measure(&self, file: &Path, clip: Option<Clip>) -> Result<LoudnessStats, Error> {
        Ok(measure_timeline(self.ffmpeg, file, clip)?.stats())
    }
}

fn reject_silence(before: &LoudnessStats) -> Result<(), Error> {
    if before.integrated <= SILENCE_LUFS {
        return Err(Error::SilentInput);
    }
    Ok(())
}

/// A temporary audio file written beside the final output (so that moving a finished
/// render into place is an atomic rename, and so that it lands on a disk with room
/// for it). Removed on drop unless committed: a failed or off-target render never
/// replaces, or masquerades as, a finished master.
struct ScratchFile {
    path: PathBuf,
}

impl ScratchFile {
    /// `<output stem>.<tag>-<pid>.wav` next to `output`.
    fn beside(output: &Path, tag: &str) -> Self {
        let mut name = output.file_stem().unwrap_or_default().to_os_string();
        name.push(format!(".{tag}-{}.wav", std::process::id()));
        Self {
            path: output.with_file_name(name),
        }
    }

    fn commit(self, dest: &Path) -> Result<(), Error> {
        std::fs::rename(&self.path, dest).map_err(|source| Error::Finalize {
            path: dest.display().to_string(),
            source,
        })
    }
}

impl Drop for ScratchFile {
    fn drop(&mut self) {
        // Already renamed away after a commit; otherwise discard the attempt.
        let _ = std::fs::remove_file(&self.path);
    }
}

/// What the limiter did in one rendered attempt.
struct LimiterPass {
    /// Loudness it removed over the whole programme (LU).
    cost_lu: f64,
    /// The signal that entered it.
    entering: Timeline,
    /// The file that came out.
    rendered: Timeline,
}

/// Measures each rendered attempt: the file itself and, when the limiter is in the
/// chain, the signal that entered it — the difference is what the limiter cost.
struct PassMeter<'a> {
    job: &'a Job<'a>,
    tap: MeterFile,
    /// The limiter's effect in the most recent attempt, if it was in the chain.
    limiter: Option<LimiterPass>,
}

impl<'a> PassMeter<'a> {
    fn new(job: &'a Job<'a>) -> Result<Self, Error> {
        Ok(Self {
            job,
            tap: MeterFile::new()?,
            limiter: None,
        })
    }

    fn filter(&self, plan: &RenderPlan) -> Filter {
        self.job.filter(plan, Some(self.tap.path()))
    }

    fn measure(&mut self, plan: &RenderPlan, rendered: &Path) -> Result<Pass, Error> {
        let rendered = measure_timeline(self.job.ffmpeg, rendered, None)?;
        let after = rendered.stats();
        self.limiter = match plan.limit_db {
            Some(_) => {
                let entering = self.tap.read()?;
                Some(LimiterPass {
                    cost_lu: entering.integrated - after.integrated,
                    entering,
                    rendered,
                })
            }
            None => None,
        };
        let limiter_cost_lu = self.limiter.as_ref().map_or(0.0, |pass| pass.cost_lu);
        match plan.limit_db {
            Some(_) => log(format!(
                "              -> {}  (limiter {limiter_cost_lu:.2} LU)",
                after.summary()
            )),
            None => log(format!("              -> {}", after.summary())),
        }
        Ok(Pass {
            after,
            limiter_cost_lu,
        })
    }
}

/// Expand a leading `~/` (the shell does not when the path is quoted).
fn expand_tilde(path: &Path) -> PathBuf {
    match (path.strip_prefix("~"), std::env::var_os("HOME")) {
        (Ok(rest), Some(home)) => PathBuf::from(home).join(rest),
        _ => path.to_path_buf(),
    }
}

/// `/x/set.aif` -> `/x/set-mastered.wav`. Always a .wav next to the input.
pub fn derive_output_path(input: &Path, suffix: &str) -> PathBuf {
    let mut name = input.file_stem().unwrap_or_default().to_os_string();
    name.push(suffix);
    name.push(".wav");
    input.with_file_name(name)
}

/// Canonical form of a path that may not exist yet, for same-file comparison.
fn resolve(path: &Path) -> PathBuf {
    if let Ok(canonical) = path.canonicalize() {
        return canonical;
    }
    let absolute = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    match (absolute.parent(), absolute.file_name()) {
        (Some(parent), Some(name)) => parent
            .canonicalize()
            .map(|dir| dir.join(name))
            .unwrap_or(absolute),
        _ => absolute,
    }
}

pub fn build_render_command(
    ffmpeg: &Path,
    input: &Path,
    output: &Path,
    filter: &Filter,
    sample_rate: u32,
    clip: Option<Clip>,
) -> Vec<OsString> {
    let mut cmd: Vec<OsString> = vec![ffmpeg.into(), "-hide_banner".into(), "-y".into()];
    cmd.extend(clip_args(clip).into_iter().map(Into::into));
    cmd.push("-i".into());
    cmd.push(input.into());
    cmd.extend(filter.ffmpeg_args().into_iter().map(Into::into));
    cmd.push("-ar".into());
    cmd.push(sample_rate.to_string().into());
    cmd.push("-c:a".into());
    cmd.push("pcm_s24le".into());
    cmd.push(output.into());
    cmd
}

fn display_command(cmd: &[OsString]) -> String {
    cmd.iter()
        .map(|arg| arg.to_string_lossy())
        .collect::<Vec<_>>()
        .join(" ")
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .unwrap_or(path.as_os_str())
        .to_string_lossy()
        .into_owned()
}

fn passes(n: usize) -> String {
    format!("{n} pass{}", if n == 1 { "" } else { "es" })
}

fn report_lines(
    before: &LoudnessStats,
    landing: &Landing,
    goal: &Goal,
    loudest: Option<&Loudest>,
    limiter_cost_lu: Option<f64>,
    preset_name: &str,
) -> Vec<String> {
    let rule = "=".repeat(56);
    let limiter = match limiter_cost_lu {
        Some(cost) => format!("took {:.1} LU off the loudness", cost.max(0.0)),
        None => "not needed".to_owned(),
    };
    vec![
        String::new(),
        rule.clone(),
        format!("  Loudness report  (preset: {preset_name})"),
        rule.clone(),
        format!("  before : {}", before.summary()),
        format!("  after  : {}", landing.after.summary()),
        match loudest {
            Some(loudest) => format!(
                "  target : loudest with limiter <= {:.1} LU  TP<={:+.1} dBTP  ({})",
                loudest.budget_lu,
                goal.ceiling_db,
                passes(landing.attempts)
            ),
            None => format!(
                "  target : I={:+.1} LUFS  TP<={:+.1} dBTP  ({})",
                goal.loudness,
                goal.ceiling_db,
                passes(landing.attempts)
            ),
        },
        format!("  limiter: {limiter}"),
        rule,
        String::new(),
    ]
}

/// `95.0` -> `1:35`; an hour or more -> `1:03:20`.
fn clock(seconds: f64) -> String {
    let s = seconds.max(0.0).round() as u64;
    if s >= 3600 {
        format!("{}:{:02}:{:02}", s / 3600, s % 3600 / 60, s % 60)
    } else {
        format!("{}:{:02}", s / 60, s % 60)
    }
}

/// Where to listen for the limiter, as lines that follow the heavy-limiting warning.
fn listening_guide(hotspots: &Hotspots) -> Vec<String> {
    let span = |s: &Stretch| format!("{} - {}", clock(s.start_s), clock(s.end_s));
    let mut out = Vec::new();
    if hotspots.heaviest.is_empty() {
        return out;
    }
    out.push("         Where to listen — the stretches the limiter worked on hardest:".to_owned());
    for stretch in &hotspots.heaviest {
        out.push(format!(
            "           {:<19} {:.1} LU",
            span(stretch),
            stretch.cost_lu
        ));
    }
    if let Some(lightest) = &hotspots.lightest {
        out.push(format!(
            "         Least affected, to compare against: {} ({:.1} LU)",
            span(lightest),
            lightest.cost_lu.max(0.0)
        ));
    }
    out.push("         Times are positions in the output file.".to_owned());
    out
}

/// Warnings about a master that was written but is not what was asked for.
///
/// `hotspots` is where the limiter worked hardest; it is listed under the
/// heavy-limiting warning so the listener knows where to check.
fn warnings(
    landing: &Landing,
    goal: &Goal,
    limiter_cost_lu: Option<f64>,
    hotspots: Option<&Hotspots>,
) -> Vec<String> {
    let mut out = Vec::new();
    if !landing.loudness_ok {
        let miss = landing.after.integrated - goal.loudness;
        out.push(format!(
            "warning: landed at {:+.1} LUFS, {:.1} LU {} the {:+.1} LUFS target after {}.",
            landing.after.integrated,
            miss.abs(),
            if miss < 0.0 { "short of" } else { "over" },
            goal.loudness,
            passes(landing.attempts)
        ));
    }
    if let Some(cost) = limiter_cost_lu.filter(|&cost| cost > HEAVY_LIMITING_LU) {
        out.push(format!(
            "warning: heavy limiting — the limiter took {cost:.1} LU off the loudness, which \
             usually pumps audibly. Consider a lower --target."
        ));
        out.extend(hotspots.map(listening_guide).unwrap_or_default());
    }
    out
}

/// Check the landed render, move it into place and report.
fn finish(
    job: &Job,
    goal: &Goal,
    before: &LoudnessStats,
    landing: &Landing,
    limiter: Option<&LimiterPass>,
    partial: ScratchFile,
) -> Result<(), Error> {
    if !landing.peak_ok {
        return Err(Error::CeilingExceeded {
            measured: landing.after.true_peak,
            ceiling: goal.ceiling_db,
            attempts: landing.attempts,
        });
    }
    partial.commit(job.output)?;
    let limiter_cost_lu = limiter.map(|pass| pass.cost_lu);
    let hotspots = limiter
        .filter(|pass| pass.cost_lu > HEAVY_LIMITING_LU)
        .map(|pass| hotspots::find(&pass.entering, &pass.rendered));
    for warning in warnings(landing, goal, limiter_cost_lu, hotspots.as_ref()) {
        log(warning);
    }
    let loudest = job.loudest.as_ref();
    for line in report_lines(
        before,
        landing,
        goal,
        loudest,
        limiter_cost_lu,
        job.preset.name,
    ) {
        println!("{line}");
    }
    println!("Done: {}", job.output.display());
    Ok(())
}

pub fn run(args: &Args) -> Result<(), Error> {
    let tools = Tools::locate()?;

    let input = expand_tilde(&args.input);
    if !input.is_file() {
        return Err(Error::Preflight(format!(
            "input not found: {}",
            input.display()
        )));
    }

    let clip = args.preview;
    let suffix = if clip.is_some() {
        "-preview"
    } else {
        "-mastered"
    };
    let output = match &args.output {
        Some(path) => expand_tilde(path),
        None => derive_output_path(&input, suffix),
    };
    if resolve(&output) == resolve(&input) {
        return Err(Error::Preflight(
            "output would overwrite the input file.".to_owned(),
        ));
    }
    if output.exists() && !args.force && !args.dry_run {
        return Err(Error::Preflight(format!(
            "output exists (use --force or --output): {}",
            output.display()
        )));
    }
    let fixed_target = match args.target {
        Some(Target::Lufs(lufs)) => Some(lufs),
        Some(Target::Max) | None => None,
    };
    let preset = args.preset.preset().with_overrides(fixed_target, args.tp);
    let loudest = (args.target == Some(Target::Max)).then_some(Loudest {
        ceiling_db: preset.target_tp,
        budget_lu: CLEAN_LIMITING_LU,
    });
    if loudest.is_some() {
        log(format!(
            "Target: the loudest master with the limiter taking at most {CLEAN_LIMITING_LU:.1} LU"
        ));
    }
    let low_end = LowEnd {
        subsonic_hz: args.subsonic,
        bass_mono_hz: args.bass_mono,
    };
    if let Some(clip) = clip {
        log(format!(
            "Preview mode: {}s for {}s",
            clip.start, clip.duration
        ));
    }
    if low_end.enabled() {
        let subsonic = low_end.subsonic_hz.map(|hz| format!("subsonic HPF {hz}Hz"));
        let bass_mono = low_end.bass_mono_hz.map(|hz| format!("bass-mono <{hz}Hz"));
        let bits: Vec<String> = subsonic.into_iter().chain(bass_mono).collect();
        log(format!("Low-end: {}", bits.join(", ")));
    }

    // An Audacity project is mixed down to a temporary WAV first (only the preview
    // range, if one was asked for); from there on it is handled like any audio file.
    let mixdown = aup::is_project(&input).then(|| ScratchFile::beside(&output, "source"));
    let (source, clip) = match &mixdown {
        Some(scratch) => {
            mix_down_project(&tools.ffmpeg, &input, &scratch.path, clip)?;
            (scratch.path.as_path(), None)
        }
        None => (input.as_path(), clip),
    };
    let Some(format) = probe_audio(&tools.ffprobe, source) else {
        return Err(Error::Preflight(format!(
            "no readable audio stream in: {}",
            input.display()
        )));
    };

    let label = file_name(&input);
    let job = Job {
        ffmpeg: &tools.ffmpeg,
        label: &label,
        input: source,
        output: &output,
        preset,
        loudest,
        clip,
        low_end,
        format,
        dry_run: args.dry_run,
    };
    match args.leveling.profile() {
        None => run_transparent(&job),
        Some(profile) => run_leveling(&job, profile, args.preserve_loud),
    }
}

/// Open an Audacity project read-only and write its mixdown to `dest`.
fn mix_down_project(
    ffmpeg: &Path,
    project: &Path,
    dest: &Path,
    range: Option<Clip>,
) -> Result<(), Error> {
    // Nothing has been written yet, so a project we cannot use is a preflight failure.
    let preflight = |err: aup::AupError| Error::Preflight(err.to_string());
    let project = aup::Project::open(project).map_err(preflight)?;
    let layout = project.layout();
    log(format!("Audacity project: {}", layout.describe()));
    if layout.lead_in_frames > 0 {
        log(format!(
            "      leaving out {:.1}s of empty timeline before the first clip",
            layout.lead_in_frames as f64 / f64::from(layout.sample_rate)
        ));
    }
    if project.has_unsaved_changes() {
        log("warning: the project has unsaved changes; using its last saved state.");
    }
    project
        .export_wav(ffmpeg, dest, range, true)
        .map_err(|err| match err {
            aup::AupError::EmptyRange(_) => preflight(err),
            other => Error::Project(other),
        })
}

/// Peaks-only path: constant gain, plus the limiter if peaks need it.
fn run_transparent(job: &Job) -> Result<(), Error> {
    log(format!("[1/2] Analyzing loudness of {} ...", job.label));
    let started = Instant::now();
    let before = job.measure(job.input, job.clip)?;
    log(format!(
        "      {}  ({:.0}s)",
        before.summary(),
        started.elapsed().as_secs_f64()
    ));
    let extra_peak_db = job.low_end_peak_allowance();
    let first = job.first_plan(&before, extra_peak_db)?;

    let command = |filter: &Filter, output: &Path| {
        build_render_command(
            job.ffmpeg,
            job.input,
            output,
            filter,
            job.format.sample_rate,
            job.clip,
        )
    };
    if job.dry_run {
        log(format!("\n[dry-run] before: {}", before.summary()));
        log(format!("[dry-run] first pass: {}", job.describe(&first)));
        log(format!(
            "[dry-run] render command:\n  {}\n",
            display_command(&command(&job.filter(&first, None), job.output))
        ));
        return Ok(());
    }

    log(format!("[2/2] Rendering -> {} ...", file_name(job.output)));
    let partial = ScratchFile::beside(job.output, "partial");
    let mut meter = PassMeter::new(job)?;
    let (goal, landing) = job.land(&before, extra_peak_db, |attempt, plan| {
        log(format!("      pass {attempt}: {}", job.describe(plan)));
        let cmd = command(&meter.filter(plan), &partial.path);
        let out = Command::new(&cmd[0])
            .args(&cmd[1..])
            .output()
            .map_err(Error::Spawn)?;
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            return Err(Error::Render {
                status: describe_status(out.status),
                stderr_tail: tail(&stderr, ERROR_TAIL_CHARS).to_owned(),
            });
        }
        meter.measure(plan, &partial.path)
    })?;
    finish(
        job,
        &goal,
        &before,
        &landing,
        meter.limiter.as_ref(),
        partial,
    )
}

/// Gain-riding path: timeline -> ride sections -> gain, plus the limiter if needed.
fn run_leveling(job: &Job, profile: Profile, preserve_loud: bool) -> Result<(), Error> {
    let mut params = profile.params();
    if preserve_loud {
        params.down_strength = PRESERVE_LOUD_DOWN;
    }

    log(format!(
        "[1/2] Building loudness timeline (leveling={}{}) ...",
        profile.name(),
        if preserve_loud { ", preserve-loud" } else { "" }
    ));
    let started = Instant::now();
    let timeline = measure_timeline(job.ffmpeg, job.input, job.clip)?;
    let before = timeline.stats();
    let duration_s = timeline.times.last().copied().unwrap_or(0.0);
    log(format!(
        "      {} frames over {:.1} min | {}  ({:.0}s)",
        timeline.times.len(),
        duration_s / 60.0,
        before.summary(),
        started.elapsed().as_secs_f64()
    ));
    let ride_db = compute_gain_db(
        &timeline.times,
        &timeline.short_term,
        &timeline.momentary,
        timeline.integrated,
        &params,
    );
    let (ride_min, ride_max) = ride_db
        .iter()
        .fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), &g| {
            (lo.min(g), hi.max(g))
        });
    log(format!("      ride {ride_min:+.2}..{ride_max:+.2} dB"));
    // The ride's largest boost lands on top of the constant gain.
    let extra_peak_db = ride_max.max(0.0) + job.low_end_peak_allowance();
    let first = job.first_plan(&before, extra_peak_db)?;

    if job.dry_run {
        log(format!("\n[dry-run] before: {}", before.summary()));
        log(format!("[dry-run] first pass: {}", job.describe(&first)));
        log(format!(
            "[dry-run] post filter: {}\n",
            job.filter(&first, None).spec
        ));
        return Ok(());
    }

    log(format!(
        "[2/2] Rendering (gain-ride) -> {}",
        file_name(job.output)
    ));
    let ride_lin = gain_to_linear(&ride_db);
    let envelope = Envelope::new(&timeline.times, &ride_lin)?;
    let partial = ScratchFile::beside(job.output, "partial");
    let mut meter = PassMeter::new(job)?;
    let (goal, landing) = job.land(&before, extra_peak_db, |attempt, plan| {
        log(format!("      pass {attempt}: {}", job.describe(plan)));
        apply_envelope(
            job.ffmpeg,
            job.input,
            &partial.path,
            envelope,
            job.format,
            &meter.filter(plan),
            job.clip,
            true,
        )?;
        meter.measure(plan, &partial.path)
    })?;
    finish(
        job,
        &goal,
        &before,
        &landing,
        meter.limiter.as_ref(),
        partial,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::presets::PresetName;

    const SOURCE: LoudnessStats = LoudnessStats {
        integrated: -15.2,
        true_peak: -5.1,
        lra: 4.5,
    };

    fn filter(spec: &str) -> Filter {
        Filter {
            spec: spec.to_owned(),
            complex: false,
        }
    }

    fn as_strings(cmd: &[OsString]) -> Vec<String> {
        cmd.iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    fn job(preset: PresetName, low_end: LowEnd) -> Job<'static> {
        Job {
            ffmpeg: Path::new("ffmpeg"),
            label: "in.wav",
            input: Path::new("/in.wav"),
            output: Path::new("/out.wav"),
            preset: preset.preset(),
            loudest: None,
            clip: None,
            low_end,
            format: AudioFormat {
                sample_rate: 44_100,
                channels: 2,
            },
            dry_run: false,
        }
    }

    fn landing(after: LoudnessStats, limit_db: Option<f64>, loudness_ok: bool) -> Landing {
        Landing {
            plan: RenderPlan {
                gain_db: 6.2,
                limit_db,
            },
            after,
            attempts: 2,
            loudness_ok,
            peak_ok: true,
        }
    }

    #[test]
    fn derive_output_path_appends_suffix() {
        let out = derive_output_path(
            Path::new("/m/Recordings/hard-groove-jun-16-2026.wav"),
            "-mastered",
        );
        assert_eq!(
            out,
            Path::new("/m/Recordings/hard-groove-jun-16-2026-mastered.wav")
        );
    }

    #[test]
    fn derive_output_path_strips_other_extensions() {
        let out = derive_output_path(Path::new("/m/set.aif"), "-preview");
        assert_eq!(out, Path::new("/m/set-preview.wav"));
    }

    #[test]
    fn render_command_uses_24bit_and_sample_rate() {
        let cmd = as_strings(&build_render_command(
            Path::new("ffmpeg"),
            Path::new("/in.wav"),
            Path::new("/out.wav"),
            &filter("volume=1dB"),
            96_000,
            None,
        ));
        assert_eq!(
            cmd,
            [
                "ffmpeg",
                "-hide_banner",
                "-y",
                "-i",
                "/in.wav",
                "-af",
                "volume=1dB",
                "-ar",
                "96000",
                "-c:a",
                "pcm_s24le",
                "/out.wav"
            ]
        );
    }

    #[test]
    fn render_command_seeks_before_input_and_maps_complex_graphs() {
        let graph = Filter {
            spec: "[0:a]anull[out]".to_owned(),
            complex: true,
        };
        let clip = Clip {
            start: 30.0,
            duration: 60.0,
        };
        let cmd = as_strings(&build_render_command(
            Path::new("ffmpeg"),
            Path::new("/in.wav"),
            Path::new("/out.wav"),
            &graph,
            44_100,
            Some(clip),
        ));
        let pos = |arg: &str| cmd.iter().position(|a| a == arg).unwrap();
        assert!(pos("-ss") < pos("-i"));
        assert_eq!(cmd[pos("-filter_complex") + 1], "[0:a]anull[out]");
        assert_eq!(cmd[pos("-map") + 1], "[out]");
    }

    #[test]
    fn goal_uses_the_preset_target_and_ceiling() {
        let goal = job(PresetName::Club, LowEnd::default())
            .goal(&SOURCE)
            .unwrap();
        assert_eq!((goal.loudness, goal.ceiling_db), (-9.0, -1.0));
    }

    #[test]
    fn even_only_goal_is_the_measured_loudness() {
        let goal = job(PresetName::EvenOnly, LowEnd::default())
            .goal(&SOURCE)
            .unwrap();
        assert_eq!(goal.loudness, SOURCE.integrated);
    }

    #[test]
    fn goal_rejects_silent_input() {
        let silent = LoudnessStats {
            integrated: -70.0,
            true_peak: -120.0,
            lra: 0.0,
        };
        let err = job(PresetName::Club, LowEnd::default())
            .goal(&silent)
            .unwrap_err();
        assert!(matches!(err, Error::SilentInput));
        assert_eq!(err.exit_code(), 1);
    }

    #[test]
    fn low_end_processing_reserves_peak_headroom() {
        let low_end = LowEnd {
            subsonic_hz: Some(28.0),
            bass_mono_hz: None,
        };
        let plain = job(PresetName::Club, LowEnd::default());
        assert_eq!(plain.low_end_peak_allowance(), 0.0);
        assert_eq!(
            job(PresetName::Club, low_end).low_end_peak_allowance(),
            LOW_END_PEAK_ALLOWANCE_DB
        );
    }

    #[test]
    fn describe_names_the_limiter_and_its_oversampling() {
        let job = job(PresetName::Club, LowEnd::default());
        let limited = RenderPlan {
            gain_db: 6.2,
            limit_db: Some(-1.2),
        };
        assert_eq!(
            job.describe(&limited),
            "gain +6.20 dB, limiter at -1.20 dBFS (4x oversampled)"
        );
        let plain = RenderPlan {
            gain_db: -1.0,
            limit_db: None,
        };
        assert_eq!(job.describe(&plain), "gain -1.00 dB, no limiter needed");
    }

    const CLUB_GOAL: Goal = Goal {
        loudness: -9.0,
        ceiling_db: -1.0,
    };
    const LANDED: LoudnessStats = LoudnessStats {
        integrated: -9.1,
        true_peak: -1.1,
        lra: 4.1,
    };

    #[test]
    fn report_shows_target_passes_and_limiter_cost() {
        let landed = landing(LANDED, Some(-1.2), true);
        let lines = report_lines(&SOURCE, &landed, &CLUB_GOAL, None, Some(0.34), "club");
        assert!(lines.contains(&"  before : I=-15.2 LUFS  TP=-5.1 dBTP  LRA=4.5 LU".to_owned()));
        assert!(lines.contains(&"  after  : I=-9.1 LUFS  TP=-1.1 dBTP  LRA=4.1 LU".to_owned()));
        assert!(lines.contains(&"  target : I=-9.0 LUFS  TP<=-1.0 dBTP  (2 passes)".to_owned()));
        assert!(lines.contains(&"  limiter: took 0.3 LU off the loudness".to_owned()));
    }

    #[test]
    fn report_names_a_loudest_search_as_the_target() {
        let landed = landing(LANDED, Some(-1.2), true);
        let loudest = Loudest {
            ceiling_db: -1.0,
            budget_lu: 0.5,
        };
        let lines = report_lines(
            &SOURCE,
            &landed,
            &CLUB_GOAL,
            Some(&loudest),
            Some(0.48),
            "club",
        );
        assert!(lines.contains(
            &"  target : loudest with limiter <= 0.5 LU  TP<=-1.0 dBTP  (2 passes)".to_owned()
        ));
    }

    #[test]
    fn report_says_when_the_limiter_was_left_out() {
        let landed = landing(LANDED, None, true);
        let lines = report_lines(&SOURCE, &landed, &CLUB_GOAL, None, None, "club");
        assert!(lines.contains(&"  limiter: not needed".to_owned()));
    }

    #[test]
    fn no_warnings_for_a_clean_landing() {
        let landed = landing(LANDED, Some(-1.2), true);
        assert!(warnings(&landed, &CLUB_GOAL, Some(0.3), None).is_empty());
        assert!(warnings(&landed, &CLUB_GOAL, None, None).is_empty());
    }

    #[test]
    fn warns_when_the_target_was_not_reached() {
        let short = LoudnessStats {
            integrated: -9.6,
            ..LANDED
        };
        let out = warnings(
            &landing(short, Some(-1.2), false),
            &CLUB_GOAL,
            Some(0.3),
            None,
        );
        assert_eq!(out.len(), 1);
        assert!(out[0].contains("landed at -9.6 LUFS, 0.6 LU short of the -9.0 LUFS target"));
    }

    #[test]
    fn warns_about_heavy_limiting() {
        let landed = landing(LANDED, Some(-1.2), true);
        let out = warnings(&landed, &CLUB_GOAL, Some(1.24), None);
        assert_eq!(out.len(), 1);
        assert!(out[0].contains("heavy limiting"), "{}", out[0]);
        assert!(out[0].contains("1.2 LU"), "{}", out[0]);
    }

    fn stretch(start_s: f64, cost_lu: f64) -> Stretch {
        Stretch {
            start_s,
            end_s: start_s + 30.0,
            cost_lu,
        }
    }

    #[test]
    fn heavy_limiting_warning_says_where_to_listen() {
        let landed = landing(LANDED, Some(-1.2), true);
        let hotspots = Hotspots {
            heaviest: vec![stretch(383.0, 2.14), stretch(3770.0, 1.86)],
            lightest: Some(stretch(45.0, 0.04)),
        };
        let out = warnings(&landed, &CLUB_GOAL, Some(1.7), Some(&hotspots));
        assert!(out[0].starts_with("warning: heavy limiting"));
        assert_eq!(
            out[1..],
            [
                "         Where to listen — the stretches the limiter worked on hardest:",
                "           6:23 - 6:53         2.1 LU",
                "           1:02:50 - 1:03:20   1.9 LU",
                "         Least affected, to compare against: 0:45 - 1:15 (0.0 LU)",
                "         Times are positions in the output file.",
            ]
        );
    }

    #[test]
    fn listening_guide_only_accompanies_the_heavy_limiting_warning() {
        let landed = landing(LANDED, Some(-1.2), true);
        let hotspots = Hotspots {
            heaviest: vec![stretch(60.0, 0.4)],
            lightest: None,
        };
        assert!(warnings(&landed, &CLUB_GOAL, Some(0.4), Some(&hotspots)).is_empty());
        assert!(listening_guide(&Hotspots::default()).is_empty());
    }

    #[test]
    fn clock_switches_to_hours_past_sixty_minutes() {
        assert_eq!(clock(0.4), "0:00");
        assert_eq!(clock(95.0), "1:35");
        assert_eq!(clock(3599.6), "1:00:00");
        assert_eq!(clock(3800.0), "1:03:20");
    }

    #[test]
    fn scratch_file_sits_beside_the_output_and_is_removed_unless_committed() {
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("set-mastered.wav");

        let abandoned = ScratchFile::beside(&output, "partial");
        let abandoned_path = abandoned.path.clone();
        assert_eq!(abandoned_path.parent(), output.parent());
        assert_eq!(abandoned_path.extension().unwrap(), "wav");
        std::fs::write(&abandoned_path, b"attempt").unwrap();
        drop(abandoned);
        assert!(!abandoned_path.exists());

        let kept = ScratchFile::beside(&output, "partial");
        std::fs::write(&kept.path, b"master").unwrap();
        kept.commit(&output).unwrap();
        assert_eq!(std::fs::read(&output).unwrap(), b"master");
    }

    #[test]
    fn resolve_handles_missing_files_and_relative_paths() {
        let dir = tempfile::tempdir().unwrap();
        let existing = dir.path().join("set.wav");
        std::fs::write(&existing, b"").unwrap();
        let via_dot = dir.path().join(".").join("set.wav");
        assert_eq!(resolve(&via_dot), resolve(&existing));
        let missing = dir.path().join("out.wav");
        assert_eq!(
            resolve(&missing),
            dir.path().canonicalize().unwrap().join("out.wav")
        );
    }

    #[test]
    fn preflight_errors_exit_with_usage_code() {
        assert_eq!(Error::Preflight("x".to_owned()).exit_code(), 2);
    }
}
