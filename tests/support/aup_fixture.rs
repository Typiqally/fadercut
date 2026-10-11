//! Test support: build Audacity project files without Audacity.
//!
//! Writes the same binary document format and SQLite schema that Audacity does, so
//! tests can exercise the reader against known audio. Shared by the unit tests (via
//! `#[path]`) and the end-to-end tests; not part of the shipped crate.
#![allow(dead_code)]

use std::collections::HashMap;
use std::path::Path;

pub const INT16: i64 = 0x0002_0001;
pub const INT24: i64 = 0x0004_0001;
pub const FLOAT: i64 = 0x0004_000F;

/// SQLite `application_id` Audacity stamps on its projects ("AUDY").
pub const APPLICATION_ID: i64 = 0x4155_4459;

/// Encoder for Audacity's binary project document (the inverse of `aup::xml`).
pub struct XmlWriter {
    char_size: u8,
    dict: Vec<u8>,
    doc: Vec<u8>,
    ids: HashMap<String, u16>,
}

impl XmlWriter {
    pub fn new() -> Self {
        Self::with_char_size(4)
    }

    pub fn with_char_size(char_size: u8) -> Self {
        Self {
            char_size,
            dict: vec![0, char_size],
            doc: Vec::new(),
            ids: HashMap::new(),
        }
    }

    fn encode(&self, text: &str) -> Vec<u8> {
        match self.char_size {
            1 => text.as_bytes().to_vec(),
            2 => text.encode_utf16().flat_map(u16::to_le_bytes).collect(),
            _ => text
                .chars()
                .flat_map(|c| (c as u32).to_le_bytes())
                .collect(),
        }
    }

    fn id(&mut self, name: &str) -> [u8; 2] {
        if let Some(id) = self.ids.get(name) {
            return id.to_le_bytes();
        }
        let id = self.ids.len() as u16;
        self.ids.insert(name.to_owned(), id);
        let encoded = self.encode(name);
        self.dict.push(15);
        self.dict.extend(id.to_le_bytes());
        self.dict.extend((encoded.len() as u16).to_le_bytes());
        self.dict.extend(encoded);
        id.to_le_bytes()
    }

    fn field(&mut self, kind: u8, name: &str, payload: &[u8]) {
        let id = self.id(name);
        self.doc.push(kind);
        self.doc.extend(id);
        self.doc.extend(payload);
    }

    pub fn start(&mut self, tag: &str) {
        self.field(1, tag, &[]);
    }

    pub fn end(&mut self, tag: &str) {
        self.field(2, tag, &[]);
    }

    pub fn text(&mut self, name: &str, value: &str) {
        let encoded = self.encode(value);
        let mut payload = (encoded.len() as i32).to_le_bytes().to_vec();
        payload.extend(encoded);
        self.field(3, name, &payload);
    }

    pub fn int(&mut self, name: &str, value: i32) {
        self.field(4, name, &value.to_le_bytes());
    }

    pub fn boolean(&mut self, name: &str, value: bool) {
        self.field(5, name, &[u8::from(value)]);
    }

    pub fn long(&mut self, name: &str, value: i32) {
        self.field(6, name, &value.to_le_bytes());
    }

    pub fn long_long(&mut self, name: &str, value: i64) {
        self.field(7, name, &value.to_le_bytes());
    }

    pub fn size_t(&mut self, name: &str, value: u32) {
        self.field(8, name, &value.to_le_bytes());
    }

    pub fn float(&mut self, name: &str, value: f32) {
        let mut payload = value.to_le_bytes().to_vec();
        payload.extend(7i32.to_le_bytes());
        self.field(9, name, &payload);
    }

    pub fn double(&mut self, name: &str, value: f64) {
        let mut payload = value.to_le_bytes().to_vec();
        payload.extend(19i32.to_le_bytes());
        self.field(10, name, &payload);
    }

    pub fn blob(&mut self, name: &str, bytes: &[u8]) {
        let mut payload = (bytes.len() as i32).to_le_bytes().to_vec();
        payload.extend(bytes);
        self.field(16, name, &payload);
    }

    /// (dictionary, document) as stored in the `project` table.
    pub fn finish(self) -> (Vec<u8>, Vec<u8>) {
        (self.dict, self.doc)
    }
}

/// One row of the `sampleblocks` table.
pub struct Block {
    pub id: i64,
    pub format: i64,
    pub samples: Vec<u8>,
}

impl Block {
    /// 24-bit samples, stored as Audacity does: one 32-bit integer each.
    pub fn int24(id: i64, samples: &[i32]) -> Self {
        Self {
            id,
            format: INT24,
            samples: samples.iter().flat_map(|s| s.to_le_bytes()).collect(),
        }
    }

    pub fn int16(id: i64, samples: &[i16]) -> Self {
        Self {
            id,
            format: INT16,
            samples: samples.iter().flat_map(|s| s.to_le_bytes()).collect(),
        }
    }

    pub fn float(id: i64, samples: &[f32]) -> Self {
        Self {
            id,
            format: FLOAT,
            samples: samples.iter().flat_map(|s| s.to_le_bytes()).collect(),
        }
    }
}

