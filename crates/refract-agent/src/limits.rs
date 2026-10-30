//! Plan limits for the `usage` request, read from the file a status line script
//! writes (see [`refract_core::limits`]).
//!
//! The file is opened on every `usage`, but only read and parsed again when its
//! size, modification time or inode changed, so an unchanged file costs an open and
//! an `fstat`. It is opened without blocking and checked through that one handle, so
//! a FIFO or other special file put in its place is refused rather than waited on.
//! A file larger than [`MAX_BYTES`] is refused unread. A missing, unreadable, too
//! large or malformed file means no limits, never an error. The file is only ever
//! read.

use std::fs::{self, File};
use std::io::Read;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::PathBuf;
use std::time::SystemTime;

use refract_core::limits::{self, PlanLimits};

/// The largest file read; the status line's is about 150 bytes.
pub const MAX_BYTES: u64 = 64 * 1024;

/// `O_NONBLOCK` on Linux, so opening a FIFO does not wait for a writer.
const O_NONBLOCK: i32 = 0o4000;

/// What identifies one version of the file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Stamp {
    len: u64,
    mtime: Option<SystemTime>,
    ino: u64,
}

/// The status line's limits file and its last reading.
#[derive(Debug)]
pub struct LimitsFile {
    path: PathBuf,
    /// The version last parsed and what it held.
    cached: Option<(Stamp, Option<PlanLimits>)>,
}

impl LimitsFile {
    pub fn new(path: PathBuf) -> LimitsFile {
        LimitsFile { path, cached: None }
    }

    /// The limits in the file now, parsing it again only when it changed.
    pub fn read(&mut self) -> Option<PlanLimits> {
        let opened = fs::OpenOptions::new()
            .read(true)
            .custom_flags(O_NONBLOCK)
            .open(&self.path);
        let Ok((file, meta)) = opened.and_then(|f| f.metadata().map(|m| (f, m))) else {
            self.cached = None;
            return None;
        };
        let stamp = Stamp {
            len: meta.len(),
            mtime: meta.modified().ok(),
            ino: meta.ino(),
        };
        if let Some((cached, limits)) = &self.cached {
            if *cached == stamp {
                return limits.clone();
            }
        }
        let limits = if meta.is_file() && meta.len() <= MAX_BYTES {
            read_bounded(file).and_then(|text| limits::parse(&text))
        } else {
            None
        };
        self.cached = Some((stamp, limits.clone()));
        limits
    }

    /// The version last parsed, for tests.
    #[cfg(test)]
    fn parses(&self) -> Option<Stamp> {
        self.cached.as_ref().map(|(stamp, _)| *stamp)
    }
}

/// The text of `file`, or `None` if it is not UTF-8 or has grown past [`MAX_BYTES`]
/// since it was checked.
fn read_bounded(file: File) -> Option<String> {
    let mut text = String::new();
    file.take(MAX_BYTES + 1).read_to_string(&mut text).ok()?;
    (text.len() as u64 <= MAX_BYTES).then_some(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use std::time::Duration;

    fn reading(used: f64) -> String {
        format!(
            r#"{{"updated": 1793361600, "five_hour": {{"used": {used}, "resets_at": 1793370000}}, "seven_day": null}}"#
        )
    }

    fn five(file: &mut LimitsFile) -> Option<f64> {
        file.read()?.five_hour.map(|w| w.used_percent)
    }

    #[test]
    fn reads_the_file_again_only_when_it_changes() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("usage-state.json");
        let mut file = LimitsFile::new(path.clone());
        assert_eq!(file.read(), None);

        fs::write(&path, reading(2.0)).unwrap();
        assert_eq!(five(&mut file), Some(2.0));
        let first = file.parses().unwrap();

        // Same size, same time: the cached reading, even though the bytes differ.
        let mtime = fs::metadata(&path).unwrap().modified().unwrap();
        fs::write(&path, reading(3.0)).unwrap();
        File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(mtime)
            .unwrap();
        assert_eq!(five(&mut file), Some(2.0));
        assert_eq!(file.parses(), Some(first));

        // A new time is read again.
        File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(mtime + Duration::from_secs(60))
            .unwrap();
        assert_eq!(five(&mut file), Some(3.0));

        // A new size is read again.
        fs::write(&path, reading(12.5)).unwrap();
        assert_eq!(five(&mut file), Some(12.5));

        // A file renamed into place with the same size and time is another inode.
        let mtime = fs::metadata(&path).unwrap().modified().unwrap();
        let other = tmp.path().join("usage-state.json.tmp");
        fs::write(&other, reading(13.5)).unwrap();
        File::options()
            .write(true)
            .open(&other)
            .unwrap()
            .set_modified(mtime)
            .unwrap();
        let before = file.parses().unwrap();
        fs::rename(&other, &path).unwrap();
        let meta = fs::metadata(&path).unwrap();
        assert_eq!(
            (meta.len(), meta.modified().unwrap()),
            (before.len, before.mtime.unwrap())
        );
        assert_eq!(five(&mut file), Some(13.5));
    }

    #[test]
    fn large_files_and_fifos_give_nothing_without_waiting() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("usage-state.json");
        let mut file = LimitsFile::new(path.clone());
        // Valid JSON padded past the limit.
        let padded = format!("{}{}", reading(5.0), " ".repeat(MAX_BYTES as usize));
        fs::write(&path, padded).unwrap();
        assert_eq!(file.read(), None);
        fs::write(&path, reading(5.0)).unwrap();
        assert_eq!(five(&mut file), Some(5.0));

        // A FIFO with no writer: opening it must not block.
        fs::remove_file(&path).unwrap();
        let made = std::process::Command::new("mkfifo")
            .arg(&path)
            .status()
            .unwrap();
        assert!(made.success());
        assert_eq!(file.read(), None);
    }

    #[test]
    fn missing_or_malformed_files_give_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("usage-state.json");
        let mut file = LimitsFile::new(path.clone());
        fs::write(&path, "{not json").unwrap();
        assert_eq!(file.read(), None);
        fs::write(&path, reading(-4.0)).unwrap();
        assert_eq!(file.read(), None);
        fs::write(&path, reading(40.0)).unwrap();
        assert_eq!(five(&mut file), Some(40.0));
        fs::remove_file(&path).unwrap();
        assert_eq!(file.read(), None);
        assert_eq!(file.parses(), None);
        fs::create_dir(&path).unwrap();
        assert_eq!(file.read(), None);
    }
}
