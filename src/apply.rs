//! Stream-apply a time-varying gain envelope to audio via ffmpeg pipes.
//!
//! ```text
//! ffmpeg (decode -> f32le)  ->  multiply by interpolated envelope
//!                           ->  ffmpeg (post filter: glue/limiter -> 24-bit WAV)
//! ```
//!
//! Memory is bounded: audio is processed in fixed-size chunks; the only full-length
//! data held in memory is the low-rate (~10 Hz) gain envelope.

use std::io::{self, Read, Write};
use std::path::Path;
use std::process::{Child, Command, Stdio};

use crate::chain::Filter;
use crate::ffutil::{clip_args, describe_status, AudioFormat, Clip};
use crate::progress::Progress;

const CHUNK_FRAMES: usize = 1 << 16;
const BYTES_PER_SAMPLE: usize = 4; // f32le

#[derive(Debug, thiserror::Error)]
pub enum ApplyError {
    #[error("gain envelope is empty or its times and gains differ in length")]
    InvalidEnvelope,
    #[error("could not run ffmpeg: {0}")]
    Spawn(#[source] io::Error),
    #[error("decode ffmpeg failed ({0})")]
    Decode(String),
    #[error("encode ffmpeg failed ({0})")]
    Encode(String),
    #[error("audio pipe failed: {0}")]
    Pipe(#[source] io::Error),
}

/// A piecewise-linear gain curve sampled at increasing times.
#[derive(Debug, Clone, Copy)]
pub struct Envelope<'a> {
    times: &'a [f64],
    gains: &'a [f64],
}

impl<'a> Envelope<'a> {
    /// `times` (seconds, strictly increasing) and linear `gains` of equal length.
    pub fn new(times: &'a [f64], gains: &'a [f64]) -> Result<Self, ApplyError> {
        if times.is_empty() || times.len() != gains.len() {
            return Err(ApplyError::InvalidEnvelope);
        }
        Ok(Self { times, gains })
    }

    fn end_time(&self) -> f64 {
        self.times[self.times.len() - 1]
    }

    fn cursor(self) -> EnvelopeCursor<'a> {
        EnvelopeCursor {
            envelope: self,
            segment: 0,
        }
    }
}

/// Evaluates an envelope at non-decreasing times in amortised O(1).
struct EnvelopeCursor<'a> {
    envelope: Envelope<'a>,
    segment: usize,
}

impl EnvelopeCursor<'_> {
    /// Linear interpolation, held constant outside the envelope's time range.
    fn gain_at(&mut self, t: f64) -> f64 {
        let Envelope { times, gains } = self.envelope;
        let last = times.len() - 1;
        if t <= times[0] {
            return gains[0];
        }
        if t >= times[last] {
            return gains[last];
        }
        while times[self.segment + 1] <= t {
            self.segment += 1;
        }
        let i = self.segment;
        let slope = (gains[i + 1] - gains[i]) / (times[i + 1] - times[i]);
        slope * (t - times[i]) + gains[i]
    }
}

/// Multiply interleaved f32le frames by the envelope, appending to `out`.
///
/// `first_frame` is the absolute index of the first frame in `input`.
fn apply_gain(
    input: &[u8],
    out: &mut Vec<u8>,
    channels: usize,
    first_frame: u64,
    sample_rate: u32,
    cursor: &mut EnvelopeCursor,
) {
    let frame_bytes = channels * BYTES_PER_SAMPLE;
    for (k, frame) in input.chunks_exact(frame_bytes).enumerate() {
        let t = (first_frame + k as u64) as f64 / f64::from(sample_rate);
        let gain = cursor.gain_at(t) as f32;
        for sample in frame.chunks_exact(BYTES_PER_SAMPLE) {
            let x = f32::from_le_bytes([sample[0], sample[1], sample[2], sample[3]]);
            out.extend_from_slice(&(x * gain).to_le_bytes());
        }
    }
}

/// Read until `buf` is full or the stream ends; returns the number of bytes read.
fn read_full(reader: &mut impl Read, buf: &mut [u8]) -> io::Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        match reader.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(filled)
}

enum PumpError {
    Read(io::Error),
    Write(io::Error),
}

