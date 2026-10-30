//! Keeps the app's standard error in `refract.log` when it has nowhere else to go.
//!
//! A release build has no console, so its standard error handle is empty and every
//! `eprintln!`, the helper's own lines forwarded by the client included, would be lost.
//! [`init`] then points standard error at a pipe. Rust's standard library looks the
//! handle up again on every write, so existing `eprintln!`s land in the pipe. One thread
//! drains the pipe and stamps each line with the time; another appends the lines to
//! the file, so a slow disk never holds up a writer. When the file thread falls behind,
//! lines are dropped and counted rather than queued without bound, and the count is
//! written once the queue empties.
//!
//! Both threads run for the life of the process, so the lines still queued when it
//! exits or panics would be lost; [`flush`] and the panic hook wait briefly for them.
//! Neither thread writes to standard error itself, which could wait on its own pipe.

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError};
use std::time::{Duration, Instant, SystemTime};

use refract_core::logline::{needs_rotation, stamped, LineSplitter};

/// The log file, in the app's log folder.
pub const LOG_NAME: &str = "refract.log";
/// The previous log file, replaced at each rotation.
pub const OLD_LOG_NAME: &str = "refract.old.log";
/// A log larger than this at startup is rotated before anything is written.
pub const STARTUP_LIMIT: u64 = 1024 * 1024;
/// The log is rotated while running once it would grow past this.
pub const RUNTIME_LIMIT: u64 = 2 * 1024 * 1024;
/// Lines waiting for the file thread before new ones are dropped.
const QUEUE: usize = 1024;
/// How long exiting or panicking waits for queued lines to reach the file.
const FLUSH_WAIT: Duration = Duration::from_millis(200);
/// Starts the numbered mark, `<FLUSH_MARK><n>]`, that ends a line [`flush`] writes to
/// standard error and the pipe thread swallows: once mark `n` has been read, everything
/// written before it has been read too.
const FLUSH_MARK: &str = "\u{1b}[refract-log-flush ";
/// How long the pipe thread waits after a failed read before reading again.
const READ_RETRY: Duration = Duration::from_millis(50);
/// Failed reads in a row (about a second's worth) after which the pipe thread stops.
const READ_FAILURES: u32 = 20;

/// Appends lines to `refract.log` in `dir`, rotating it to `refract.old.log` past a
/// limit. Failures are not reported anywhere: the line is dropped and the file is
/// opened again for the next one.
pub struct LogFile {
    dir: PathBuf,
    file: Option<File>,
    size: u64,
    limit: u64,
    /// The size past which the next rotation is tried: `limit`, or twice that after a
    /// rotation failed.
    rotate_at: u64,
    /// The last rotation failed.
    blocked: bool,
}

impl LogFile {
    /// Opens the log in `dir`, creating the folder, and rotates it first if it is over
    /// `startup_limit`. After that it is rotated whenever it would pass `limit`.
    pub fn open(dir: &Path, startup_limit: u64, limit: u64) -> LogFile {
        let mut log = LogFile {
            dir: dir.to_path_buf(),
            file: None,
            size: 0,
            limit,
            rotate_at: limit,
            blocked: false,
        };
        log.reopen();
        if needs_rotation(log.size, 0, startup_limit) {
            log.rotate(0);
        }
        log
    }

    pub fn path(&self) -> PathBuf {
        self.dir.join(LOG_NAME)
    }

    /// Appends `text`, which should end in a newline, rotating first if it would take
    /// the file past the limit.
    pub fn append(&mut self, text: &str) {
        if needs_rotation(self.size, text.len() as u64, self.rotate_at) {
            self.rotate(text.len() as u64);
        }
        if self.file.is_none() {
            self.reopen();
        }
        let Some(file) = &mut self.file else { return };
        match file.write_all(text.as_bytes()) {
            Ok(()) => self.size += text.len() as u64,
            Err(_) => self.file = None,
        }
    }