/// Write a project database the way Audacity leaves it after a clean close.
pub fn write_project(path: &Path, dict: &[u8], doc: &[u8], blocks: &[Block]) {
    let conn = rusqlite::Connection::open(path).unwrap();
    conn.execute_batch(&format!(
        "PRAGMA application_id = {APPLICATION_ID};
         CREATE TABLE project(id INTEGER PRIMARY KEY, dict BLOB, doc BLOB);
         CREATE TABLE autosave(id INTEGER PRIMARY KEY, dict BLOB, doc BLOB);
         CREATE TABLE sampleblocks(
             blockid INTEGER PRIMARY KEY AUTOINCREMENT, sampleformat INTEGER,
             summin REAL, summax REAL, sumrms REAL,
             summary256 BLOB, summary64k BLOB, samples BLOB);"
    ))
    .unwrap();
    conn.execute(
        "INSERT INTO project(id, dict, doc) VALUES (1, ?1, ?2)",
        rusqlite::params![dict, doc],
    )
    .unwrap();
    for block in blocks {
        conn.execute(
            "INSERT INTO sampleblocks(blockid, sampleformat, samples) VALUES (?1, ?2, ?3)",
            rusqlite::params![block.id, block.format, block.samples],
        )
        .unwrap();
    }
}

/// Attributes of one clip in a [`TrackSpec`].
pub struct ClipSpec {
    pub offset: f64,
    pub trim_left: f64,
    pub trim_right: f64,
    /// (block id, sample count). A non-positive id is a silent block.
    pub blocks: Vec<(i64, i64)>,
    /// Volume envelope points as (seconds from the stored audio's start, gain).
    pub envelope: Vec<(f64, f64)>,
}

impl ClipSpec {
    pub fn at_zero(blocks: Vec<(i64, i64)>) -> Self {
        Self {
            offset: 0.0,
            trim_left: 0.0,
            trim_right: 0.0,
            blocks,
            envelope: Vec::new(),
        }
    }
}

/// Attributes of one `<wavetrack>` (one channel).
pub struct TrackSpec {
    /// Starts a stereo pair with the track that follows.
    pub linked: bool,
    pub mute: bool,
    pub solo: bool,
    pub rate: f64,
    pub volume: f64,
    pub pan: f64,
    pub clips: Vec<ClipSpec>,
}

impl TrackSpec {
    pub fn new(rate: f64, clips: Vec<ClipSpec>) -> Self {
        Self {
            linked: false,
            mute: false,
            solo: false,
            rate,
            volume: 1.0,
            pan: 0.0,
            clips,
        }
    }
}

/// Write `<wavetrack>` elements the way Audacity 4 does, including the attributes
/// that only exist to trip up a careless reader.
pub fn write_tracks(w: &mut XmlWriter, tracks: &[TrackSpec]) {
    for track in tracks {
        w.start("wavetrack");
        w.text("name", "Audio 1");
        w.int("gain", 20); // spectrogram setting that shares the volume's name
        w.int("channel", if track.linked { 0 } else { 1 });
        w.int("linked", if track.linked { 3 } else { 0 });
        w.boolean("mute", track.mute);
        w.boolean("solo", track.solo);
        w.double("rate", track.rate);
        w.double("gain", track.volume);
        w.double("pan", track.pan);
        w.long("sampleformat", INT24 as i32);
        w.start("effects");
        w.boolean("active", true);
        w.end("effects");
        for clip in &track.clips {
            w.start("waveclip");
            w.double("offset", clip.offset);
            w.double("trimLeft", clip.trim_left);
            w.double("trimRight", clip.trim_right);
            w.int("centShift", 0);
            w.double("clipStretchRatio", 1.0);
            w.start("sequence");
            w.size_t("maxsamples", 262_144);
            w.size_t("sampleformat", INT24 as u32);
            let total: i64 = clip.blocks.iter().map(|(_, len)| len).sum();
            w.long_long("numsamples", total);
            let mut start = 0;
            for (id, len) in &clip.blocks {
                w.start("waveblock");
                w.long_long("start", start);
                w.long_long("length", *len);
                w.long_long("blockid", *id);
                w.end("waveblock");
                start += len;
            }
            w.end("sequence");
            w.start("envelope");
            w.size_t("numpoints", clip.envelope.len() as u32);
            for (t, val) in &clip.envelope {
                w.start("controlpoint");
                w.double("t", *t);
                w.double("val", *val);
                w.end("controlpoint");
            }
            w.end("envelope");
            w.end("waveclip");
        }
        w.end("wavetrack");
    }
}

/// A complete project document around the given tracks.
pub fn project_document(rate: f64, tracks: &[TrackSpec]) -> (Vec<u8>, Vec<u8>) {
    let mut w = XmlWriter::new();
    w.start("project");
    w.text("audacityversion", "4.0.1");
    w.double("rate", rate);
    w.start("thumbnail");
    w.blob("data", &[0xFF; 64]);
    w.end("thumbnail");
    w.start("tags");
    w.end("tags");
    write_tracks(&mut w, tracks);
    w.start("effects");
    w.boolean("active", true);
    w.end("effects");
    w.end("project");
    w.finish()
}

/// A one-clip stereo project holding the given 24-bit channels, split into blocks.
pub fn stereo_project(path: &Path, rate: u32, left: &[i32], right: &[i32], block_len: usize) {
    assert_eq!(left.len(), right.len());
    let mut blocks = Vec::new();
    let mut refs: [Vec<(i64, i64)>; 2] = [Vec::new(), Vec::new()];
    let mut id = 500; // Audacity's ids rarely start at 1
    for (channel, samples) in [left, right].into_iter().enumerate() {
        for chunk in samples.chunks(block_len) {
            id += 1;
            blocks.push(Block::int24(id, chunk));
            refs[channel].push((id, chunk.len() as i64));
        }
    }
    let [left_refs, right_refs] = refs;
    let mut left_track = TrackSpec::new(f64::from(rate), vec![ClipSpec::at_zero(left_refs)]);
    left_track.linked = true;
    let right_track = TrackSpec::new(f64::from(rate), vec![ClipSpec::at_zero(right_refs)]);
    let (dict, doc) = project_document(f64::from(rate), &[left_track, right_track]);
    write_project(path, &dict, &doc, &blocks);
}