/// Copy decoded audio to the encoder, applying the envelope. Closes `sink` on return.
fn pump(
    mut source: impl Read,
    mut sink: impl Write,
    envelope: Envelope,
    format: AudioFormat,
    mut progress: Option<Progress>,
) -> Result<(), PumpError> {
    let channels = usize::from(format.channels);
    let frame_bytes = channels * BYTES_PER_SAMPLE;
    let mut cursor = envelope.cursor();
    let mut input = vec![0u8; CHUNK_FRAMES * frame_bytes];
    let mut output = Vec::with_capacity(input.len());
    let mut pos: u64 = 0;
    loop {
        let filled = read_full(&mut source, &mut input).map_err(PumpError::Read)?;
        // A trailing partial frame can only come from a truncated stream; drop it.
        let whole = filled - filled % frame_bytes;
        if whole == 0 {
            break;
        }
        output.clear();
        apply_gain(
            &input[..whole],
            &mut output,
            channels,
            pos,
            format.sample_rate,
            &mut cursor,
        );
        sink.write_all(&output).map_err(PumpError::Write)?;
        pos += (whole / frame_bytes) as u64;
        if let Some(p) = progress.as_mut() {
            p.update(pos);
        }
        if filled < input.len() {
            break;
        }
    }
    sink.flush().map_err(PumpError::Write)?;
    if let Some(p) = progress.as_ref() {
        p.finish();
    }
    Ok(())
}

fn reap(mut child: Child) -> Result<std::process::ExitStatus, ApplyError> {
    child.wait().map_err(ApplyError::Pipe)
}

