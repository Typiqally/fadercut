//! From a project document to a mixdown plan: which sample blocks play where, and at
//! what gain in each output channel.
//!
//! Follows what Audacity itself does when it plays or exports a project. Anything it
//! would render that this does not reproduce (stretched clips, realtime effects,
//! mixed sample rates) is refused rather than rendered differently. Pure — no I/O.

use super::envelope::Envelope;
use super::xml::Element;

#[derive(Debug, PartialEq, thiserror::Error)]
pub enum LayoutError {
    #[error("not an Audacity project document (root element is <{0}>)")]
    NotAProject(String),
    #[error("the project has no audible audio (no wave tracks, or all muted or empty)")]
    NoAudio,
    #[error(
        "the project uses {0}, which fadercut cannot reproduce. Export the audio from \
         Audacity (WAV, 32-bit float) and master that file instead."
    )]
    Unsupported(&'static str),
    #[error("the project document is inconsistent: {0}")]
    Invalid(String),
}

/// A run of samples inside one stored block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlockRef {
    /// Row id in `sampleblocks`, or `None` for a block of silence.
    pub id: Option<i64>,
    /// Samples to skip at the start of the stored block.
    pub skip: usize,
    /// Samples used from the block.
    pub len: usize,
    /// Position of this run within its clip, in frames.
    pub at: u64,
}

/// A contiguous stretch of audio on the output timeline.
#[derive(Debug, Clone, PartialEq)]
pub struct Clip {
    /// First output frame.
    pub start: u64,
    pub frames: u64,
    pub blocks: Vec<BlockRef>,
    /// The clip's volume envelope, when it has one that changes the audio.
    pub envelope: Option<Envelope>,
}

/// One channel of one track, with the gain it contributes to each output channel.
#[derive(Debug, Clone, PartialEq)]
pub struct Source {
    pub gain_left: f32,
    pub gain_right: f32,
    pub clips: Vec<Clip>,
}

/// The whole project as a stereo mixdown.
#[derive(Debug, Clone, PartialEq)]
pub struct Layout {
    pub sample_rate: u32,
    pub sources: Vec<Source>,
    /// Length of the mixdown, in frames.
    pub frames: u64,
    /// Empty timeline before the first clip, which the mixdown leaves out.
    pub lead_in_frames: u64,
    /// Audible tracks (a stereo pair counts once).
    pub tracks: usize,
}

impl Layout {
    pub fn duration_s(&self) -> f64 {
        self.frames as f64 / f64::from(self.sample_rate)
    }

    pub fn describe(&self) -> String {
        format!(
            "{} audible track{}, {} Hz, {:.1} min",
            self.tracks,
            if self.tracks == 1 { "" } else { "s" },
            self.sample_rate,
            self.duration_s() / 60.0
        )
    }
}

/// Stereo pan law Audacity uses: the far side is attenuated linearly.
fn pan_gains(pan: f64) -> (f64, f64) {
    let pan = pan.clamp(-1.0, 1.0);
    if pan < 0.0 {
        (1.0, 1.0 + pan)
    } else {
        (1.0 - pan, 1.0)
    }
}

/// Realtime effects are applied on playback and export; an active, non-empty list
/// changes the audio.
fn has_active_effects(owner: &Element) -> bool {
    owner.children_named("effects").any(|list| {
        list.flag("active").unwrap_or(true) && list.children_named("effect").next().is_some()
    })
}

struct StoredBlock {
    start: i64,
    len: i64,
    id: i64,
}

fn stored_blocks(sequence: &Element, num_samples: i64) -> Result<Vec<StoredBlock>, LayoutError> {
    let invalid = |what: &str| LayoutError::Invalid(what.to_owned());
    let mut starts = Vec::new();
    for block in sequence.children_named("waveblock") {
        let start = block
            .integer("start")
            .ok_or_else(|| invalid("a sample block has no start"))?;
        let id = block
            .integer("blockid")
            .ok_or_else(|| invalid("a sample block has no id"))?;
        starts.push((start, id));
    }
    starts.sort_by_key(|&(start, _)| start);

    let mut blocks = Vec::with_capacity(starts.len());
    for (i, &(start, id)) in starts.iter().enumerate() {
        let end = starts.get(i + 1).map_or(num_samples, |&(next, _)| next);
        blocks.push(StoredBlock {
            start,
            len: end - start,
            id,
        });
    }
    let contiguous = blocks.first().map_or(num_samples == 0, |b| b.start == 0)
        && blocks.iter().all(|b| b.len > 0);
    if !contiguous {
        return Err(invalid("a clip's sample blocks do not cover its length"));
    }
    Ok(blocks)
}

