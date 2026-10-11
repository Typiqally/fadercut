//! Read-only access to Audacity projects (`.aup3`, `.aup4`).
//!
//! A project is an SQLite database: a `project` table holding the serialized
//! project document, and a `sampleblocks` table holding the audio in blocks. This
//! module opens one without ever writing to it, works out how its tracks and clips
//! mix down ([`layout`]), and renders that mixdown ([`render`]).

pub mod envelope;
pub mod layout;
pub mod render;
pub mod xml;

#[cfg(test)]
#[path = "../../tests/support/aup_fixture.rs"]
pub(crate) mod fixture;

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use rusqlite::{Connection, OpenFlags, OptionalExtension};

use crate::ffutil::{describe_status, Clip};
use crate::progress::Progress;
use layout::{Layout, LayoutError};
use render::{BlockSource, RenderError, Renderer};
use xml::XmlError;

/// SQLite `application_id` of an Audacity project ("AUDY").
const APPLICATION_ID: i64 = 0x4155_4459;

// Audacity's `sampleFormat` codes: bytes per stored sample in the high word.
const FORMAT_INT16: i64 = 0x0002_0001;
const FORMAT_INT24: i64 = 0x0004_0001;
const FORMAT_FLOAT: i64 = 0x0004_000F;

const CHUNK_FRAMES: usize = 1 << 15;

