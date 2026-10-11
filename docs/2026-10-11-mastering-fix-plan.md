# Mastering Quality Fix Plan

**Date:** 2026-10-11
**Status:** Complete 2026-10-11. All steps done; the owner chose to keep `club`
(−9 LUFS / −1 dBTP) as the default.

An audit of the mastering chain found that the default `club` preset does not
produce a good master. The architecture (constant gain, limiter, slow gated section
riding) is sound; the defects are in the default chain and in what the tool claims
versus what it measures. This plan records the findings and the order of work.

## Order of work

0. **Port the app to Rust** — done. Behaviour-faithful; deviations are listed in the
   design doc, Revision 6.
1. **Replace the clipper + sample-peak limiter** — done.
2. **Make the loudness target land** — done (measure, correct, re-render).
3. **Fix or remove the glue compressor** — done (removed).
4. **One tail for every path** — done.
5. **Preset targets for web upload** — decided: the owner kept `club` (−9 / −1) as
   the default, knowing the cost shown under "Outcome".
6. **Audio-level tests** — done.

## Outcome (measured 2026-10-11, same test set as the findings)

| Run | Before the fixes | After |
|---|---|---|
| `club` (−9 / −1) | −9.8 LUFS, tanh on everything, null −23 dB | −9.3 LUFS, −1.1 dBTP, limiter cost 0.3 LU, null −27 dB |
| `--target -11` | — | −11.0 LUFS, limiter cost 0.0 LU, null −61 dB |
| `streaming` | −14.0 LUFS, null −128 dB | unchanged (limiter left out) |
| `even-only` | null −127 dB | bit-identical to the source |
| `--leveling gentle` | −10.1 LUFS | −9.1 LUFS in 2 passes, limiter cost 0.2 LU |
| `--leveling strong --preserve-loud` | −11.8 LUFS (`strong`) | −9.1 LUFS, limiter cost 0.2 LU |
| `--bass-mono --subsonic` | +0.4 dBTP against a −1 ceiling | −1.1 dBTP; limiter cost 1.2 LU at −9 (warns), 0.1 LU at −11 |
| `--target -7` | −8.2 LUFS | −7.5 LUFS, limiter cost 3.4 LU (warns twice) |

An hour-long set renders in under a minute per pass.

Things learned that changed the plan:

- **A −2 dBTP ceiling at −9 LUFS is not viable on this material**: 1.5 LU of
  limiting and a null residual of −19 dB, as bad as the old clipper. The step 5
  proposal to give `club` that ceiling was dropped.
- **Limiter settings.** Release 50 / 100 / 250 / 500 ms traded distortion against
  pumping and lost loudness (block-wise null −32 / −33 / −36 / −40 dB, global −28 /
  −27 / −25 / −24 dB); 100 ms kept. With 60 Hz held 4.5 dB over the threshold it
  gave −47 dB bass THD and −45 dB intermodulation on a 7 kHz tone. Attack 5 vs
  10 ms and `asc` on vs off made no measurable difference.
- **Low-end filters raise peaks.** Phase rotation in the high-pass and the LR4 split
  un-does the peak control already in mastered tracks, so at loud targets they cost
  limiting. A linear-phase alternative would avoid that; not attempted.
- **Lossy transcodes.** Decoded true peak after AAC 160k / MP3 128k / Opus 64k:
  +1.1 … +1.6 dBTP for the −9 LUFS master, −0.2 … +0.8 for −11, −2.7 … −1.9 for −14.

**Step 5 decision:** offered −11, −14 or keeping −9 as the default, the owner kept
−9 LUFS / −1 dBTP. The tool reports the limiter cost on every run and warns above
0.7 LU, so a set that does not take −9 well says so.

## Findings (measured 2026-10-11)

Test material: a 4.7-minute pseudo-set made from four commercial hard-groove tracks
(75 s each, faders at −7 / −10.5 / −8 / −6 dB, 6 s crossfades), 44.1 kHz stereo.
Source measured I = −15.2 LUFS, TP = −5.1 dBTP, LRA = 4.5 LU. No real set recording
was available, and nothing was judged by ear — measurements only.

Method: EBU R128 via `ebur128=peak=true`; distortion via a null test (output minus
the gain-matched, time-aligned source, residual energy relative to output); THD and
intermodulation via synthetic tones through the exact filter strings.

### 1. The `club` soft clipper saturates the whole signal

`asoftclip=type=tanh:threshold=0.891` is `thr * tanh(x / thr)` — a waveshaper over
the full signal, not a peak clipper. The docstring claim that programme below the
threshold passes "~unchanged" is false.

| 1 kHz sine peak | level change | THD |
|---|---|---|
| −12 dBFS | −0.23 dB | 0.65 % |
| −6 dBFS | −0.85 dB | 2.44 % |
| −3 dBFS | −1.60 dB | 4.55 % |
| −1 dBFS | −2.37 dB | 6.71 % |

- Kick + hat intermodulation (60 Hz at −4 dBFS, 7 kHz at −16 dBFS): sidebands at
  −16 dB relative to the 7 kHz tone.
