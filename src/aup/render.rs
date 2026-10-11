//! Mix a project layout down to interleaved stereo, chunk by chunk.
//!
//! Memory is bounded: at most one decoded sample block is held per clip, however
//! long the project is. Where the blocks come from is injected.

use super::layout::{Clip, Layout};

/// Supplies the decoded samples of a stored block.
pub trait BlockSource {
    type Error;

    /// The block's samples as floats in [-1, 1] (full scale), in stored order.
    fn load(&mut self, id: i64) -> Result<Vec<f32>, Self::Error>;
}

#[derive(Debug, PartialEq, thiserror::Error)]
pub enum RenderError<E> {
    #[error("{0}")]
    Source(E),
    #[error("sample block {id} holds {actual} samples, but the project uses {needed} of it")]
    ShortBlock {
        id: i64,
        needed: usize,
        actual: usize,
    },
}

/// Read position within one clip: its current block, decoded.
#[derive(Default)]
struct ClipCursor {
    block: usize,
    loaded: Option<(usize, Vec<f32>)>,
}

pub struct Renderer<'a, B> {
    layout: &'a Layout,
    blocks: B,
    pos: u64,
    end: u64,
    /// One cursor per clip, in source-then-clip order.
    cursors: Vec<ClipCursor>,
    /// Samples of the run being mixed, after its clip's volume envelope.
    shaped: Vec<f32>,
}

impl<'a, B: BlockSource> Renderer<'a, B> {
    /// Render frames `[start, end)` of the mixdown (clamped to its length).
    pub fn new(layout: &'a Layout, blocks: B, start: u64, end: u64) -> Self {
        let clips = layout.sources.iter().map(|s| s.clips.len()).sum();
        let end = end.min(layout.frames);
        Self {
            layout,
            blocks,
            pos: start.min(end),
            end,
            cursors: std::iter::repeat_with(ClipCursor::default)
                .take(clips)
                .collect(),
            shaped: Vec::new(),
        }
    }

    pub fn frames_left(&self) -> u64 {
        self.end - self.pos
    }

    /// Fill `out` with up to `max_frames` interleaved stereo frames and return how
    /// many were produced; zero means the mixdown is complete.
    pub fn render(
        &mut self,
        max_frames: usize,
        out: &mut Vec<f32>,
    ) -> Result<usize, RenderError<B::Error>> {
        let frames = self.frames_left().min(max_frames as u64) as usize;
        out.clear();
        out.resize(frames * 2, 0.0);
        let (from, to) = (self.pos, self.pos + frames as u64);

        let mut cursors = self.cursors.iter_mut();
        for source in &self.layout.sources {
            for clip in &source.clips {
                let cursor = cursors.next().expect("one cursor per clip");
                mix_clip(
                    clip,
                    (source.gain_left, source.gain_right),
                    (from, to),
                    cursor,
                    &mut self.blocks,
                    &mut self.shaped,
                    out,
                )?;
            }
        }
        self.pos = to;
        Ok(frames)
    }
}