    /// Renames the log to `refract.old.log`, replacing any old one, and starts a new
    /// one. The rename fails while another program holds the log without letting it be
    /// renamed (a `Get-Content -Wait` tail, say). The log then keeps its handle and may
    /// grow to twice the limit before one more try; if that fails too, or `incoming`
    /// more bytes would take it past twice the limit, it is emptied instead.
    fn rotate(&mut self, incoming: u64) {
        let twice = self.limit.saturating_mul(2);
        // Our own handle allows renaming, so it stays open in case the rename fails.
        if std::fs::rename(self.path(), self.dir.join(OLD_LOG_NAME)).is_ok() {
            self.reopen();
        } else if !(self.blocked || self.size.saturating_add(incoming) >= twice) {
            self.blocked = true;
            self.rotate_at = twice;
            return;
        } else if !self.truncate() {
            // Not even emptied: try again after another limit's worth.
            self.rotate_at = self.size.saturating_add(self.limit);
            return;
        }
        self.blocked = false;
        self.rotate_at = self.limit;
    }

    /// Empties the log, returning whether that worked.
    fn truncate(&mut self) -> bool {
        self.file = None;
        let emptied = OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(self.path())
            .is_ok();
        self.reopen();
        emptied
    }

    fn reopen(&mut self) {
        let opened = std::fs::create_dir_all(&self.dir).and_then(|()| {
            OpenOptions::new()
                .create(true)
                .append(true)
                .open(self.path())
        });
        self.file = opened.ok();
        self.size = self
            .file
            .as_ref()
            .and_then(|f| f.metadata().ok())
            .map_or(0, |m| m.len());
    }
}

/// What goes to the file thread.
pub enum Message {
    /// A stamped line, ending in a newline.
    Line(String),
    /// Lines were dropped: write how many once the queue is empty.
    Dropped,
}

/// Counters the two threads share with [`flush`].
#[derive(Default)]
pub struct Shared {
    /// Lines queued for the file thread and not yet written.
    pending: AtomicUsize,
    /// Lines dropped and not yet reported in the file.
    dropped: AtomicU64,
    /// The number of the last flush mark written to the pipe, and the highest read back.
    marks_sent: AtomicU64,
    marks_seen: AtomicU64,
}

impl Shared {
    /// Whether everything up to flush mark `mark` is in the file.
    fn written_through(&self, mark: u64) -> bool {
        self.marks_seen.load(Ordering::SeqCst) >= mark
            && self.pending.load(Ordering::SeqCst) == 0
            && self.dropped.load(Ordering::SeqCst) == 0
    }
}

/// Reads `input` until it ends, sending each line, stamped with the time it was read,
/// to `lines`. A full queue drops the line and counts it; a closed queue drops
/// everything. A failed read is tried again after a pause; after [`READ_FAILURES`] in
/// a row it gives up, rather than leave writers waiting on a pipe that fills.
pub fn pump(mut input: impl Read, lines: &SyncSender<Message>, shared: &Shared) {
    let mut splitter = LineSplitter::new();
    let queue = |line: &str| {
        shared.pending.fetch_add(1, Ordering::SeqCst);
        match lines.try_send(Message::Line(stamped(SystemTime::now(), line))) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => {
                shared.pending.fetch_sub(1, Ordering::SeqCst);
                shared.dropped.fetch_add(1, Ordering::SeqCst);
            }
            Err(TrySendError::Disconnected(_)) => {
                shared.pending.fetch_sub(1, Ordering::SeqCst);
            }
        }
    };
    // A flush mark may end a line another process left unfinished; the rest is kept.
    let send = |line: &str| match flush_mark(line) {
        Some((rest, mark)) => {
            if !rest.is_empty() {
                queue(rest);
            }
            shared.marks_seen.fetch_max(mark, Ordering::SeqCst);
        }
        None => queue(line),
    };
    // Counted after the line it follows, so a full queue still has a line to finish
    // and the file thread sees the count when it gets to the end.
    let report_drops = || {
        if shared.dropped.load(Ordering::SeqCst) > 0 {
            let _ = lines.try_send(Message::Dropped);
        }
    };
    let mut buf = [0u8; 8192];
    let mut failures = 0;
    loop {
        match input.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                failures = 0;
                splitter.push(&buf[..n], send);
                report_drops();
            }
            Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => {
                failures += 1;
                if failures == READ_FAILURES {
                    break;
                }
                std::thread::sleep(READ_RETRY);
            }
        }
    }
    splitter.finish(send);
    report_drops();
}

