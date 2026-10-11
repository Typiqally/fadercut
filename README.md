# fadercut

Reusable CLI to master a DJ set for upload: set the overall level, hold a true-peak
ceiling, and optionally even out section-to-section loudness. Takes an audio file or
an Audacity project directly. Written in Rust; wraps `ffmpeg` (`ebur128` for
measurement, `alimiter` for limiting).

## Why

Simple peak-normalization fails on DJ sets because a single reverb spike pins the
ceiling while the rest of the set stays quiet. fadercut raises the whole set by one
constant gain and, only if the peaks would then exceed the ceiling, limits them —
without touching the music's dynamics otherwise.

A DJ set is made of tracks that are already mastered, so there is little loudness
left to gain. The tool is built around that: it measures what the limiter cost and
tells you when a target is asking too much of the material.

## How a master is made

```
[subsonic high-pass] -> [bass-mono] -> constant gain -> [limiter] -> 24-bit WAV
     --subsonic         --bass-mono    (sets loudness)  (only if peaks need it)
```

1. **Measure** the source (EBU R128 integrated loudness and true peak).
2. **Render** with the gain that would land the target. The limiter is left out
   entirely when the gained peaks stay under the ceiling, so the render is a pure
   gain change (`even-only` on material already under the ceiling is bit-identical
   to the source).
3. **Measure the rendered file.** If loudness is more than 0.3 LU off target, or the
   true peak is over the ceiling, correct and render again (up to 4 passes).
4. **Write the output only once the ceiling holds.** Renders go to a
   `<name>.partial-<pid>.wav` beside the output and are moved into place at the end;
   a failed run leaves nothing behind.

The limiter is ffmpeg's look-ahead `alimiter` run at 4× the sample rate (2× at
88.2/96 kHz), so it limits inter-sample peaks rather than only sample values. There
is no clipper, no compressor and no waveshaping anywhere in the chain.

### The report

```
  before : I=-15.2 LUFS  TP=-5.1 dBTP  LRA=4.5 LU
  after  : I=-9.3 LUFS  TP=-1.1 dBTP  LRA=4.2 LU
  target : I=-9.0 LUFS  TP<=-1.0 dBTP  (1 pass)
  limiter: took 0.3 LU off the loudness
```

`limiter:` is the loudness difference between the signal entering the limiter and
the finished file — how hard it worked across the whole set, not just on one spike.
Above 0.7 LU the tool warns about heavy limiting; that is the point to lower
`--target`. It also warns if the target could not be reached.

The heavy-limiting warning always comes with where to listen, so you can judge it
yourself:

```
warning: heavy limiting — the limiter took 1.7 LU off the loudness, which usually pumps audibly. Consider a lower --target.
         Where to listen — the stretches the limiter worked on hardest:
           6:23 - 6:53         2.1 LU
           38:59 - 39:29       2.4 LU
         Least affected, to compare against: 0:45 - 1:15 (0.0 LU)
         Times are positions in the output file.
```

Up to five 30-second stretches are listed, at least three minutes apart, ranked by
how much loudness the limiter removed there. Breakdowns and silence are skipped.
To hear one without re-rendering the set, pass it to `--preview`, which accepts
`mm:ss` and `h:mm:ss`.

## Leveling (`--leveling gentle|strong`)

Mimics an engineer riding the fader by section before the gain/limiter stage:

- **Perceptual** — driven by EBU R128 short-term loudness, not raw peaks.
- **Gated** — sections >10 LU below the program loudness (breakdowns, drops, quiet
  intros) are *frozen*, never boosted or ducked.
- **Intro/drop-aware** — onsets emerging from silence, and the opening build, are
  left at their natural level; only sustained track-level differences get corrected.
- **Partial** — `gentle` corrects ~60% of the deviation (±3 dB at most), `strong`
  ~85% (±4 dB), easing the spread without flattening the energy arc.