/// Add the part of `clip` that falls inside the window `[from, to)` to `out`.
fn mix_clip<B: BlockSource>(
    clip: &Clip,
    (gain_left, gain_right): (f32, f32),
    (from, to): (u64, u64),
    cursor: &mut ClipCursor,
    blocks: &mut B,
    shaped: &mut Vec<f32>,
    out: &mut [f32],
) -> Result<(), RenderError<B::Error>> {
    let (begin, end) = (from.max(clip.start), to.min(clip.start + clip.frames));
    if begin >= end {
        return Ok(());
    }
    // The same window in clip-relative frames.
    let (begin, end) = (begin - clip.start, end - clip.start);

    let ends_before = |index: usize| {
        clip.blocks
            .get(index)
            .is_some_and(|b| b.at + b.len as u64 <= begin)
    };
    while ends_before(cursor.block) {
        cursor.block += 1;
    }
    let mut index = cursor.block;
    while let Some(block) = clip.blocks.get(index).filter(|b| b.at < end) {
        if let Some(id) = block.id {
            if !matches!(&cursor.loaded, Some((loaded, _)) if *loaded == index) {
                let samples = blocks.load(id).map_err(RenderError::Source)?;
                if samples.len() < block.skip + block.len {
                    return Err(RenderError::ShortBlock {
                        id,
                        needed: block.skip + block.len,
                        actual: samples.len(),
                    });
                }
                cursor.loaded = Some((index, samples));
            }
            let (_, samples) = cursor.loaded.as_ref().expect("block was just loaded");

            let lo = begin.max(block.at);
            let hi = end.min(block.at + block.len as u64);
            let stored = block.skip + (lo - block.at) as usize;
            let run = &samples[stored..stored + (hi - lo) as usize];
            // As in Audacity's mixer, the envelope shapes the samples before the
            // track's volume and pan are applied.
            let run = match &clip.envelope {
                Some(envelope) => {
                    envelope.apply(lo, run, shaped);
                    shaped.as_slice()
                }
                None => run,
            };
            let first_frame = (clip.start + lo - from) as usize;
            let frames = out[first_frame * 2..].chunks_exact_mut(2);
            for (frame, &sample) in frames.zip(run) {
                frame[0] += sample * gain_left;
                frame[1] += sample * gain_right;
            }
        }
        index += 1;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aup::envelope::Envelope;
    use crate::aup::layout::{BlockRef, Source};
    use std::collections::HashMap;

    /// Blocks held in memory, counting how often each is loaded.
    #[derive(Default)]
    struct Memory {
        blocks: HashMap<i64, Vec<f32>>,
        loads: Vec<i64>,
    }

    impl BlockSource for &mut Memory {
        type Error = String;

        fn load(&mut self, id: i64) -> Result<Vec<f32>, String> {
            self.loads.push(id);
            self.blocks
                .get(&id)
                .cloned()
                .ok_or_else(|| format!("no block {id}"))
        }
    }

    fn block(id: i64, skip: usize, len: usize, at: u64) -> BlockRef {
        BlockRef {
            id: (id > 0).then_some(id),
            skip,
            len,
            at,
        }
    }

    fn clip(start: u64, blocks: Vec<BlockRef>) -> Clip {
        Clip {
            start,
            frames: blocks.iter().map(|b| b.len as u64).sum(),
            blocks,
            envelope: None,
        }
    }

    fn layout(sources: Vec<Source>) -> Layout {
        let frames = sources
            .iter()
            .flat_map(|s| &s.clips)
            .map(|c| c.start + c.frames)
            .max()
            .unwrap_or(0);
        Layout {
            sample_rate: 1000,
            sources,
            frames,
            lead_in_frames: 0,
            tracks: 1,
        }
    }

    fn ramp(from: i32, len: usize) -> Vec<f32> {
        (0..len).map(|i| (from + i as i32) as f32).collect()
    }

    /// Render everything in chunks of `chunk` frames.
    fn render_all(layout: &Layout, memory: &mut Memory, chunk: usize) -> Vec<f32> {
        let mut renderer = Renderer::new(layout, memory, 0, u64::MAX);
        let (mut all, mut buf) = (Vec::new(), Vec::new());
        while renderer.render(chunk, &mut buf).unwrap() > 0 {
            all.extend_from_slice(&buf);
        }
        all
    }

    fn stereo_layout() -> (Layout, Memory) {
        let mut memory = Memory::default();
        memory.blocks.insert(1, ramp(0, 5));
        memory.blocks.insert(2, ramp(5, 5));
        memory.blocks.insert(3, ramp(100, 10));
        let left = Source {
            gain_left: 1.0,
            gain_right: 0.0,
            clips: vec![clip(0, vec![block(1, 0, 5, 0), block(2, 0, 5, 5)])],
        };
        let right = Source {
            gain_left: 0.0,
            gain_right: 1.0,
            clips: vec![clip(0, vec![block(3, 0, 10, 0)])],
        };
        (layout(vec![left, right]), memory)
    }

    #[test]
    fn interleaves_a_stereo_pair() {
        let (layout, mut memory) = stereo_layout();
        let out = render_all(&layout, &mut memory, 4096);
        let expected: Vec<f32> = (0..10).flat_map(|i| [i as f32, 100.0 + i as f32]).collect();
        assert_eq!(out, expected);
    }

    #[test]
    fn output_does_not_depend_on_chunk_size() {
        let (layout, mut memory) = stereo_layout();
        let whole = render_all(&layout, &mut memory, 4096);
        for chunk in [1, 3, 5, 7] {
            assert_eq!(
                render_all(&layout, &mut memory, chunk),
                whole,
                "chunk {chunk}"
            );
        }
    }

    #[test]
    fn each_block_is_loaded_once_when_read_in_order() {
        let (layout, mut memory) = stereo_layout();
        render_all(&layout, &mut memory, 3);
        memory.loads.sort_unstable();
        assert_eq!(memory.loads, [1, 2, 3]);
    }

    #[test]
    fn clips_are_placed_with_silence_between_them_and_trims_applied() {
        let mut memory = Memory::default();
        memory.blocks.insert(1, ramp(1, 4));
        memory.blocks.insert(2, ramp(10, 6));
        let source = Source {
            gain_left: 1.0,
            gain_right: 1.0,
            clips: vec![
                clip(0, vec![block(1, 0, 4, 0)]),
                // Uses samples 2..5 of block 2, then a silent block.
                clip(6, vec![block(2, 2, 3, 0), block(-2, 0, 2, 3)]),
            ],
        };
        let out = render_all(&layout(vec![source]), &mut memory, 5);
        let left: Vec<f32> = out.chunks_exact(2).map(|f| f[0]).collect();
        assert_eq!(
            left,
            [1.0, 2.0, 3.0, 4.0, 0.0, 0.0, 12.0, 13.0, 14.0, 0.0, 0.0]
        );
    }

    #[test]
    fn overlapping_tracks_are_summed_with_their_gains() {
        let mut memory = Memory::default();
        memory.blocks.insert(1, vec![1.0; 4]);
        memory.blocks.insert(2, vec![1.0; 2]);
        let centre = Source {
            gain_left: 0.5,
            gain_right: 0.5,
            clips: vec![clip(0, vec![block(1, 0, 4, 0)])],
        };
        let panned = Source {
            gain_left: 1.0,
            gain_right: 0.25,
            clips: vec![clip(1, vec![block(2, 0, 2, 0)])],
        };
        let out = render_all(&layout(vec![centre, panned]), &mut memory, 4096);
        assert_eq!(out, [0.5, 0.5, 1.5, 0.75, 1.5, 0.75, 0.5, 0.5]);
    }

    #[test]
    fn a_clips_envelope_shapes_it_before_the_track_gain() {
        let mut memory = Memory::default();
        memory.blocks.insert(1, vec![1.0; 6]);
        memory.blocks.insert(2, vec![1.0; 6]);
        // Frames 2..8 of the stored audio play, so the clip's first frame is 2 ms
        // into an envelope that steps from 1 to 0.5 at 5 ms.
        let mut fading = clip(0, vec![block(1, 2, 4, 0), block(2, 0, 2, 4)]);
        let points = [(0.0, 1.0), (0.005, 1.0), (0.005, 0.5), (1.0, 0.5)];
        fading.envelope = Some(Envelope::new(&points, 0.002, 1000.0).unwrap());
        let source = Source {
            gain_left: 1.0,
            gain_right: 0.5,
            clips: vec![fading, clip(8, vec![block(1, 0, 2, 0)])],
        };
        let layout = layout(vec![source]);
        let expected = [1.0, 1.0, 1.0, 0.5, 0.5, 0.5, 0.0, 0.0, 1.0, 1.0];
        for chunk in [4096, 1, 3] {
            let out = render_all(&layout, &mut memory, chunk);
            let left: Vec<f32> = out.chunks_exact(2).map(|f| f[0]).collect();
            let right: Vec<f32> = out.chunks_exact(2).map(|f| f[1]).collect();
            assert_eq!(left, expected, "chunk {chunk}");
            assert_eq!(right, expected.map(|v| v * 0.5), "chunk {chunk}");
        }
    }

    #[test]
    fn a_range_renders_the_same_frames_as_the_whole() {
        let (layout, mut memory) = stereo_layout();
        let whole = render_all(&layout, &mut memory, 4096);
        let mut renderer = Renderer::new(&layout, &mut memory, 3, 8);
        assert_eq!(renderer.frames_left(), 5);
        let mut buf = Vec::new();
        assert_eq!(renderer.render(4096, &mut buf).unwrap(), 5);
        assert_eq!(buf, whole[6..16]);
        assert_eq!(renderer.render(4096, &mut buf).unwrap(), 0);
    }

    #[test]
    fn a_range_past_the_end_is_empty() {
        let (layout, mut memory) = stereo_layout();
        let mut renderer = Renderer::new(&layout, &mut memory, 50, 60);
        assert_eq!(renderer.render(16, &mut Vec::new()).unwrap(), 0);
    }

    #[test]
    fn missing_and_short_blocks_are_errors() {
        let (layout, mut memory) = stereo_layout();
        memory.blocks.insert(2, ramp(5, 3)); // two samples short
        let mut renderer = Renderer::new(&layout, &mut memory, 0, u64::MAX);
        assert_eq!(
            renderer.render(4096, &mut Vec::new()),
            Err(RenderError::ShortBlock {
                id: 2,
                needed: 5,
                actual: 3
            })
        );

        let (layout, mut memory) = stereo_layout();
        memory.blocks.remove(&3);
        let mut renderer = Renderer::new(&layout, &mut memory, 0, u64::MAX);
        assert_eq!(
            renderer.render(4096, &mut Vec::new()),
            Err(RenderError::Source("no block 3".to_owned()))
        );
    }
}
