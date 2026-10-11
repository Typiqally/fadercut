//! Small shared ffmpeg helpers: tool lookup, probing, clip args, error context.

use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};

/// Homebrew's bin is not always on PATH (e.g. when launched from a GUI).
const HOMEBREW_BIN: &str = "/opt/homebrew/bin";

/// A `[start, start + duration]` slice of the input in seconds (preview mode).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Clip {
    pub start: f64,
    pub duration: f64,
}

/// Input-seek args for processing only a clip. Placed before `-i` for a fast seek.
pub fn clip_args(clip: Option<Clip>) -> Vec<String> {
    match clip {
        None => Vec::new(),
        Some(c) => vec![
            "-ss".to_owned(),
            c.start.to_string(),
            "-t".to_owned(),
            c.duration.to_string(),
        ],
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioFormat {
    pub sample_rate: u32,
    pub channels: u16,
}

/// Locate an executable on PATH, falling back to the Homebrew bin.
pub fn find_tool(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH").unwrap_or_default();
    find_tool_in(name, std::env::split_paths(&path), Path::new(HOMEBREW_BIN))
}

fn find_tool_in(
    name: &str,
    dirs: impl Iterator<Item = PathBuf>,
    fallback_dir: &Path,
) -> Option<PathBuf> {
    dirs.chain(std::iter::once(fallback_dir.to_path_buf()))
        .map(|dir| dir.join(name))
        .find(|candidate| is_executable(candidate))
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.metadata()
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

/// Sample rate and channel count of the first audio stream, or `None` if unknown.
pub fn probe_audio(ffprobe: &Path, input: &Path) -> Option<AudioFormat> {
    let out = Command::new(ffprobe)
        .args(["-v", "error", "-select_streams", "a:0"])
        .args(["-show_entries", "stream=sample_rate,channels"])
        .args(["-of", "default=noprint_wrappers=1:nokey=1"])
        .arg(input)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    parse_probe(&String::from_utf8_lossy(&out.stdout))
}

/// ffprobe prints sample_rate then channels, one per line.
fn parse_probe(stdout: &str) -> Option<AudioFormat> {
    let mut vals = stdout.split_whitespace();
    let sample_rate = vals.next()?.parse().ok()?;
    let channels = vals.next()?.parse().ok()?;
    (sample_rate > 0 && channels > 0).then_some(AudioFormat {
        sample_rate,
        channels,
    })
}

/// Last `max_chars` characters of `s` — ffmpeg's useful error is at the end.
pub fn tail(s: &str, max_chars: usize) -> &str {
    let count = s.chars().count();
    if count <= max_chars {
        return s;
    }
    match s.char_indices().nth(count - max_chars) {
        Some((idx, _)) => &s[idx..],
        None => s,
    }
}

/// Human-readable exit status for error messages, e.g. `exit 1`.
pub fn describe_status(status: ExitStatus) -> String {
    match status.code() {
        Some(code) => format!("exit {code}"),
        None => "terminated by signal".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clip_args_empty_without_clip() {
        assert!(clip_args(None).is_empty());
    }

    #[test]
    fn clip_args_seek_and_duration() {
        let clip = Clip {
            start: 30.0,
            duration: 60.5,
        };
        assert_eq!(clip_args(Some(clip)), ["-ss", "30", "-t", "60.5"]);
    }

    #[test]
    fn parse_probe_reads_rate_then_channels() {
        let fmt = parse_probe("44100\n2\n").unwrap();
        assert_eq!((fmt.sample_rate, fmt.channels), (44100, 2));
    }

    #[test]
    fn parse_probe_rejects_garbage() {
        assert!(parse_probe("").is_none());
        assert!(parse_probe("N/A\n2\n").is_none());
        assert!(parse_probe("44100\n").is_none());
        assert!(parse_probe("0\n2\n").is_none());
    }

    #[test]
    fn tail_keeps_last_chars_on_char_boundaries() {
        assert_eq!(tail("abcdef", 3), "def");
        assert_eq!(tail("ab", 3), "ab");
        assert_eq!(tail("héllo", 4), "éllo");
    }

    #[test]
    fn find_tool_falls_back_to_extra_dir() {
        let dir = tempfile::tempdir().unwrap();
        let tool = dir.path().join("sometool");
        std::fs::write(&tool, "#!/bin/sh\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let empty = std::iter::empty::<PathBuf>();
        assert_eq!(find_tool_in("sometool", empty, dir.path()), Some(tool));
        let empty = std::iter::empty::<PathBuf>();
        assert_eq!(find_tool_in("missing", empty, dir.path()), None);
    }
}