/// The text before a flush mark that ends `line`, and the mark's number.
fn flush_mark(line: &str) -> Option<(&str, u64)> {
    let (rest, tail) = line.rsplit_once(FLUSH_MARK)?;
    let mark = tail.strip_suffix(']')?.parse().ok()?;
    Some((rest, mark))
}

/// Writes everything from `lines` to `log` until the sending side is gone, and how
/// many lines were dropped whenever the queue has emptied.
pub fn drain(lines: Receiver<Message>, mut log: LogFile, shared: &Shared) {
    for message in lines {
        if let Message::Line(line) = message {
            log.append(&line);
            shared.pending.fetch_sub(1, Ordering::SeqCst);
        }
        if shared.pending.load(Ordering::SeqCst) == 0 {
            let dropped = shared.dropped.load(Ordering::SeqCst);
            if dropped > 0 {
                let note = format!("refract: {dropped} log lines were dropped");
                log.append(&stamped(SystemTime::now(), &note));
                // Only now, so a flush cannot finish before the note is in the file.
                shared.dropped.fetch_sub(dropped, Ordering::SeqCst);
            }
        }
    }
}

/// Writes a flush mark to `stderr`, the log's pipe, and waits until the pipe thread
/// has read it and every line before it is in the file, or until `timeout`. Returns
/// whether everything got there.
pub fn flush_through(shared: &Shared, mut stderr: impl Write, timeout: Duration) -> bool {
    let mark = shared.marks_sent.fetch_add(1, Ordering::SeqCst) + 1;
    if writeln!(stderr, "{FLUSH_MARK}{mark}]").is_err() {
        return false;
    }
    let deadline = Instant::now() + timeout;
    while !shared.written_through(mark) {
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    true
}

/// Waits briefly for lines already written to standard error to reach the log, before
/// the app exits. Does nothing when there is no log.
pub fn flush() {
    #[cfg(windows)]
    win::flush(FLUSH_WAIT);
}

/// Sends standard error to `refract.log` in the app's log folder, if standard error
/// goes nowhere (a release build, or one started without a console). Run from a
/// terminal, or with standard error redirected, it is left alone.
pub fn init(app: &tauri::AppHandle) {
    #[cfg(windows)]
    {
        use tauri::Manager;
        if win::stderr_is_usable() {
            return;
        }
        let Ok(dir) = app.path().app_log_dir() else {
            return;
        };
        let version = app.package_info().version.to_string();
        if win::start(&dir, &version) {
            win::flush_on_panic();
        }
    }
    #[cfg(not(windows))]
    let _ = app;
}

#[cfg(windows)]
mod win {
    use std::os::windows::io::{FromRawHandle, OwnedHandle};
    use std::path::Path;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::OnceLock;
    use std::time::{Duration, SystemTime};

    use refract_core::logline::stamped;
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::Console::{GetStdHandle, SetStdHandle, STD_ERROR_HANDLE};
    use windows::Win32::System::Pipes::CreatePipe;

    use super::{
        drain, flush_through, pump, LogFile, Shared, FLUSH_WAIT, QUEUE, RUNTIME_LIMIT,
        STARTUP_LIMIT,
    };

    /// Bytes the pipe holds before a writer waits for the reader.
    const PIPE_SIZE: u32 = 64 * 1024;
    const FILE_THREAD: &str = "refract-log-file";
    const PIPE_THREAD: &str = "refract-log-pipe";

    /// Set once [`start`] begins, so it runs at most once.
    static STARTED: AtomicBool = AtomicBool::new(false);
    /// The running log's counters, set once standard error points at it.
    static RUNNING: OnceLock<&'static Shared> = OnceLock::new();

    /// Whether this process has a standard error handle. A GUI app started from
    /// Explorer, the Run key or a shortcut has none. A handle to a pipe whose reader
    /// has gone is not caught: it still reports itself as a pipe, and no cheap call
    /// tells a broken one apart without writing to it.
    pub fn stderr_is_usable() -> bool {
        // SAFETY: only reads the process's standard handle.
        unsafe { GetStdHandle(STD_ERROR_HANDLE) }.is_ok()
    }

    /// Starts the log and points standard error at it, returning whether it did.
    pub fn start(dir: &Path, version: &str) -> bool {
        if STARTED.swap(true, Ordering::SeqCst) {
            return false;
        }
        let mut log = LogFile::open(dir, STARTUP_LIMIT, RUNTIME_LIMIT);
        let header = format!(
            "refract: Refract {version} started (process {})",
            std::process::id()
        );
        log.append(&stamped(SystemTime::now(), &header));

        let (mut read, mut write) = (HANDLE::default(), HANDLE::default());
        // SAFETY: both handles are written by the call; neither is inheritable, and
        // `Command` duplicates the write end for children that inherit standard error.
        if unsafe { CreatePipe(&mut read, &mut write, None, PIPE_SIZE) }.is_err() {
            log.append(&stamped(
                SystemTime::now(),
                "refract: cannot create the log pipe",
            ));
            return false;
        }
        // SAFETY: `read` is a fresh handle owned from here on; `write` stays open for
        // the life of the process as its standard error.
        let read = std::fs::File::from(unsafe { OwnedHandle::from_raw_handle(read.0) });
        // Shared with threads that run for the life of the process.
        let shared: &'static Shared = Box::leak(Box::default());

        let (lines, queue) = std::sync::mpsc::sync_channel(QUEUE);
        let file_thread = std::thread::Builder::new()
            .name(FILE_THREAD.into())
            .spawn(move || drain(queue, log, shared));
        if file_thread.is_err() {
            return false;
        }
        let pipe_thread = std::thread::Builder::new()
            .name(PIPE_THREAD.into())
            .spawn(move || {
                // Dropped before `read`, which the closure owns.
                let _reset = ResetOnExit;
                pump(&read, &lines, shared);
            });
        // SAFETY: `write` is a valid pipe handle that is never closed.
        if pipe_thread.is_err() || unsafe { SetStdHandle(STD_ERROR_HANDLE, write) }.is_err() {
            return false;
        }
        RUNNING.set(shared).is_ok()
    }

    pub fn flush(timeout: Duration) -> bool {
        let Some(shared) = RUNNING.get() else {
            return true;
        };
        flush_through(shared, std::io::stderr(), timeout)
    }

    /// After the usual panic message, waits briefly for it to reach the log. A panic
    /// on one of the log's own threads empties standard error first, since nothing
    /// would read what it wrote.
    pub fn flush_on_panic() {
        let report = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let name = std::thread::current().name().map(str::to_owned);
            let own = matches!(name.as_deref(), Some(FILE_THREAD | PIPE_THREAD));
            if own {
                // SAFETY: only replaces the process's standard handle.
                let _ = unsafe { SetStdHandle(STD_ERROR_HANDLE, HANDLE::default()) };
            }
            report(info);
            if !own {
                flush(FLUSH_WAIT);
            }
        }));
    }

    /// Empties the standard error handle if the pipe thread ever stops (it panicked,
    /// or every write end closed), before the read end closes, so later writes are
    /// dropped instead of failing or waiting on a pipe nobody reads.
    struct ResetOnExit;

    impl Drop for ResetOnExit {
        fn drop(&mut self) {
            // SAFETY: only replaces the process's standard handle.
            let _ = unsafe { SetStdHandle(STD_ERROR_HANDLE, HANDLE::default()) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use refract_core::logline::MAX_LINE;

    fn temp_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("refract-logfile-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn read(path: PathBuf) -> String {
        std::fs::read_to_string(path).unwrap_or_default()
    }

    fn lines_of(rx: &Receiver<Message>) -> Vec<String> {
        rx.try_iter()
            .filter_map(|m| match m {
                Message::Line(line) => Some(line),
                Message::Dropped => None,
            })
            .collect()
    }

    #[test]
    fn appends_and_rotates_past_the_limit() {
        let dir = temp_dir("rotate");
        let mut log = LogFile::open(&dir.join("logs"), STARTUP_LIMIT, 10);
        log.append("one\n");
        log.append("two\n");
        assert_eq!(read(log.path()), "one\ntwo\n");
        log.append("three\n");
        assert_eq!(read(log.path()), "three\n");
        assert_eq!(read(dir.join("logs").join(OLD_LOG_NAME)), "one\ntwo\n");
        log.append("four\n");
        assert_eq!(read(dir.join("logs").join(OLD_LOG_NAME)), "three\n");
        assert_eq!(read(log.path()), "four\n");
        drop(log);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn rotates_a_large_log_at_startup() {
        let dir = temp_dir("startup");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(LOG_NAME), "0123456789").unwrap();
        std::fs::write(dir.join(OLD_LOG_NAME), "older").unwrap();

        let mut log = LogFile::open(&dir, 20, RUNTIME_LIMIT);
        log.append("kept\n");
        assert_eq!(read(log.path()), "0123456789kept\n");
        drop(log);

        let mut log = LogFile::open(&dir, 10, RUNTIME_LIMIT);
        log.append("fresh\n");
        assert_eq!(read(log.path()), "fresh\n");
        assert_eq!(read(dir.join(OLD_LOG_NAME)), "0123456789kept\n");
        drop(log);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_blocked_rotation_waits_then_empties_the_log() {
        let dir = temp_dir("stuck");
        // A folder with something in it cannot be replaced by a rename on any system.
        std::fs::create_dir_all(dir.join(OLD_LOG_NAME)).unwrap();
        std::fs::write(dir.join(OLD_LOG_NAME).join("keep"), "x").unwrap();
        let mut log = LogFile::open(&dir, STARTUP_LIMIT, 10);
        let size = |log: &LogFile| std::fs::metadata(log.path()).unwrap().len();
        log.append("0123456789\n");
        // Past the limit: the rename fails, so the log may grow to twice the limit (20
        // bytes) before trying again.
        for line in ["a\n", "b\n", "c\n", "d\n"] {
            log.append(line);
            assert!(size(&log) <= 20);
        }
        assert_eq!(read(log.path()), "0123456789\na\nb\nc\nd\n");
        // The second try fails too, so the log is emptied before it passes 20 bytes.
        log.append("e\n");
        assert_eq!(read(log.path()), "e\n");
        // A fresh log goes back to the usual limit.
        for line in ["f\n", "g\n", "h\n", "i\n"] {
            log.append(line);
        }
        assert_eq!(read(log.path()), "e\nf\ng\nh\ni\n");
        log.append("j\n");
        assert!(size(&log) <= 20);
        assert_eq!(read(log.path()), "e\nf\ng\nh\ni\nj\n");
        drop(log);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_missing_folder_drops_lines_without_failing() {
        let dir = temp_dir("blocked");
        std::fs::write(&dir, "a file where the folder should be").unwrap();
        let mut log = LogFile::open(&dir.join("logs"), STARTUP_LIMIT, RUNTIME_LIMIT);
        log.append("lost\n");
        std::fs::remove_file(&dir).unwrap();
        log.append("found\n");
        assert_eq!(read(log.path()), "found\n");
        drop(log);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn pumps_stamped_lines_and_counts_dropped_ones() {
        let shared = Shared::default();
        let (tx, rx) = std::sync::mpsc::sync_channel(2);
        pump(&b"first\nsecond\nthird\nfourth"[..], &tx, &shared);
        let got = lines_of(&rx);
        assert_eq!(got.len(), 2);
        assert!(got[0].ends_with("Z first\n"), "{got:?}");
        assert!(got[1].ends_with("Z second\n"), "{got:?}");
        assert_eq!(shared.pending.load(Ordering::SeqCst), 2);
        assert_eq!(shared.dropped.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn dropped_lines_are_reported_once_the_queue_empties() {
        let dir = temp_dir("dropped");
        let shared = Shared::default();
        let (tx, rx) = std::sync::mpsc::sync_channel(2);
        pump(&b"one\ntwo\nthree\nfour\n"[..], &tx, &shared);
        drop(tx);
        let log = LogFile::open(&dir, STARTUP_LIMIT, RUNTIME_LIMIT);
        let path = log.path();
        drain(rx, log, &shared);
        let text = read(path);
        let lines: Vec<&str> = text.lines().map(|l| l.split_once(' ').unwrap().1).collect();
        assert_eq!(lines, ["one", "two", "refract: 2 log lines were dropped"]);
        assert!(shared.written_through(0));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn keeps_reading_after_a_failed_read_and_without_a_file_thread() {
        let shared = Shared::default();
        let (tx, rx) = std::sync::mpsc::sync_channel(4);
        drop(rx);
        let mut input = Flaky(vec![
            Ok(b"nobody\n".to_vec()),
            Err(io::ErrorKind::BrokenPipe),
            Ok(b"listens\n".to_vec()),
        ]);
        pump(&mut input, &tx, &shared);
        assert!(input.0.is_empty());
        assert!(shared.written_through(0));
    }

    #[test]
    fn gives_up_after_reads_keep_failing() {
        let shared = Shared::default();
        let (tx, _rx) = std::sync::mpsc::sync_channel(4);
        let mut input = Flaky(
            (0..READ_FAILURES + 5)
                .map(|_| Err(io::ErrorKind::Other))
                .collect(),
        );
        pump(&mut input, &tx, &shared);
        assert_eq!(input.0.len(), 5);
    }

    #[test]
    fn flushing_waits_for_earlier_lines_and_is_bounded() {
        let dir = temp_dir("flush");
        let shared = Shared::default();
        let (reader, mut writer) = io::pipe().unwrap();
        let (tx, rx) = std::sync::mpsc::sync_channel(QUEUE);
        let log = LogFile::open(&dir, STARTUP_LIMIT, RUNTIME_LIMIT);
        let path = log.path();
        let shared = &shared;
        std::thread::scope(|scope| {
            // Each thread owns its end of the queue, so both finish once `writer` closes.
            scope.spawn(move || pump(reader, &tx, shared));
            scope.spawn(move || drain(rx, log, shared));
            write!(writer, "first\nunfinished ").unwrap();
            assert!(flush_through(shared, &writer, Duration::from_secs(10)));
            let text = read(path.clone());
            let lines: Vec<&str> = text.lines().map(|l| l.split_once(' ').unwrap().1).collect();
            assert_eq!(lines, ["first", "unfinished "]);
            assert!(!text.contains(FLUSH_MARK));
            drop(writer);
        });

        // A mark cut in two by an overlong line is missed, but the next one still counts.
        let (reader, mut writer) = io::pipe().unwrap();
        let (tx, rx) = std::sync::mpsc::sync_channel(QUEUE);
        let log = LogFile::open(&dir.join("split"), STARTUP_LIMIT, RUNTIME_LIMIT);
        std::thread::scope(|scope| {
            scope.spawn(move || pump(reader, &tx, shared));
            scope.spawn(move || drain(rx, log, shared));
            writer.write_all(&vec![b'x'; MAX_LINE - 5]).unwrap();
            assert!(!flush_through(shared, &writer, Duration::from_millis(100)));
            let started = Instant::now();
            assert!(flush_through(shared, &writer, Duration::from_secs(10)));
            assert!(started.elapsed() < Duration::from_secs(5));
            drop(writer);
        });

        // Nothing reads this pipe, so the wait gives up.
        let (_reader, writer) = io::pipe().unwrap();
        let started = Instant::now();
        assert!(!flush_through(shared, &writer, Duration::from_millis(30)));
        assert!(started.elapsed() < Duration::from_secs(2));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Hands out one result per read.
    struct Flaky(Vec<Result<Vec<u8>, io::ErrorKind>>);

    impl Read for Flaky {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if self.0.is_empty() {
                return Ok(0);
            }
            let chunk = self.0.remove(0).map_err(io::Error::from)?;
            buf[..chunk.len()].copy_from_slice(&chunk);
            Ok(chunk.len())
        }
    }

    /// The real redirect: a write to standard error ends up in the file.
    #[cfg(windows)]
    #[test]
    fn standard_error_goes_to_the_file() {
        use windows::Win32::System::Console::{GetStdHandle, SetStdHandle, STD_ERROR_HANDLE};

        let dir = temp_dir("redirect");
        // SAFETY: reads and later restores the process's standard handle.
        let old = unsafe { GetStdHandle(STD_ERROR_HANDLE) }.unwrap_or_default();
        assert!(win::start(&dir, "9.9.9"));
        let wrote = writeln!(io::stderr(), "refract: hello from the test");
        let idle = win::flush(Duration::from_secs(5));
        let _ = unsafe { SetStdHandle(STD_ERROR_HANDLE, old) };
        wrote.unwrap();
        assert!(idle);
        let text = read(dir.join(LOG_NAME));
        assert!(text.contains("Z refract: Refract 9.9.9 started"), "{text}");
        assert!(text.contains("Z refract: hello from the test\n"), "{text}");
        // The file thread keeps the log open, so the folder is left behind.
    }
}