- No oversampling, so harmonics alias. (`asoftclip oversample=4` still loses ~6 dB
  on ffmpeg 9.0.2, so the existing note about that bug is accurate.)
- The clipper's maximum output is below the limiter ceiling, so the `alimiter` after
  it never engages in `club` mode.
- Null-test residual on the test set at equal loudness (−9.0 LUFS): −21.3 dB for the
  current chain versus −27.1 dB for a 4× oversampled limiter.

### 2. Loudness targets are missed

| Run | Target | Result |
|---|---|---|
| `club` | −9 | −9.8 |
| `club --target -7` | −7 | −8.2 |
| `--leveling gentle` | −9 | −10.1 |
| `--leveling strong` | −9 | −11.8 |

Gain is computed from the input measurement and never corrected for what the
clipper, limiter, ride or glue remove. The tool prints the miss but does not act on
it or fail.

### 3. The glue compressor is level-dependent

`acompressor=threshold=-18dB:ratio=2` sits before make-up gain with an absolute
threshold. It took 2.3 LU off the test set; the same set recorded 6 dB quieter lost
only 0.5 LU. The amount of "glue" depends on recording level, not on the music.

### 4. The true-peak ceiling is not enforced

`alimiter` is a sample-peak limiter. Limiter-only at club gain measured sample peak
−1.00 dBFS but true peak **+0.4 dBTP** against a −1 dBTP ceiling. This is also what
`--bass-mono` / `--subsonic` produce in the non-leveling path, because that path
builds its tail without the clipper.

After lossy encoding the `club` master decodes at +1.2 (AAC 160k), +0.8 (MP3 128k)
and +1.3 dBTP (Opus 64k).

### 5. Leveling "before" report shows the wrong true peak

`lavfi.r128.true_peak` metadata is linear, but was parsed as dB: a −5.1 dBTP file
reported +0.6 dBTP. Display only. The Python test fixture used an invented key
format, which hid it. **Fixed during the Rust port.**

### 6. Tests do not check audio

All 42 Python tests pass; they assert filter strings and parsing, never the rendered
result.

### What works

- `streaming` and `even-only` are bit-transparent while the limiter is idle (null
  residual −128 dB).
- Gentle leveling cut section spread from 4.5 to 2.0 LU with moves of about ±2 dB.
- The LR4 bass-mono split is built correctly.

## Fix design (as planned; see "Outcome" for what was built)

### Step 1 — limiter

Replace `asoftclip` + `alimiter` with a 4× oversampled limiter with delay
compensation, and leave a margin for the downsampling filter:

```
volume=<gain>dB,
aresample=<4×sr>:filter_size=64,
alimiter=limit=<ceiling − 0.2 dB>:level=disabled:attack=5:release=100:asc=1:latency=1,
aresample=<sr>:filter_size=64
```

Measured on the test set: −9.3 LUFS, −1.1 dBTP, null residual −27.1 dB. Release
60 / 100 / 250 ms gave −27.9 / −27.1 / −25.4 dB global residual; pick by ear.
`latency=1` is required — without it the limiter delay is a fractional sample at the
base rate. Verify true peak on the rendered output and fail if it exceeds the
ceiling. If a clipper is kept at all, make it opt-in and hard-knee (linear below the
threshold), inside the oversampled section.

### Step 2 — land the target

After rendering, measure integrated loudness; if it misses by more than ~0.3 LU,
adjust the pre-limiter gain by the error and render again (bounded to a few
iterations). Report the limiter's gain reduction so a squashed result is visible.

### Step 3 — glue

Either drop it, or reference its threshold to measured loudness (for example
programme loudness + N dB) so the amount is independent of recording level, and
include its loss in the step 2 correction.

### Step 4 — low-end tail

Build one tail per preset and reuse it in every path, so low-end flags only add the
high-pass and band split.

### Step 5 — targets for the web

SoundCloud normalises playback to about −14 LUFS and advises true peak below
−2 dBTP for masters louder than that (Spotify gives the same guidance). A −9 LUFS
master is turned down there, so the limiting buys nothing. No evidence was found
either way for Mixcloud. The test set's natural peak-to-loudness ratio was about
10 dB, so roughly −11 LUFS at −1 dBTP is reachable with no limiting at all.

Proposed: keep a loud preset for un-normalised platforms but with a −2 dBTP ceiling;
make the default something near −11 to −12 LUFS; keep `streaming` at −14 / −1.
Confirm the default with the owner before changing it.

### Step 6 — tests

Integration tests that render synthetic audio and assert: integrated loudness within
tolerance of target, true peak at or below the ceiling, bounded null-test residual,
and that a tone below the ceiling comes out undistorted.

## Sources

- SoundCloud Help, "Will SoundCloud play my track at the level it's mastered"
  (read via a search summary; the page returned 403 to direct fetch)
- Spotify for Artists, "Loudness normalization"
- FabFilter Pro-L 2 manual, "True peak limiting"
- FFmpeg `af_alimiter.c` source
- Voxengo OVC-128 release notes (clipper before limiter)
