# What the mastering is based on

This file explains why fadercut processes a set the way it does, and how strong the
evidence is for each choice. Three kinds of basis are kept apart:

- **Standards** — published measurement methods the tool implements or relies on.
- **Platform guidance** — what streaming services say they do to uploaded audio.
- **This project's measurements** — tests run on the tool itself. The full record is
  in [`2026-10-11-mastering-fix-plan.md`](2026-10-11-mastering-fix-plan.md).

Where a choice rests on convention or judgement rather than on any of these, it says
so. Nothing here was decided by a listening test; see [Limits](#limits).

## The chain in one paragraph

A DJ set is a recording of tracks that have each already been mastered. There is
little loudness left to gain and a lot of sound to lose, so the chain does as little
as possible: one constant gain for the whole set, a look-ahead limiter that is only
inserted if the gained peaks would cross the ceiling, and — only on request — a slow
fader ride between sections and two low-end filters. There is no compressor, clipper
or saturation stage.

## 1. Loudness is measured the broadcast-standard way

| What fadercut measures | Standard |
|---|---|
| Integrated loudness (LUFS), gated | ITU-R BS.1770 [1], EBU R 128 [2] |
| Momentary (400 ms) and short-term (3 s) loudness | EBU Tech 3341 [3] |
| Loudness range (LRA) | EBU Tech 3342 [4] |
| True peak (dBTP) | ITU-R BS.1770, Annex 2 [1] |

The measurements come from FFmpeg's `ebur128` filter [9], which implements these
documents. The tool uses them as follows:

- **Integrated loudness** sets the gain and decides whether a render landed on
  target (within 0.3 LU; the render is repeated with a corrected gain if not).
- **Short-term loudness** drives the section leveling and locates where the limiter
  worked hardest.
- **True peak** is checked on the finished file. A file over its ceiling is an
  error and is not written.

Streaming services measure with the same method (Spotify names ITU 1770 explicitly
[5]), so a number reported by fadercut is the number a platform will act on.

## 2. Why the ceiling is a true peak, and why −1 dBTP

**Peaks between samples are real.** The waveform a converter or codec reconstructs
can rise above the highest stored sample. Nielsen and Lund documented this for
commercial masters and measured how consumer players misbehave on it [8]; BS.1770
defines "true peak" as an estimate of that reconstructed peak, obtained by
oversampling [1].

**Lossy encoding raises peaks again.** Every platform re-encodes uploads, and the
decoded result does not have the same peaks as the master.

- EBU R 128 sets a maximum of −1 dBTP for production audio [2].
- Spotify and SoundCloud both ask for a true peak below −1 dBTP, and below −2 dBTP
  for masters louder than −14 LUFS, giving lossy transcoding as the reason [5][6].
- Apple recommends at least 1 dB of headroom and recommends checking the *encoded*
  file for clipping rather than the master [7].
- AES TD1008 recommends no more than −1 dBTP at the codec input [10] (taken from a
  secondary source; see the note in [Sources](#sources)).

**Measured here:** masters with a −1 dBTP ceiling, encoded to AAC 160k, MP3 128k and
Opus 64k and decoded again, peaked at +1.1 to +1.6 dBTP when mastered to −9 LUFS,
−0.2 to +0.8 dBTP at −11 LUFS, and −2.7 to −1.9 dBTP at −14 LUFS. The louder the
master, the more the codec overshoots, which is consistent with the platforms
asking for −2 dBTP on loud masters.

**What follows in the design**

- The default ceiling is −1 dBTP (`--tp` changes it).
- The limiter runs oversampled — 4× at 44.1/48 kHz, 2× at 88.2/96 kHz — so it acts
  on inter-sample peaks, not only on sample values. A limiter at the base rate
  measured −1.0 dBFS sample peak but **+0.4 dBTP**, i.e. it missed its own ceiling.
- The limiter threshold sits 0.2 dB under the ceiling to absorb the overshoot of
  the downsampling filter, and the result is verified by measuring the file.

A −2 dBTP ceiling at −9 LUFS was tried and rejected for this material: it cost
1.5 LU of limiting and distorted as much as the clipper it was meant to replace.

## 3. How loud: what the platforms do, and what the material gives

**Platforms that normalise turn loud masters down.** Spotify plays tracks at
−14 LUFS by default and lowers anything louder [5]. SoundCloud documents the same
behaviour with the same −14 LUFS figure [6]. A master pushed past that level is
simply played quieter there, so limiting applied to reach it costs quality and buys
nothing. AES guidance for streaming is lower still: TD1004 (2015) recommended a
window of −16 to −20 LUFS [10].

**Platforms that do not normalise play the file as it is.** There a louder master
does play louder. No documentation either way was found for Mixcloud.

**What a set gives before the limiter has to work.** On the test set the natural
gap between peak and loudness was about 10 dB, so roughly −11 LUFS is reachable at a
−1 dBTP ceiling with almost no limiting:

| Target (ceiling −1 dBTP) | Limiter cost | Residual vs. source¹ |
|---|---|---|
| −14 LUFS | limiter not inserted | none (pure gain) |
| −11 LUFS | 0.0 LU | −61 dB |
| −9 LUFS | 0.3 LU | −27 dB |
| −7 LUFS | 3.4 LU, and only reaches −7.5 | −15 dB |

¹ Null test, explained under [Method](#method).

The author's own recorded sets (96 kHz, already peaking at about 0 dBTP) were less
forgiving than the test set: −9 LUFS cost 1.4 to 1.7 LU of limiting, −11 LUFS cost
0.2 to 0.3 LU.

**What follows in the design**

- `streaming` (−14 LUFS) matches what normalising platforms play back and stays
  clean through their transcode.
- `club` (−9 LUFS) is the default by the author's choice: a loud file for
  platforms that play uploads as they are. On normalising platforms the evidence
  above favours `streaming`.
- Because the right target depends on the material, the tool reports what the
  limiter cost on every run, warns above 0.7 LU, and lists where to listen. The
  0.7 LU threshold is a rule of thumb from these measurements, not a standard.

## 4. The limiter: look-ahead peak limiting, nothing else

**No clipper, no waveshaper.** The original chain used a `tanh` soft clipper.
Measured, it was a saturator over the whole signal, not a peak clipper: 0.65 % THD
on a sine at −12 dBFS, 6.7 % at −1 dBFS, and intermodulation between kick and
hi-hat only 16 dB below the hi-hat. It was removed.

**No compressor.** The original "glue" compressor had an absolute threshold, so how
much it compressed depended on how hot the set was recorded (2.3 LU lost on the
test set, 0.5 LU on the same audio recorded 6 dB quieter). It was removed rather
than repaired: the tracks in a set are already compressed.

**Settings.** FFmpeg's `alimiter` [9] with 5 ms look-ahead/attack and 100 ms
release. The release was chosen from a sweep (50 / 100 / 250 / 500 ms), which
traded distortion against pumping and lost loudness; 100 ms was kept. With a
60 Hz tone held 4.5 dB over the threshold it gave −47 dB bass THD and −45 dB
intermodulation on a 7 kHz tone. Attack of 5 versus 10 ms made no measurable
difference. These happen to be the time constants Spotify publishes for its own
playback limiter [5]; that was not the reason for choosing them.

**Left out when idle.** If the gained peaks stay under the ceiling the limiter is
not in the chain at all, and the render is a pure gain change.

## 5. Section leveling: a slow fader ride, not dynamics processing

Tracks in a set differ in level because they were mixed in at different fader
positions, not because their dynamics need changing. `--leveling` therefore moves
one slow gain over the whole signal, the way an engineer rides a fader:

| Property | `gentle` | `strong` | Why |
|---|---|---|---|
| Share of the deviation corrected | 60 % | 85 % | Ease the spread, keep the energy arc |
| Largest move | ±3 dB | ±4 dB | Small moves only |
| Fastest change | 0.3 dB/s | 0.4 dB/s | Too slow to pump |
| Sections ignored | >10 LU below programme | >12 LU | Breakdowns are meant to be quiet |
| Loudness smoothed over | 15 s | 12 s | Follow sections, not bars |

The gate works like the relative gate in BS.1770 [1], which also sits 10 LU below
the programme level: material that far down is not part of what is being levelled.
The other numbers are design choices, not taken from a standard. Measured on the test set, `gentle`
reduced the spread between sections from 4.5 LU to 2.0 LU with moves of about
±2 dB.

## 6. Low end (both optional)

**`--bass-mono` (default 120 Hz).** The low band is summed to mono through a
fourth-order Linkwitz-Riley split — two cascaded second-order filters per band,
whose low and high outputs sum back to a flat response [11]. The reasons usually
given are that club subwoofers are commonly fed a mono sum, that out-of-phase bass
cancels when summed, and that vinyl cutting requires it. This is convention among
producers rather than a standard, the frequency quoted varies from 90 to 150 Hz,
and the rule is disputed [12].

**`--subsonic` (default 28 Hz).** A high-pass that removes inaudible rumble.
Conventional; no source claimed.

**Measured cost.** Both filters rotate phase, which re-shapes waveforms that were
already peak-controlled and raises their peaks by a dB or two. At loud targets that
is paid for in limiting: 1.2 LU at −9 LUFS on the test set, 0.1 LU at −11.

## 7. Reading Audacity projects

Not a mastering choice, but the same rule applies: reproduce the reference exactly
or refuse. The project reader follows Audacity's own source for how clips are
placed, trimmed, mixed and shaped by envelopes [13]. Its mixdown of a 63-minute,
three-track project with a drawn envelope was compared with Audacity 4.0.1's own
32-bit float export: bit-identical up to the envelope, and within −144.5 dBFS after
it.

## Method

- **Loudness and true peak:** FFmpeg `ebur128` with `peak=true`, on the rendered
  file.
- **Null test:** subtract the gain-matched, time-aligned source from the output and
  report the energy of what is left, relative to the output. It measures everything
  that is not "the source times a constant". Lower is cleaner.
- **Limiter cost:** integrated loudness entering the limiter minus that of the
  finished file.
- **THD and intermodulation:** synthetic tones through the exact filter chain.
- **Codec overshoot:** encode the master, decode it, measure true peak.
- **Test material:** a 4.7-minute set built from four commercial hard-groove tracks
  with crossfades (−15.2 LUFS, −5.1 dBTP, LRA 4.5 LU), plus the author's recorded
  sets.

The automated tests assert the same properties on synthetic audio: loudness on
target, true peak under the ceiling including peaks that fall between samples, no
distortion of a tone under the ceiling, and a null test against the source.

## Limits

- **No listening tests.** Every quality claim above is a measurement. A null
  residual or a loudness cost says how much changed, not how it sounds.
- **Little material.** One constructed test set and a handful of real sets, all
  hard groove and techno. The shape of the results should hold for any set of
  mastered tracks; the exact numbers will not.
- **Platform behaviour changes.** The guidance was read on 2026-10-11 and describes
  what the services publish, not an independent test of what they do.

## Sources

Read directly unless noted. Accessed 2026-10-11.

1. ITU-R BS.1770, *Algorithms to measure audio programme loudness and true-peak
   audio level.* <https://www.itu.int/rec/R-REC-BS.1770>
2. EBU R 128, *Loudness normalisation and permitted maximum level of audio
   signals.* <https://tech.ebu.ch/docs/r/r128.pdf>
3. EBU Tech 3341, *Loudness metering: "EBU Mode" metering to supplement EBU R 128.*
   <https://tech.ebu.ch/docs/tech/tech3341.pdf>
4. EBU Tech 3342, *Loudness range: a measure to supplement EBU R 128.*
   <https://tech.ebu.ch/docs/tech/tech3342.pdf>
5. Spotify for Artists, *Loudness normalization.*
   <https://support.spotify.com/artists/article/loudness-normalization/>
6. SoundCloud Help Center, *Will SoundCloud play my track at the level it's
   mastered?*
   <https://help.soundcloud.com/hc/en-us/articles/360053660014-Will-SoundCloud-play-my-track-at-the-level-it-s-mastered>
   — read through a search summary; the page refuses automated access.
7. Apple, *Apple Digital Masters: Music as the Artist and Sound Engineer Intended*
   (technology brief).
   <https://www.apple.com/apple-music/apple-digital-masters/docs/apple-digital-masters.pdf>
8. S. H. Nielsen and T. Lund, *0 dBFS+ Levels in Digital Mastering*, AES 109th
   Convention, 2000, paper 5251.
   <https://aes2.org/publications/elibrary-page/?id=9087> — abstract and secondary
   summaries only.
9. FFmpeg Filters Documentation, `ebur128`, `alimiter`, `aresample`.
   <https://ffmpeg.org/ffmpeg-filters.html>; limiter source:
   <https://github.com/FFmpeg/FFmpeg/blob/master/libavfilter/af_alimiter.c>
10. AES TD1004 (2015) and its successor TD1008 (2021), *Recommendations for
    Loudness of Internet Audio Streaming and On-Demand Distribution.* Neither
    document could be opened; the figures quoted are from Production Expert's
    report
    (<https://www.production-expert.com/production-expert-1/aes-update-loudness-recommendations-for-audio-only-streaming>)
    and other secondary sources, and should be checked against the originals.
11. Linkwitz–Riley filter (after S. Linkwitz, *Active Crossover Networks for
    Noncoincident Drivers*, JAES, 1976).
    <https://en.wikipedia.org/wiki/Linkwitz%E2%80%93Riley_filter>
12. Mono bass: producer guides and forum threads rather than formal sources, e.g.
    Warp Academy's argument against the blanket rule
    (<https://warpacademy.com/?p=274534>).
13. Audacity source code: `Envelope.cpp` and `MixerSource.cpp`
    (<https://github.com/audacity/audacity/tree/master/au3/libraries/au3-mixer>),
    `WaveClip.cpp`, `WaveTrack.cpp` and `Sequence.cpp`
    (<https://github.com/audacity/audacity/tree/master/au3/libraries/au3-wave-track>),
    `ProjectSerializer.cpp`
    (<https://github.com/audacity/audacity/tree/master/au3/libraries/au3-project-file-io>).