/// Where one `<waveclip>` plays, before the timeline is shifted to start at zero.
struct PlacedClip {
    start: i64,
    frames: i64,
    blocks: Vec<BlockRef>,
    envelope: Option<Envelope>,
}

/// The control points of a clip's volume envelope as (time, gain). Times count from
/// the start of the clip's stored audio, trimmed or not.
fn envelope_points(clip: &Element) -> Result<Vec<(f64, f64)>, LayoutError> {
    // Audacity keeps one envelope per clip; were there several, the last read wins.
    let Some(envelope) = clip.children_named("envelope").last() else {
        return Ok(Vec::new());
    };
    envelope
        .children_named("controlpoint")
        .map(|point| point.number("t").zip(point.number("val")))
        .collect::<Option<_>>()
        .ok_or_else(|| {
            LayoutError::Invalid("a volume envelope point has no time or gain".to_owned())
        })
}

fn place_clip(clip: &Element, rate: f64) -> Result<Option<PlacedClip>, LayoutError> {
    let invalid = |what: &str| LayoutError::Invalid(what.to_owned());
    if (clip.number("clipStretchRatio").unwrap_or(1.0) - 1.0).abs() > 1e-9
        || clip.integer("centShift").unwrap_or(0) != 0
    {
        return Err(LayoutError::Unsupported(
            "a stretched or pitch-shifted clip",
        ));
    }
    let sequence = clip
        .child("sequence")
        .ok_or_else(|| invalid("a clip has no audio sequence"))?;
    let num_samples = sequence
        .integer("numsamples")
        .filter(|n| *n >= 0)
        .ok_or_else(|| invalid("a clip has no sample count"))?;
    let blocks = stored_blocks(sequence, num_samples)?;

    // Same rounding as WaveClip::TimeToSamples / GetPlayStartSample.
    let to_samples = |seconds: f64| (seconds * rate + 0.5).floor() as i64;
    let offset = clip.number("offset").unwrap_or(0.0);
    let trim_left_s = clip.number("trimLeft").unwrap_or(0.0);
    let trim_left = to_samples(trim_left_s).clamp(0, num_samples);
    let trim_right = to_samples(clip.number("trimRight").unwrap_or(0.0)).clamp(0, num_samples);

    // The part of the stored audio that is audible: [first, last).
    let mut first = trim_left;
    let last = num_samples - trim_right;
    let mut start = ((offset + trim_left_s) * rate).round() as i64;
    if start < 0 {
        // Audio before time zero is never played.
        first -= start;
        start = 0;
    }
    if first >= last {
        return Ok(None);
    }

    // An envelope whose points are all at unity changes nothing; Audacity leaves one
    // behind on clips that have been stretched and rendered. Skipping it keeps such
    // clips a plain copy of their samples.
    let points = envelope_points(clip)?;
    let envelope = if Envelope::is_unity(&points) {
        None
    } else {
        // The envelope's clock starts at the clip's stored audio (`offset`), and
        // Audacity places each played frame on it by its position on the timeline.
        let origin_s = start as f64 / rate - offset;
        Some(Envelope::new(&points, origin_s, rate).map_err(|e| invalid(&e.to_string()))?)
    };

    let mut refs = Vec::new();
    for block in &blocks {
        let (lo, hi) = (block.start.max(first), (block.start + block.len).min(last));
        if lo >= hi {
            continue;
        }
        refs.push(BlockRef {
            id: (block.id > 0).then_some(block.id),
            skip: (lo - block.start) as usize,
            len: (hi - lo) as usize,
            at: (lo - first) as u64,
        });
    }
    Ok(Some(PlacedClip {
        start,
        frames: last - first,
        blocks: refs,
        envelope,
    }))
}

fn track_rate(track: &Element) -> Result<u32, LayoutError> {
    track
        .number("rate")
        .filter(|r| r.fract() == 0.0 && (1.0..=1_000_000.0).contains(r))
        .map(|r| r as u32)
        .ok_or_else(|| LayoutError::Invalid("a track has no usable sample rate".to_owned()))
}

