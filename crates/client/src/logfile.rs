//! A size-capped log file for the service, which has no console.
//!
//! `client.log` grows to the cap, then becomes `client.log.1` (replacing
//! the previous one) and a new `client.log` is started: at most about twice
//! the cap on disk. Rotation happens between log events, never inside one.

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use tracing_subscriber::fmt::MakeWriter;

pub const LOG_FILE: &str = "client.log";
/// 5 MiB per file.
pub const DEFAULT_CAP: u64 = 5 * 1024 * 1024;

pub struct RotatingFile {
    state: Mutex<State>,
}

struct State {
    path: PathBuf,
    cap: u64,
    /// `None` only if reopening after a rotation failed; writes are then
    /// dropped until the next rotation attempt succeeds.
    file: Option<File>,
    size: u64,
}

fn open_append(path: &Path) -> io::Result<File> {
    OpenOptions::new().create(true).append(true).open(path)
}

impl RotatingFile {
    /// Appends to `path`, rotating once it holds `cap` bytes.
    pub fn open(path: impl Into<PathBuf>, cap: u64) -> io::Result<RotatingFile> {
        let path = path.into();
        let file = open_append(&path)?;
        let size = file.metadata()?.len();
        Ok(RotatingFile { state: Mutex::new(State { path, cap, file: Some(file), size }) })
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl State {
    fn rotate(&mut self) {
        // Closed first: an open file cannot be renamed on Windows.
        self.file = None;
        let mut rotated = self.path.clone().into_os_string();
        rotated.push(".1");
        // Replaces the previous `.1`. Should that fail, the current file is
        // started over instead, so the cap still holds.
        let reopened = match std::fs::rename(&self.path, PathBuf::from(rotated)) {
            Ok(()) => open_append(&self.path),
            Err(_) => File::create(&self.path),
        };
        self.file = reopened.ok();
        self.size = 0;
    }
}

/// Writes one log event; holds the file for the whole event.
pub struct Writer<'a>(MutexGuard<'a, State>);

impl Write for Writer<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let state = &mut *self.0;
        match &mut state.file {
            Some(file) => {
                let written = file.write(buf)?;
                state.size += written as u64;
                Ok(written)
            }
            None => Ok(buf.len()),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match &mut self.0.file {
            Some(file) => file.flush(),
            None => Ok(()),
        }
    }
}

impl<'a> MakeWriter<'a> for RotatingFile {
    type Writer = Writer<'a>;

    fn make_writer(&'a self) -> Writer<'a> {
        let mut state = self.lock();
        if state.size >= state.cap || state.file.is_none() {
            state.rotate();
        }
        Writer(state)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn log(file: &RotatingFile, line: &str) {
        file.make_writer().write_all(line.as_bytes()).unwrap();
    }

    fn read(path: &Path) -> String {
        std::fs::read_to_string(path).unwrap_or_default()
    }

    #[test]
    fn rotates_to_one_old_file_once_the_cap_is_reached() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(LOG_FILE);
        let old = dir.path().join("client.log.1");
        let file = RotatingFile::open(&path, 20).unwrap();

        log(&file, "first event 0123\n"); // 17 bytes
        log(&file, "second event\n"); // over the cap, but events stay whole
        assert_eq!(read(&path), "first event 0123\nsecond event\n");
        assert!(!old.exists());

        log(&file, "third\n");
        assert_eq!(read(&old), "first event 0123\nsecond event\n");
        assert_eq!(read(&path), "third\n");

        log(&file, "fourth event that is long\n");
        log(&file, "fifth\n");
        // Only one old file is kept.
        assert_eq!(read(&old), "third\nfourth event that is long\n");
        assert_eq!(read(&path), "fifth\n");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 2);
    }

    #[test]
    fn appends_to_an_existing_log_and_counts_what_is_there() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(LOG_FILE);
        std::fs::write(&path, "from an earlier run that filled the file\n").unwrap();
        let file = RotatingFile::open(&path, 20).unwrap();
        log(&file, "new\n");
        assert_eq!(read(&path), "new\n");
        assert_eq!(read(&dir.path().join("client.log.1")), "from an earlier run that filled the file\n");
    }

    #[test]
    fn works_as_the_writer_of_a_subscriber() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(LOG_FILE);
        let subscriber = tracing_subscriber::fmt()
            .with_writer(RotatingFile::open(&path, DEFAULT_CAP).unwrap())
            .with_ansi(false)
            .finish();
        tracing::subscriber::with_default(subscriber, || tracing::info!(answer = 42, "logged to the file"));
        let text = read(&path);
        assert!(text.contains("logged to the file answer=42"), "{text}");
    }
}