/// Decode, multiply by the interpolated linear-gain envelope, re-encode.
///
/// `post_filter` is applied after the gain (glue compression, make-up gain and the
/// limiter). When `show_progress` is set, a live progress line goes to stderr.
#[allow(clippy::too_many_arguments)]
pub fn apply_envelope(
    ffmpeg: &Path,
    input: &Path,
    output: &Path,
    envelope: Envelope,
    format: AudioFormat,
    post_filter: &Filter,
    clip: Option<Clip>,
    show_progress: bool,
) -> Result<(), ApplyError> {
    let rate = format.sample_rate.to_string();
    let channels = format.channels.to_string();

    let mut dec = Command::new(ffmpeg)
        .args(["-hide_banner", "-loglevel", "error"])
        .args(clip_args(clip))
        .arg("-i")
        .arg(input)
        .args(["-f", "f32le", "-ac", &channels, "-ar", &rate, "-"])
        // ffmpeg reads interactive commands from stdin; never let it eat ours.
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .spawn()
        .map_err(ApplyError::Spawn)?;
    let spawned_enc = Command::new(ffmpeg)
        .args(["-hide_banner", "-loglevel", "error", "-y"])
        .args(["-f", "f32le", "-ar", &rate, "-ac", &channels, "-i", "-"])
        .args(post_filter.ffmpeg_args())
        .args(["-c:a", "pcm_s24le"])
        .arg(output)
        .stdin(Stdio::piped())
        .spawn();
    let mut enc = match spawned_enc {
        Ok(enc) => enc,
        Err(e) => {
            let _ = dec.kill();
            let _ = dec.wait();
            return Err(ApplyError::Spawn(e));
        }
    };

    let source = dec.stdout.take().expect("decoder stdout is piped");
    let sink = enc.stdin.take().expect("encoder stdin is piped");
    let total_frames = (envelope.end_time() * f64::from(format.sample_rate)) as u64;
    let progress =
        show_progress.then(|| Progress::new("applying gain", total_frames, format.sample_rate));

    // `pump` owns both pipe ends, so the encoder sees EOF as soon as it returns.
    let pumped = pump(source, sink, envelope, format, progress);
    if pumped.is_err() {
        // Stop the decoder rather than let it run on with nowhere to write.
        let _ = dec.kill();
    }
    let dec_status = reap(dec)?;
    let enc_status = reap(enc)?;

    match pumped {
        // A failed write means the encoder went away; its exit status says why.
        Err(PumpError::Write(_)) if !enc_status.success() => {
            Err(ApplyError::Encode(describe_status(enc_status)))
        }
        Err(PumpError::Write(e)) | Err(PumpError::Read(e)) => Err(ApplyError::Pipe(e)),
        Ok(()) if !dec_status.success() => Err(ApplyError::Decode(describe_status(dec_status))),
        Ok(()) if !enc_status.success() => Err(ApplyError::Encode(describe_status(enc_status))),
        Ok(()) => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const STEREO_48K: AudioFormat = AudioFormat {
        sample_rate: 48000,
        channels: 2,
    };

    fn frames_to_bytes(samples: &[f32]) -> Vec<u8> {
        samples.iter().flat_map(|s| s.to_le_bytes()).collect()
    }

    fn bytes_to_samples(bytes: &[u8]) -> Vec<f32> {
        bytes
            .chunks_exact(4)
            .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
            .collect()
    }

    #[test]
    fn envelope_rejects_empty_or_mismatched() {
        assert!(Envelope::new(&[], &[]).is_err());
        assert!(Envelope::new(&[0.0, 1.0], &[1.0]).is_err());
    }

    #[test]
    fn cursor_interpolates_and_holds_at_the_ends() {
        let (times, gains) = ([1.0, 2.0, 4.0], [1.0, 2.0, 0.0]);
        let mut cursor = Envelope::new(&times, &gains).unwrap().cursor();
        assert_eq!(cursor.gain_at(0.0), 1.0); // before the first point
        assert_eq!(cursor.gain_at(1.0), 1.0);
        assert_eq!(cursor.gain_at(1.5), 1.5);
        assert_eq!(cursor.gain_at(2.0), 2.0);
        assert_eq!(cursor.gain_at(3.0), 1.0);
        assert_eq!(cursor.gain_at(4.0), 0.0);
        assert_eq!(cursor.gain_at(9.0), 0.0); // after the last point
    }

    #[test]
    fn apply_gain_scales_every_channel_of_a_frame() {
        // Gain ramps 1.0 -> 2.0 over one second at 4 Hz "sample rate".
        let (times, gains) = ([0.0, 1.0], [1.0, 2.0]);
        let mut cursor = Envelope::new(&times, &gains).unwrap().cursor();
        let input = frames_to_bytes(&[1.0, -1.0, 1.0, -1.0, 1.0, -1.0]);
        let mut out = Vec::new();
        apply_gain(&input, &mut out, 2, 0, 4, &mut cursor);
        assert_eq!(bytes_to_samples(&out), [1.0, -1.0, 1.25, -1.25, 1.5, -1.5]);
    }

    #[test]
    fn apply_gain_uses_absolute_frame_position() {
        let (times, gains) = ([0.0, 1.0], [1.0, 2.0]);
        let mut cursor = Envelope::new(&times, &gains).unwrap().cursor();
        let mut out = Vec::new();
        apply_gain(&frames_to_bytes(&[1.0]), &mut out, 1, 2, 4, &mut cursor);
        assert_eq!(bytes_to_samples(&out), [1.5]);
    }

    #[test]
    fn pump_is_chunk_boundary_independent() {
        // More than one chunk, with a ragged final chunk and a trailing partial frame.
        let frames = CHUNK_FRAMES + 1000;
        let samples: Vec<f32> = (0..frames * 2).map(|i| (i % 7) as f32 - 3.0).collect();
        let mut input = frames_to_bytes(&samples);
        input.extend_from_slice(&[1, 2, 3]); // truncated frame
        let (times, gains) = ([0.0, frames as f64 / 48000.0], [1.0, 0.5]);
        let envelope = Envelope::new(&times, &gains).unwrap();

        let mut piped = Vec::new();
        assert!(pump(input.as_slice(), &mut piped, envelope, STEREO_48K, None).is_ok());

        let mut direct = Vec::new();
        let whole = &input[..frames * 2 * 4];
        apply_gain(whole, &mut direct, 2, 0, 48000, &mut envelope.cursor());
        assert_eq!(piped.len(), frames * 2 * 4);
        assert_eq!(piped, direct);
    }

    #[test]
    fn pump_reports_write_failures() {
        struct Broken;
        impl Write for Broken {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                Err(io::ErrorKind::BrokenPipe.into())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let input = frames_to_bytes(&[0.5, 0.5]);
        let (times, gains) = ([0.0], [1.0]);
        let envelope = Envelope::new(&times, &gains).unwrap();
        let result = pump(input.as_slice(), Broken, envelope, STEREO_48K, None);
        assert!(matches!(result, Err(PumpError::Write(_))));
    }
}