/// Build the mixdown plan for a decoded project document.
pub fn build(project: &Element) -> Result<Layout, LayoutError> {
    if project.name != "project" {
        return Err(LayoutError::NotAProject(project.name.clone()));
    }
    if project.child("timetrack").is_some() {
        return Err(LayoutError::Unsupported("a time track"));
    }
    if has_active_effects(project) {
        return Err(LayoutError::Unsupported("master realtime effects"));
    }

    // A wave track flagged `linked` is the left channel of a stereo pair; the right
    // channel is the wave track that follows it.
    let wave_tracks: Vec<&Element> = project.children_named("wavetrack").collect();
    let mut groups: Vec<&[&Element]> = Vec::new();
    let mut i = 0;
    while i < wave_tracks.len() {
        let linked = wave_tracks[i].integer("linked").unwrap_or(0) != 0;
        let width = if linked && i + 1 < wave_tracks.len() {
            2
        } else {
            1
        };
        groups.push(&wave_tracks[i..i + width]);
        i += width;
    }

    // Mute, solo, volume and pan belong to the pair and are read from its leader.
    let solo = |g: &&[&Element]| g[0].flag("solo").unwrap_or(false);
    let any_solo = groups.iter().any(solo);
    let audible: Vec<&[&Element]> = groups
        .into_iter()
        .filter(|g| {
            if any_solo {
                solo(g)
            } else {
                !g[0].flag("mute").unwrap_or(false)
            }
        })
        .collect();

    let mut sample_rate = None;
    let mut placed: Vec<(f32, f32, Vec<PlacedClip>)> = Vec::new();
    for group in &audible {
        let leader = group[0];
        if has_active_effects(leader) {
            return Err(LayoutError::Unsupported("realtime effects on a track"));
        }
        let volume = leader.float("gain").unwrap_or(1.0);
        let (left, right) = pan_gains(leader.number("pan").unwrap_or(0.0));
        let (left, right) = ((left * volume) as f32, (right * volume) as f32);

        for (channel, track) in group.iter().enumerate() {
            let rate = track_rate(track)?;
            if *sample_rate.get_or_insert(rate) != rate {
                return Err(LayoutError::Unsupported(
                    "tracks with different sample rates",
                ));
            }
            let (gain_left, gain_right) = match (group.len(), channel) {
                (2, 0) => (left, 0.0),
                (2, _) => (0.0, right),
                _ => (left, right),
            };
            let mut clips = Vec::new();
            for clip in track.children_named("waveclip") {
                clips.extend(place_clip(clip, f64::from(rate))?);
            }
            placed.push((gain_left, gain_right, clips));
        }
    }

    let all_clips = || placed.iter().flat_map(|(_, _, clips)| clips);
    let (Some(first), Some(end)) = (
        all_clips().map(|c| c.start).min(),
        all_clips().map(|c| c.start + c.frames).max(),
    ) else {
        return Err(LayoutError::NoAudio);
    };
    let sample_rate = sample_rate.ok_or(LayoutError::NoAudio)?;

    let sources = placed
        .into_iter()
        .map(|(gain_left, gain_right, clips)| Source {
            gain_left,
            gain_right,
            clips: clips
                .into_iter()
                .map(|c| Clip {
                    start: (c.start - first) as u64,
                    frames: c.frames as u64,
                    blocks: c.blocks,
                    envelope: c.envelope,
                })
                .collect(),
        })
        .collect();
    Ok(Layout {
        sample_rate,
        sources,
        frames: (end - first) as u64,
        lead_in_frames: first as u64,
        tracks: audible.len(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aup::fixture::{project_document, write_tracks, ClipSpec, TrackSpec, XmlWriter};
    use crate::aup::xml::decode;

    const RATE: f64 = 1000.0;

    fn layout_of(tracks: &[TrackSpec]) -> Result<Layout, LayoutError> {
        let (dict, doc) = project_document(RATE, tracks);
        build(&decode(&dict, &doc).unwrap())
    }

    fn mono(blocks: Vec<(i64, i64)>) -> TrackSpec {
        TrackSpec::new(RATE, vec![ClipSpec::at_zero(blocks)])
    }

    fn stereo_pair() -> Vec<TrackSpec> {
        let mut left = mono(vec![(1, 300), (2, 200)]);
        left.linked = true;
        vec![left, mono(vec![(3, 300), (4, 200)])]
    }

    #[test]
    fn stereo_pair_maps_each_channel_to_its_side() {
        let layout = layout_of(&stereo_pair()).unwrap();
        assert_eq!(layout.sample_rate, 1000);
        assert_eq!(layout.frames, 500);
        assert_eq!((layout.tracks, layout.lead_in_frames), (1, 0));
        assert_eq!(layout.describe(), "1 audible track, 1000 Hz, 0.0 min");

        let [left, right] = &layout.sources[..] else {
            panic!("expected two sources")
        };
        assert_eq!((left.gain_left, left.gain_right), (1.0, 0.0));
        assert_eq!((right.gain_left, right.gain_right), (0.0, 1.0));
        assert_eq!(
            left.clips[0].blocks,
            [
                BlockRef {
                    id: Some(1),
                    skip: 0,
                    len: 300,
                    at: 0
                },
                BlockRef {
                    id: Some(2),
                    skip: 0,
                    len: 200,
                    at: 300
                },
            ]
        );
    }

    #[test]
    fn volume_is_the_floating_point_gain_not_the_spectrogram_setting() {
        let mut tracks = stereo_pair();
        tracks[0].volume = 0.5;
        let layout = layout_of(&tracks).unwrap();
        assert_eq!(layout.sources[0].gain_left, 0.5);
        assert_eq!(layout.sources[1].gain_right, 0.5); // the pair follows its leader
    }

    #[test]
    fn pan_attenuates_the_far_side_only() {
        assert_eq!(pan_gains(0.0), (1.0, 1.0));
        assert_eq!(pan_gains(-0.25), (1.0, 0.75));
        assert_eq!(pan_gains(1.0), (0.0, 1.0));

        let mut track = mono(vec![(1, 100)]);
        track.pan = 0.5;
        let layout = layout_of(&[track]).unwrap();
        let source = &layout.sources[0];
        assert_eq!((source.gain_left, source.gain_right), (0.5, 1.0));
    }

    #[test]
    fn muted_tracks_are_left_out_and_solo_overrides() {
        let mut muted = mono(vec![(1, 100)]);
        muted.mute = true;
        let layout = layout_of(&[muted, mono(vec![(2, 50)])]).unwrap();
        assert_eq!((layout.tracks, layout.frames), (1, 50));

        let mut soloed = mono(vec![(1, 100)]);
        soloed.solo = true;
        soloed.mute = true; // solo wins over mute, as in Audacity
        let layout = layout_of(&[soloed, mono(vec![(2, 50)])]).unwrap();
        assert_eq!((layout.tracks, layout.frames), (1, 100));
    }

    #[test]
    fn a_project_with_nothing_audible_is_an_error() {
        let mut muted = mono(vec![(1, 100)]);
        muted.mute = true;
        assert_eq!(layout_of(&[muted]), Err(LayoutError::NoAudio));
        assert_eq!(layout_of(&[]), Err(LayoutError::NoAudio));
        assert_eq!(layout_of(&[mono(vec![])]), Err(LayoutError::NoAudio));
    }

    #[test]
    fn trims_select_the_audible_part_of_the_stored_audio() {
        let clip = ClipSpec {
            offset: 2.0,
            trim_left: 0.35, // 350 samples: all of block 1 and 50 of block 2
            trim_right: 0.1, // 100 samples off the end of block 3
            blocks: vec![(1, 300), (2, 300), (3, 300)],
            envelope: vec![],
        };
        let layout = layout_of(&[TrackSpec::new(RATE, vec![clip])]).unwrap();
        // The clip starts at 2.35 s; that lead-in is dropped from the mixdown.
        assert_eq!(layout.lead_in_frames, 2350);
        assert_eq!(layout.frames, 450);
        assert_eq!(
            layout.sources[0].clips[0],
            Clip {
                start: 0,
                frames: 450,
                blocks: vec![
                    BlockRef {
                        id: Some(2),
                        skip: 50,
                        len: 250,
                        at: 0
                    },
                    BlockRef {
                        id: Some(3),
                        skip: 0,
                        len: 200,
                        at: 250
                    },
                ],
                envelope: None,
            }
        );
    }

    #[test]
    fn clips_keep_their_spacing_and_silent_blocks_have_no_id() {
        let first = ClipSpec::at_zero(vec![(1, 100)]);
        let second = ClipSpec {
            offset: 0.5,
            trim_left: 0.0,
            trim_right: 0.0,
            blocks: vec![(-40, 40), (2, 60)],
            envelope: vec![],
        };
        let layout = layout_of(&[TrackSpec::new(RATE, vec![first, second])]).unwrap();
        assert_eq!(layout.frames, 600);
        let clips = &layout.sources[0].clips;
        assert_eq!((clips[0].start, clips[1].start), (0, 500));
        assert_eq!(clips[1].blocks[0].id, None);
        assert_eq!(clips[1].blocks[1].id, Some(2));
    }

    #[test]
    fn audio_before_time_zero_is_not_played() {
        let clip = ClipSpec {
            offset: -0.25,
            trim_left: 0.0,
            trim_right: 0.0,
            blocks: vec![(1, 1000)],
            envelope: vec![],
        };
        let layout = layout_of(&[TrackSpec::new(RATE, vec![clip])]).unwrap();
        assert_eq!((layout.frames, layout.lead_in_frames), (750, 0));
        assert_eq!(layout.sources[0].clips[0].blocks[0].skip, 250);
    }

    #[test]
    fn mixed_sample_rates_are_refused() {
        let other = TrackSpec::new(44_100.0, vec![ClipSpec::at_zero(vec![(2, 100)])]);
        assert_eq!(
            layout_of(&[mono(vec![(1, 100)]), other]),
            Err(LayoutError::Unsupported(
                "tracks with different sample rates"
            ))
        );
    }

    /// A one-track project whose single clip carries extra attributes/children.
    fn with_clip_extras(extras: impl Fn(&mut XmlWriter)) -> Result<Layout, LayoutError> {
        let mut w = XmlWriter::new();
        w.start("project");
        w.start("wavetrack");
        w.double("rate", RATE);
        w.start("waveclip");
        extras(&mut w);
        w.start("sequence");
        w.long_long("numsamples", 100);
        w.start("waveblock");
        w.long_long("start", 0);
        w.long_long("blockid", 1);
        w.end("waveblock");
        w.end("sequence");
        w.end("waveclip");
        w.end("wavetrack");
        w.end("project");
        let (dict, doc) = w.finish();
        build(&decode(&dict, &doc).unwrap())
    }

    #[test]
    fn minimal_documents_use_audacity_defaults() {
        // No gain, pan, offset, trims or block lengths: an older, sparser project.
        let layout = with_clip_extras(|_| {}).unwrap();
        assert_eq!(layout.frames, 100);
        assert_eq!(layout.sources[0].gain_left, 1.0);
        assert_eq!(layout.sources[0].clips[0].blocks[0].len, 100);
    }

    #[test]
    fn audio_altering_clip_features_are_refused() {
        assert_eq!(
            with_clip_extras(|w| w.double("clipStretchRatio", 1.25)),
            Err(LayoutError::Unsupported(
                "a stretched or pitch-shifted clip"
            ))
        );
        assert_eq!(
            with_clip_extras(|w| w.int("centShift", 100)),
            Err(LayoutError::Unsupported(
                "a stretched or pitch-shifted clip"
            ))
        );
    }

    fn with_envelope(points: &'static [(f64, f64)]) -> Result<Layout, LayoutError> {
        with_clip_extras(|w| {
            w.start("envelope");
            w.size_t("numpoints", points.len() as u32);
            for (t, val) in points {
                w.start("controlpoint");
                w.double("t", *t);
                w.double("val", *val);
                w.end("controlpoint");
            }
            w.end("envelope");
        })
    }

    #[test]
    fn an_envelope_that_stays_at_unity_is_left_out() {
        for points in [&[][..], &[(0.0, 1.0)], &[(0.0, 1.0), (1.0, 1.0)]] {
            let layout = with_envelope(points).unwrap();
            assert_eq!(layout.sources[0].clips[0].envelope, None);
        }
    }

    #[test]
    fn a_volume_envelope_is_placed_on_the_clips_stored_audio() {
        // Stored audio starts at 2.0 s and its first 0.25 s is trimmed away, so the
        // clip's first frame is 0.25 s into the envelope: halfway down the fade.
        let clip = ClipSpec {
            offset: 2.0,
            trim_left: 0.25,
            trim_right: 0.0,
            blocks: vec![(1, 1000)],
            envelope: vec![(0.0, 1.0), (0.5, 0.01), (0.75, 0.01)],
        };
        let layout = layout_of(&[TrackSpec::new(RATE, vec![clip])]).unwrap();
        let envelope = layout.sources[0].clips[0].envelope.as_ref().unwrap();
        assert!((envelope.gain_at(0) - 0.1).abs() < 1e-12);
        assert!((envelope.gain_at(250) - 0.01).abs() < 1e-12);
        assert_eq!(envelope.gain_at(700), 0.01);
    }

    #[test]
    fn an_envelope_on_audio_cut_off_at_time_zero_keeps_its_place() {
        let clip = ClipSpec {
            offset: -0.25,
            trim_left: 0.0,
            trim_right: 0.0,
            blocks: vec![(1, 1000)],
            envelope: vec![(0.0, 1.0), (0.5, 0.01)],
        };
        let layout = layout_of(&[TrackSpec::new(RATE, vec![clip])]).unwrap();
        let envelope = layout.sources[0].clips[0].envelope.as_ref().unwrap();
        assert!((envelope.gain_at(0) - 0.1).abs() < 1e-12);
    }

    #[test]
    fn a_broken_envelope_is_reported() {
        for points in [&[(0.0, 0.0)][..], &[(1.0, 0.5), (0.0, 1.0)]] {
            let err = with_envelope(points).unwrap_err();
            assert!(matches!(err, LayoutError::Invalid(_)), "{err}");
        }
        let no_gain = with_clip_extras(|w| {
            w.start("envelope");
            w.start("controlpoint");
            w.double("t", 0.0);
            w.end("controlpoint");
            w.end("envelope");
        });
        assert!(matches!(no_gain, Err(LayoutError::Invalid(_))));
    }

    fn project_with(extra: impl Fn(&mut XmlWriter)) -> Result<Layout, LayoutError> {
        let mut w = XmlWriter::new();
        w.start("project");
        write_tracks(&mut w, &[mono(vec![(1, 100)])]);
        extra(&mut w);
        w.end("project");
        let (dict, doc) = w.finish();
        build(&decode(&dict, &doc).unwrap())
    }

    #[test]
    fn realtime_effects_and_time_tracks_are_refused() {
        let active_effect = |w: &mut XmlWriter| {
            w.start("effects");
            w.boolean("active", true);
            w.start("effect");
            w.text("id", "Compressor");
            w.end("effect");
            w.end("effects");
        };
        assert_eq!(
            project_with(active_effect),
            Err(LayoutError::Unsupported("master realtime effects"))
        );
        let bypassed = |w: &mut XmlWriter| {
            w.start("effects");
            w.boolean("active", false);
            w.start("effect");
            w.end("effect");
            w.end("effects");
        };
        assert!(project_with(bypassed).is_ok());
        let time_track = |w: &mut XmlWriter| {
            w.start("timetrack");
            w.end("timetrack");
        };
        assert_eq!(
            project_with(time_track),
            Err(LayoutError::Unsupported("a time track"))
        );
    }

    #[test]
    fn gaps_in_a_clip_are_reported() {
        let gap = with_clip_extras(|_| {});
        assert!(gap.is_ok());
        let mut w = XmlWriter::new();
        w.start("project");
        w.start("wavetrack");
        w.double("rate", RATE);
        w.start("waveclip");
        w.start("sequence");
        w.long_long("numsamples", 100);
        w.start("waveblock");
        w.long_long("start", 10); // nothing covers samples 0..10
        w.long_long("blockid", 1);
        w.end("waveblock");
        w.end("sequence");
        w.end("waveclip");
        w.end("wavetrack");
        w.end("project");
        let (dict, doc) = w.finish();
        let err = build(&decode(&dict, &doc).unwrap()).unwrap_err();
        assert!(matches!(err, LayoutError::Invalid(_)), "{err}");
    }

    #[test]
    fn other_documents_are_not_projects() {
        let mut w = XmlWriter::new();
        w.start("labels");
        w.end("labels");
        let (dict, doc) = w.finish();
        assert_eq!(
            build(&decode(&dict, &doc).unwrap()),
            Err(LayoutError::NotAProject("labels".to_owned()))
        );
    }
}