- **Asymmetric (`--preserve-loud`)** — lifts quiet sections but leaves loud ones
  (drops, climax, the set's build) almost untouched.
- **Slew-limited** — a slow rate of change, which structurally prevents pumping and
  any fast loud->quiet move.

Whatever the ride does to overall loudness is corrected by the measure-and-render
loop above. Output is 24-bit WAV at the source sample rate; the original is never
modified.

## Audacity projects

Point fadercut at the project instead of exporting from Audacity first:

```bash
fadercut "~/Documents/Audacity4/Projects/friday.aup4"    # -> friday-mastered.wav
```

Reads Audacity 4 (`.aup4`) projects, verified against real ones. Audacity 3
(`.aup3`) projects use the same format and are accepted, but have not been tried on a
real file. The result is one audio file: what you would hear playing the project
from its first clip to its last.

- **All audible tracks are mixed**, honouring mute, solo, volume and pan. A stereo
  track stays stereo; a mono track goes to both sides.
- **Cuts, moves and trims are followed.** Each clip is placed where it sits on the
  timeline, with silence in any gap. Empty timeline before the first clip is left
  out. Audio hidden by trimming a clip's edges stays hidden.
- **Clip volume envelopes are applied** — the fades and level changes drawn on a
  clip — with the same curve Audacity uses between points (a straight line in
  decibels).
- **The audio is taken at the tracks' own sample rate and bit depth** — nothing is
  resampled or dithered on the way in.
- **The project is never modified.** It is opened read-only in SQLite's immutable
  mode: no locks are taken and no journal files are created beside it.
- **Close the project in Audacity first.** While it is open, part of its audio lives
  in a side file that cannot be read safely, so fadercut refuses it and says so.
  Unsaved changes are not seen; the last saved state is used (with a warning).

The mixdown goes to a temporary `<output>.source-<pid>.wav` beside the output and is
deleted afterwards. It is 32-bit float, about 46 MB per minute at 96 kHz. With
`--preview` only the requested range is mixed down.

Refused, rather than rendered differently from Audacity: stretched or pitch-shifted
clips, realtime effects, a time track, and tracks at different sample rates. For those — and for legacy Audacity 2 `.aup` projects — export from
Audacity as WAV (32-bit float) and master that file.

## Requirements

- `ffmpeg` + `ffprobe` — `brew install ffmpeg`
- Rust 1.79+ to build — `cargo build --release` produces `target/release/fadercut`
  (or `cargo install --path .` to put `fadercut` on your PATH)

## Usage

```bash
fadercut "set.wav"                              # club preset
fadercut "set.aup4"                             # straight from an Audacity project
fadercut "set.wav" --preset streaming           # -14 LUFS for normalising platforms
fadercut "set.wav" --preset even-only --leveling gentle  # even sections, keep loudness
fadercut "set.wav" --leveling gentle --preserve-loud     # lift quiet, keep loud/build
fadercut "set.wav" --bass-mono --subsonic      # mono <120Hz, HPF 28Hz
fadercut "set.wav" --preview 1:30-2:00 ...     # audition a 30s slice in ~1s
fadercut "set.wav" --target -11                # choose your own loudness
fadercut "set.wav" --dry-run                   # measure + print the first pass
fadercut "set.wav" --output /tmp/out.wav --force
```

Exit codes: `0` success, `1` processing failed, `2` bad arguments or a failed
preflight check. Nothing is written in either failure case.

### Options

| Flag        | Meaning |
|-------------|---------|
| `--preset`   | `club` (default, -9 LUFS), `streaming` (-14), `even-only` (keep loudness) |
| `--leveling` | `off` (default), `gentle`, `strong` — section-to-section loudness riding |
| `--preserve-loud` | with `--leveling`: lift quiet sections but keep loud ones (build) |
| `--bass-mono [HZ]` | fold lows below HZ (default 120) to mono |
| `--subsonic [HZ]` | high-pass below HZ (default 28) to remove inaudible rumble |
| `--preview START-END` | process only a time range to audition fast (e.g. `0:30-1:30`, `1:02:50-1:03:20`) |
| `--target`   | Override integrated loudness (LUFS) |
| `--tp`       | Override the true-peak ceiling (dBTP, -20 to 0), default -1 |
| `--output`   | Output path (default `<input>-mastered.wav`, or `-preview.wav`) |
| `--force`   | Overwrite existing output |
| `--dry-run` | Measure only; print the first-pass plan and ffmpeg command |

## Presets

| Preset      | Integrated | True Peak | Use |
|-------------|-----------:|----------:|-----|
| `club`      | -9 LUFS    | -1 dBTP   | Loud master for platforms that do not normalise (default) |
| `streaming` | -14 LUFS   | -1 dBTP   | SoundCloud/Spotify/YouTube reference level |
| `even-only` | measured   | -1 dBTP   | Keep current loudness; only bring peaks under the ceiling |

## Choosing a target

Measured on a 4.7-minute test set of four hard-groove tracks (source −15.2 LUFS,
−5.1 dBTP). These are measurements, not listening tests, and one set is not every
set — but the shape will hold for any set built from already-mastered tracks.

| Target / ceiling | Limiter cost | Difference from source¹ | Peak after AAC/MP3/Opus² |
|---|---|---|---|
| −14 LUFS / −1 dBTP | not needed | none (pure gain) | −2.7 … −1.9 dBTP |
| −11 LUFS / −1 dBTP | 0.0 LU | −61 dB | −0.2 … +0.8 dBTP |
| −9 LUFS / −1 dBTP | 0.3 LU | −27 dB | +1.1 … +1.6 dBTP |
| −9 LUFS / −2 dBTP | 1.5 LU (warns) | −19 dB | — |
| −7 LUFS / −1 dBTP | 3.4 LU (warns, lands at −7.5) | −15 dB | — |

¹ Null test: energy of everything that is not "the source times a constant",
relative to the output. Lower is cleaner.
² True peak of the master after encoding to AAC 160k, MP3 128k and Opus 64k and
decoding again. Above 0 dBTP a decoder with fixed-point output clips.

- **SoundCloud** normalises playback to about −14 LUFS, so a louder master is simply
  turned down there; `--preset streaming` gives up nothing and stays clean through
  the transcode.
- On platforms that play files at their own level, louder does play louder. Around
  −11 LUFS is the most this material gives before the limiter starts to work.
- `--bass-mono` and `--subsonic` rotate phase, which raises peaks by a dB or two on
  mastered material; at loud targets that is paid for in limiting (1.2 LU at −9 LUFS
  on the test set, 0.1 LU at −11).

## Development

```bash
cargo test                      # unit tests + end-to-end tests
cargo clippy --all-targets
cargo fmt --check
```

Unit tests cover the pure logic (filter construction, ebur128 parsing, the gain
ride, envelope application, the measure-and-correct loop, and decoding and mixing
Audacity projects). The end-to-end tests in
`tests/cli.rs` run the real binary on synthetic audio and check the rendered files:
loudness on target, true peak under the ceiling (including peaks that fall between
samples), no distortion of a tone under the ceiling, and a null test against the
source. Project input is checked against fixtures written in Audacity's on-disk
format (`tests/support/aup_fixture.rs`): a project must master sample-for-sample
like the same audio as a WAV, and the project file must be left byte-identical.
They are skipped when `ffmpeg` is not installed.

Design history: `docs/2026-06-15-design.md`. The audit that led to the
current chain: `docs/2026-10-11-mastering-fix-plan.md`.

## Licence

MIT — see `LICENSE`.