#[derive(Debug, thiserror::Error)]
pub enum AupError {
    #[error(
        "this is a legacy Audacity 2 project (.aup), which is not supported. Open it in \
         Audacity and export the audio as WAV (32-bit float)."
    )]
    LegacyFormat,
    #[error(
        "the project is open in Audacity (or was not closed cleanly). Save and close it \
         there first, so that all of its audio is in the project file."
    )]
    InUse,
    #[error("not an Audacity project: {0}")]
    NotAProject(String),
    #[error("could not read the Audacity project: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("could not read the Audacity project: {0}")]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Document(#[from] XmlError),
    #[error(transparent)]
    Layout(#[from] LayoutError),
    #[error("sample block {0} is missing from the project")]
    MissingBlock(i64),
    #[error("sample block {id} has an unknown sample format ({format:#x})")]
    UnknownFormat { id: i64, format: i64 },
    #[error("sample block {id} holds {actual} samples, but the project uses {needed} of it")]
    ShortBlock {
        id: i64,
        needed: usize,
        actual: usize,
    },
    #[error("the requested range starts after the end of the project ({0:.1} s long)")]
    EmptyRange(f64),
    #[error("could not run ffmpeg: {0}")]
    Spawn(#[source] std::io::Error),
    #[error("writing the project's audio failed ({0})")]
    Encode(String),
    #[error("writing the project's audio failed: {0}")]
    Pipe(#[source] std::io::Error),
}

impl From<RenderError<AupError>> for AupError {
    fn from(err: RenderError<AupError>) -> Self {
        match err {
            RenderError::Source(source) => source,
            RenderError::ShortBlock { id, needed, actual } => {
                AupError::ShortBlock { id, needed, actual }
            }
        }
    }
}

fn extension(path: &Path) -> String {
    path.extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default()
}

/// Whether `path` names an Audacity project (by extension), of any generation.
pub fn is_project(path: &Path) -> bool {
    matches!(extension(path).as_str(), "aup" | "aup3" | "aup4")
}

/// `project.aup4` -> `project.aup4-wal`.
fn sidecar(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(suffix);
    PathBuf::from(name)
}

/// Percent-encode a path for use in an SQLite `file:` URI.
fn uri_path(path: &Path) -> String {
    #[cfg(unix)]
    let bytes = std::os::unix::ffi::OsStrExt::as_bytes(path.as_os_str()).to_vec();
    #[cfg(not(unix))]
    let bytes = path.to_string_lossy().into_owned().into_bytes();

    let mut out = String::with_capacity(bytes.len());
    for byte in bytes {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' => {
                out.push(char::from(byte));
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

/// Convert a stored sample block to floats at full scale, as Audacity does.
fn decode_samples(format: i64, bytes: &[u8]) -> Option<Vec<f32>> {
    match format {
        FORMAT_INT16 => Some(
            bytes
                .chunks_exact(2)
                .map(|b| f32::from(i16::from_le_bytes([b[0], b[1]])) / 32_768.0)
                .collect(),
        ),
        // 24-bit samples are stored one per 32-bit integer.
        FORMAT_INT24 => Some(
            bytes
                .chunks_exact(4)
                .map(|b| i32::from_le_bytes([b[0], b[1], b[2], b[3]]) as f32 / 8_388_608.0)
                .collect(),
        ),
        FORMAT_FLOAT => Some(
            bytes
                .chunks_exact(4)
                .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
                .collect(),
        ),
        _ => None,
    }
}

struct SqliteBlocks<'c> {
    conn: &'c Connection,
}

impl BlockSource for SqliteBlocks<'_> {
    type Error = AupError;

    fn load(&mut self, id: i64) -> Result<Vec<f32>, AupError> {
        let mut stmt = self
            .conn
            .prepare_cached("SELECT sampleformat, samples FROM sampleblocks WHERE blockid = ?1")?;
        let mut rows = stmt.query([id])?;
        let row = rows.next()?.ok_or(AupError::MissingBlock(id))?;
        let format: i64 = row.get(0)?;
        let bytes = row
            .get_ref(1)?
            .as_blob()
            .map_err(|_| AupError::MissingBlock(id))?;
        decode_samples(format, bytes).ok_or(AupError::UnknownFormat { id, format })
    }
}

/// An Audacity project opened for reading.
pub struct Project {
    conn: Connection,
    layout: Layout,
    unsaved_changes: bool,
}

impl Project {
    /// Open a project without modifying it.
    ///
    /// The database is opened read-only and *immutable*: SQLite takes no locks and
    /// creates no journal, `-wal` or `-shm` files beside it. The price is that a
    /// project Audacity still has open cannot be read reliably, so that is refused.
    pub fn open(path: &Path) -> Result<Self, AupError> {
        if extension(path) == "aup" {
            return Err(AupError::LegacyFormat);
        }
        if ["-wal", "-shm"].iter().any(|s| sidecar(path, s).exists()) {
            return Err(AupError::InUse);
        }
        let uri = format!(
            "file:{}?mode=ro&immutable=1",
            uri_path(&std::path::absolute(path)?)
        );
        let flags = OpenFlags::SQLITE_OPEN_READ_ONLY
            | OpenFlags::SQLITE_OPEN_URI
            | OpenFlags::SQLITE_OPEN_NO_MUTEX;
        let conn = Connection::open_with_flags(uri, flags)?;

        let application_id: i64 = conn.query_row("PRAGMA application_id", [], |r| r.get(0))?;
        if application_id != APPLICATION_ID {
            return Err(AupError::NotAProject(
                "the file is a database, but not one Audacity wrote".to_owned(),
            ));
        }
        let document: Option<(Vec<u8>, Vec<u8>)> = conn
            .query_row("SELECT dict, doc FROM project WHERE id = 1", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .optional()?;
        let (dict, doc) =
            document.ok_or_else(|| AupError::NotAProject("it has never been saved".to_owned()))?;
        let unsaved_changes =
            conn.query_row("SELECT EXISTS(SELECT 1 FROM autosave)", [], |r| r.get(0))?;

        let layout = layout::build(&xml::decode(&dict, &doc)?)?;
        Ok(Self {
            conn,
            layout,
            unsaved_changes,
        })
    }

    pub fn layout(&self) -> &Layout {
        &self.layout
    }

    /// The project holds changes that were never saved; only the saved state is read.
    pub fn has_unsaved_changes(&self) -> bool {
        self.unsaved_changes
    }

    /// A renderer over frames `[start, end)` of the project's mixdown.
    pub fn renderer(
        &self,
        start: u64,
        end: u64,
    ) -> Renderer<'_, impl BlockSource<Error = AupError> + '_> {
        Renderer::new(&self.layout, SqliteBlocks { conn: &self.conn }, start, end)
    }

    /// Mix the project (or a time range of it) down to a 32-bit float stereo WAV.
    pub fn export_wav(
        &self,
        ffmpeg: &Path,
        dest: &Path,
        range: Option<Clip>,
        show_progress: bool,
    ) -> Result<(), AupError> {
        let rate = f64::from(self.layout.sample_rate);
        let (start, end) = match range {
            None => (0, self.layout.frames),
            Some(clip) => {
                let start = (clip.start * rate).round() as u64;
                (start, start + (clip.duration * rate).round() as u64)
            }
        };
        let mut renderer = self.renderer(start, end);
        let total = renderer.frames_left();
        if total == 0 {
            return Err(AupError::EmptyRange(self.layout.duration_s()));
        }

        // ffmpeg writes the container, switching to RF64 if the audio passes 4 GB.
        let mut encoder = Command::new(ffmpeg)
            .args(["-hide_banner", "-loglevel", "error", "-y"])
            .args(["-f", "f32le", "-ac", "2", "-ar"])
            .arg(self.layout.sample_rate.to_string())
            .args(["-i", "-", "-c:a", "pcm_f32le", "-rf64", "auto"])
            .arg(dest)
            .stdin(Stdio::piped())
            .spawn()
            .map_err(AupError::Spawn)?;
        let mut sink = encoder.stdin.take().expect("encoder stdin is piped");

        let mut progress =
            show_progress.then(|| Progress::new("reading project", total, self.layout.sample_rate));
        let mut pump = || -> Result<(), AupError> {
            let (mut frames, mut bytes) = (Vec::new(), Vec::new());
            let mut done = 0u64;
            loop {
                let n = renderer.render(CHUNK_FRAMES, &mut frames)?;
                if n == 0 {
                    break;
                }
                bytes.clear();
                bytes.extend(frames.iter().flat_map(|s| s.to_le_bytes()));
                sink.write_all(&bytes).map_err(AupError::Pipe)?;
                done += n as u64;
                if let Some(p) = progress.as_mut() {
                    p.update(done);
                }
            }
            sink.flush().map_err(AupError::Pipe)
        };
        let pumped = pump();
        drop(sink); // end of input for the encoder
        let status = encoder.wait().map_err(AupError::Pipe)?;

        match pumped {
            // A broken pipe means the encoder went away; its exit status says why.
            Err(AupError::Pipe(_)) if !status.success() => {
                Err(AupError::Encode(describe_status(status)))
            }
            Err(err) => Err(err),
            Ok(()) if !status.success() => Err(AupError::Encode(describe_status(status))),
            Ok(()) => {
                if let Some(p) = progress.as_ref() {
                    p.finish();
                }
                Ok(())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fixture::{self, Block, ClipSpec, TrackSpec};
    use super::*;

    fn render_all(project: &Project) -> Vec<f32> {
        let mut renderer = project.renderer(0, u64::MAX);
        let (mut all, mut buf) = (Vec::new(), Vec::new());
        while renderer.render(1000, &mut buf).unwrap() > 0 {
            all.extend_from_slice(&buf);
        }
        all
    }

    /// 24-bit ramps: left counts up from 1, right counts down from -1.
    fn stereo_fixture(dir: &Path, frames: i32, block_len: usize) -> PathBuf {
        let path = dir.join("set.aup4");
        let left: Vec<i32> = (1..=frames).collect();
        let right: Vec<i32> = (1..=frames).map(|v| -v).collect();
        fixture::stereo_project(&path, 96_000, &left, &right, block_len);
        path
    }

    fn dir_listing(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn recognises_projects_by_extension() {
        assert!(is_project(Path::new("/x/set.aup4")));
        assert!(is_project(Path::new("/x/Set.AUP3")));
        assert!(is_project(Path::new("/x/old.aup")));
        assert!(!is_project(Path::new("/x/set.wav")));
        assert!(!is_project(Path::new("/x/aup4")));
    }

    #[test]
    fn decodes_each_sample_format_to_full_scale_floats() {
        let int16 = Block::int16(1, &[i16::MIN, 0, 16_384]);
        assert_eq!(
            decode_samples(int16.format, &int16.samples).unwrap(),
            [-1.0, 0.0, 0.5]
        );
        let int24 = Block::int24(1, &[-8_388_608, 0, 4_194_304, 1]);
        assert_eq!(
            decode_samples(int24.format, &int24.samples).unwrap(),
            [-1.0, 0.0, 0.5, 1.0 / 8_388_608.0]
        );
        let float = Block::float(1, &[-1.5, 0.25]);
        assert_eq!(
            decode_samples(float.format, &float.samples).unwrap(),
            [-1.5, 0.25]
        );
        assert_eq!(decode_samples(0x0008_0001, &[0; 8]), None);
    }

    #[test]
    fn uri_paths_escape_everything_sqlite_would_misread() {
        assert_eq!(
            uri_path(Path::new("/Users/dj/My Sets/a?b#c%d.aup4")),
            "/Users/dj/My%20Sets/a%3Fb%23c%25d.aup4"
        );
    }

    #[test]
    fn reads_a_stereo_project_sample_for_sample() {
        let dir = tempfile::tempdir().unwrap();
        let project = Project::open(&stereo_fixture(dir.path(), 2500, 1000)).unwrap();
        assert_eq!(project.layout().sample_rate, 96_000);
        assert_eq!(project.layout().frames, 2500);
        assert!(!project.has_unsaved_changes());

        let audio = render_all(&project);
        assert_eq!(audio.len(), 5000);
        for (frame, pair) in audio.chunks_exact(2).enumerate() {
            let expected = (frame as f32 + 1.0) / 8_388_608.0;
            assert_eq!(pair, [expected, -expected], "frame {frame}");
        }
    }

    #[test]
    fn opening_and_reading_leaves_the_project_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let path = stereo_fixture(dir.path(), 2500, 1000);
        let before = std::fs::read(&path).unwrap();
        let modified = std::fs::metadata(&path).unwrap().modified().unwrap();

        let project = Project::open(&path).unwrap();
        render_all(&project);
        drop(project);

        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert_eq!(
            std::fs::metadata(&path).unwrap().modified().unwrap(),
            modified
        );
        // No journal, -wal or -shm files appeared beside it.
        assert_eq!(dir_listing(dir.path()), ["set.aup4"]);
    }

    #[test]
    fn a_read_only_project_file_can_still_be_opened() {
        let dir = tempfile::tempdir().unwrap();
        let path = stereo_fixture(dir.path(), 100, 50);
        let mut permissions = std::fs::metadata(&path).unwrap().permissions();
        permissions.set_readonly(true);
        std::fs::set_permissions(&path, permissions).unwrap();
        assert_eq!(render_all(&Project::open(&path).unwrap()).len(), 200);
    }

    #[test]
    fn paths_with_spaces_and_symbols_open() {
        let dir = tempfile::tempdir().unwrap();
        let odd = dir.path().join("my set #2 (100%).aup4");
        std::fs::rename(stereo_fixture(dir.path(), 100, 50), &odd).unwrap();
        assert_eq!(Project::open(&odd).unwrap().layout().frames, 100);
    }

    #[test]
    fn a_project_open_in_audacity_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = stereo_fixture(dir.path(), 100, 50);
        for suffix in ["-wal", "-shm"] {
            let marker = sidecar(&path, suffix);
            std::fs::write(&marker, b"").unwrap();
            assert!(matches!(Project::open(&path), Err(AupError::InUse)));
            std::fs::remove_file(&marker).unwrap();
        }
        assert!(Project::open(&path).is_ok());
    }

    #[test]
    fn legacy_and_foreign_files_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let legacy = dir.path().join("old.aup");
        std::fs::write(&legacy, b"<?xml version=\"1.0\"?>").unwrap();
        assert!(matches!(
            Project::open(&legacy),
            Err(AupError::LegacyFormat)
        ));

        let text = dir.path().join("notes.aup4");
        std::fs::write(&text, b"this is not a database").unwrap();
        assert!(matches!(Project::open(&text), Err(AupError::Sqlite(_))));

        let other_db = dir.path().join("other.aup4");
        rusqlite::Connection::open(&other_db)
            .unwrap()
            .execute_batch("CREATE TABLE t(x);")
            .unwrap();
        assert!(matches!(
            Project::open(&other_db),
            Err(AupError::NotAProject(_))
        ));
    }

    #[test]
    fn unsaved_changes_are_detected() {
        let dir = tempfile::tempdir().unwrap();
        let path = stereo_fixture(dir.path(), 100, 50);
        rusqlite::Connection::open(&path)
            .unwrap()
            .execute(
                "INSERT INTO autosave(id, dict, doc) VALUES (1, x'', x'')",
                [],
            )
            .unwrap();
        assert!(Project::open(&path).unwrap().has_unsaved_changes());
    }

    #[test]
    fn a_missing_sample_block_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("broken.aup4");
        let track = TrackSpec::new(48_000.0, vec![ClipSpec::at_zero(vec![(7, 10)])]);
        let (dict, doc) = fixture::project_document(48_000.0, &[track]);
        fixture::write_project(&path, &dict, &doc, &[]);

        let project = Project::open(&path).unwrap();
        let mut renderer = project.renderer(0, u64::MAX);
        let err = AupError::from(renderer.render(100, &mut Vec::new()).unwrap_err());
        assert!(matches!(err, AupError::MissingBlock(7)), "{err}");
    }
}
