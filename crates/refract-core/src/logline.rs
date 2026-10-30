//! The pure parts of the app's log file: splitting a byte stream into lines, stamping
//! each with the time, and deciding when the file is full.
//!
//! The app reads its own standard error in chunks of whatever size the pipe hands
//! over, so a line can arrive in pieces, and several lines in one piece.

use std::time::{Duration, SystemTime};

use chrono::{DateTime, Utc};

/// Lines longer than this many bytes (as read, before invalid UTF-8 is replaced) are
/// cut into pieces of at most this size, so one runaway write cannot grow without bound.
pub const MAX_LINE: usize = 16 * 1024;

/// Splits a byte stream into lines. A trailing `\r` is dropped, and bytes that are not
/// UTF-8 are replaced.
#[derive(Debug, Default)]
pub struct LineSplitter {
    partial: Vec<u8>,
}

impl LineSplitter {
    pub fn new() -> LineSplitter {
        LineSplitter::default()
    }

    /// Adds `chunk` and calls `emit` with each line it completes.
    pub fn push(&mut self, chunk: &[u8], mut emit: impl FnMut(&str)) {
        let mut rest = chunk;
        while let Some(end) = rest.iter().position(|&b| b == b'\n') {
            let line = &rest[..end];
            // A `\r` here is dropped now, so it does not fill a piece of its own (one that
            // ends a chunk can still leave an empty line when a cut falls just before it).
            self.extend(line.strip_suffix(b"\r").unwrap_or(line), &mut emit);
            self.emit_partial(&mut emit);
            rest = &rest[end + 1..];
        }
        self.extend(rest, &mut emit);
    }

    /// Adds `bytes` to the unfinished line, emitting a piece each time it fills up.
    fn extend(&mut self, mut bytes: &[u8], emit: &mut impl FnMut(&str)) {
        while self.partial.len() + bytes.len() > MAX_LINE {
            let (head, tail) = bytes.split_at(MAX_LINE - self.partial.len());
            self.partial.extend_from_slice(head);
            emit(&String::from_utf8_lossy(&self.partial));
            self.partial.clear();
            bytes = tail;
        }
        self.partial.extend_from_slice(bytes);
    }

    /// Calls `emit` with what is left after the last newline, if anything.
    pub fn finish(&mut self, mut emit: impl FnMut(&str)) {
        if !self.partial.is_empty() {
            self.emit_partial(&mut emit);
        }
    }

    fn emit_partial(&mut self, emit: &mut impl FnMut(&str)) {
        if self.partial.last() == Some(&b'\r') {
            self.partial.pop();
        }
        emit(&String::from_utf8_lossy(&self.partial));
        self.partial.clear();
    }
}

/// `time` in UTC to the millisecond, as `2026-10-30T14:03:12.345Z`.
pub fn timestamp(time: SystemTime) -> String {
    let since = time
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or(Duration::ZERO);
    DateTime::<Utc>::from_timestamp(since.as_secs() as i64, since.subsec_nanos())
        .unwrap_or_default()
        .format("%Y-%m-%dT%H:%M:%S%.3fZ")
        .to_string()
}

/// `line` as it goes into the log file: the time, a space, the line and a newline.
pub fn stamped(time: SystemTime, line: &str) -> String {
    format!("{} {line}\n", timestamp(time))
}

/// Whether a file of `size` bytes must be rotated before `incoming` more bytes are
/// written, to stay within `limit`. An empty file never is, so a single line larger
/// than the limit still gets written.
pub fn needs_rotation(size: u64, incoming: u64, limit: u64) -> bool {
    size > 0 && size.saturating_add(incoming) > limit
}

#[cfg(test)]
mod tests {
    use super::*;

    fn split(chunks: &[&[u8]]) -> Vec<String> {
        let mut splitter = LineSplitter::new();
        let mut lines = Vec::new();
        for chunk in chunks {
            splitter.push(chunk, |l| lines.push(l.to_owned()));
        }
        splitter.finish(|l| lines.push(l.to_owned()));
        lines
    }

    #[test]
    fn lines_are_joined_across_chunks() {
        assert_eq!(
            split(&[
                b"refract: hel",
                b"per started\nsecond",
                b" line\n",
                b"third"
            ]),
            ["refract: helper started", "second line", "third"]
        );
        assert_eq!(split(&[b"a\nb\n\nc\n"]), ["a", "b", "", "c"]);
        assert_eq!(split(&[b"", b"\n"]), [""]);
        assert!(split(&[b""]).is_empty());
    }

    #[test]
    fn crlf_and_bad_utf8_are_tidied() {
        assert_eq!(split(&[b"one\r", b"\ntwo\r\n"]), ["one", "two"]);
        // A character split across two chunks survives.
        assert_eq!(split(&[b"caf\xc3", b"\xa9\n"]), ["caf\u{e9}"]);
        assert_eq!(split(&[b"bad \xff byte\n"]), ["bad \u{fffd} byte"]);
    }

    #[test]
    fn no_line_is_longer_than_the_cap() {
        let mut stream = vec![b'a'; MAX_LINE * 3 + 7];
        stream.extend_from_slice(b"\n");
        stream.extend(vec![b'b'; MAX_LINE]);
        stream.extend_from_slice(b"\r\nshort\n");
        stream.extend(vec![b'c'; MAX_LINE - 3]);
        // One chunk holding everything, and the same bytes in odd-sized chunks.
        let whole = split(&[&stream]);
        let pieces: Vec<&[u8]> = stream.chunks(MAX_LINE / 3 + 1).collect();
        for lines in [&whole, &split(&pieces)] {
            let lengths: Vec<usize> = lines.iter().map(String::len).collect();
            assert_eq!(
                lengths,
                [MAX_LINE, MAX_LINE, MAX_LINE, 7, MAX_LINE, 5, MAX_LINE - 3]
            );
            assert_eq!(lines[5], "short");
        }
    }

    #[test]
    fn runaway_lines_are_cut() {
        let long = vec![b'x'; MAX_LINE * 2 + 5];
        let lines = split(&[&long[..MAX_LINE - 1], &long[MAX_LINE - 1..], b"\n"]);
        let lengths: Vec<usize> = lines.iter().map(String::len).collect();
        assert_eq!(lengths, [MAX_LINE, MAX_LINE, 5]);
    }

    #[test]
    fn timestamps_are_utc_to_the_millisecond() {
        let time = SystemTime::UNIX_EPOCH + Duration::from_millis(1_793_369_592_345);
        assert_eq!(timestamp(time), "2026-10-30T14:13:12.345Z");
        assert_eq!(
            stamped(time, "refract: ready"),
            "2026-10-30T14:13:12.345Z refract: ready\n"
        );
        assert_eq!(
            timestamp(SystemTime::UNIX_EPOCH - Duration::from_secs(5)),
            "1970-01-01T00:00:00.000Z"
        );
    }

    #[test]
    fn files_rotate_past_the_limit() {
        assert!(!needs_rotation(0, 10, 100));
        assert!(!needs_rotation(0, 500, 100));
        assert!(!needs_rotation(90, 10, 100));
        assert!(needs_rotation(91, 10, 100));
        assert!(needs_rotation(200, 0, 100));
        assert!(needs_rotation(u64::MAX, 1, 100));
    }
}
