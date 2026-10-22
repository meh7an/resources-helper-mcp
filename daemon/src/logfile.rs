//! A log file that rotates by size: `rhd.log` is renamed to `rhd.1.log` once it would grow
//! past the limit, older files move up one number, and the oldest is deleted.

use std::{
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
};

/// Size at which the log rotates.
pub const MAX_LOG_BYTES: u64 = 10 * 1024 * 1024;

/// Log files kept, the current one included.
pub const KEPT_LOG_FILES: usize = 5;

/// An append-only file that rotates once it reaches `max_bytes`.
#[derive(Debug)]
pub struct RotatingFile {
    dir: PathBuf,
    stem: String,
    max_bytes: u64,
    kept: usize,
    file: File,
    written: u64,
}

impl RotatingFile {
    /// Opens `<stem>.log` in `dir` for appending; it rotates before the write that would take
    /// it past `max_bytes`, keeping `kept` files in all.
    ///
    /// # Errors
    ///
    /// Fails when the file cannot be opened or its size read.
    pub fn open(dir: &Path, stem: &str, max_bytes: u64, kept: usize) -> io::Result<Self> {
        let path = numbered(dir, stem, 0);
        let file = open_append(&path)?;
        let written = file.metadata()?.len();
        Ok(Self {
            dir: dir.to_owned(),
            stem: stem.to_owned(),
            max_bytes,
            kept: kept.max(1),
            file,
            written,
        })
    }

    fn path(&self, number: usize) -> PathBuf {
        numbered(&self.dir, &self.stem, number)
    }

    /// Renames the open file, which Windows allows because std opens files with delete
    /// sharing, then starts a new one.
    fn rotate(&mut self) -> io::Result<()> {
        self.file.flush()?;
        match fs::remove_file(self.path(self.kept - 1)) {
            Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error),
            _ => {}
        }
        for number in (0..self.kept - 1).rev() {
            let from = self.path(number);
            if from.exists() {
                fs::rename(&from, self.path(number + 1))?;
            }
        }
        self.file = open_append(&self.path(0))?;
        self.written = 0;
        Ok(())
    }
}

impl Write for RotatingFile {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let incoming = u64::try_from(buf.len()).unwrap_or(u64::MAX);
        if self.written > 0
            && self.written.saturating_add(incoming) > self.max_bytes
            && let Err(error) = self.rotate()
        {
            // Keep logging into whatever file is open (an editor may hold an old one), and
            // try again only after another full file.
            self.file = open_append(&self.path(0))?;
            self.written = 0;
            writeln!(self.file, "log rotation failed: {error}")?;
        }
        let count = self.file.write(buf)?;
        self.written += u64::try_from(count).unwrap_or(u64::MAX);
        Ok(count)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

fn numbered(dir: &Path, stem: &str, number: usize) -> PathBuf {
    match number {
        0 => dir.join(format!("{stem}.log")),
        _ => dir.join(format!("{stem}.{number}.log")),
    }
}

fn open_append(path: &Path) -> io::Result<File> {
    OpenOptions::new().create(true).append(true).open(path)
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    fn read(dir: &Path, name: &str) -> String {
        fs::read_to_string(dir.join(name)).unwrap()
    }

    #[test]
    fn test_rotating_file_shifts_files_and_drops_the_oldest() {
        let dir = TempDir::new().unwrap();
        let mut log = RotatingFile::open(dir.path(), "rhd", 10, 3).unwrap();
        for line in ["first...\n", "second..\n", "third...\n", "fourth..\n"] {
            log.write_all(line.as_bytes()).unwrap();
        }
        log.flush().unwrap();

        assert_eq!(read(dir.path(), "rhd.log"), "fourth..\n");
        assert_eq!(read(dir.path(), "rhd.1.log"), "third...\n");
        assert_eq!(read(dir.path(), "rhd.2.log"), "second..\n");
        assert!(!dir.path().join("rhd.3.log").exists());
    }

    #[test]
    fn test_rotating_file_counts_what_an_earlier_run_wrote() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("rhd.log"), "0123456789").unwrap();
        let mut log = RotatingFile::open(dir.path(), "rhd", 10, 2).unwrap();
        log.write_all(b"new\n").unwrap();
        log.flush().unwrap();

        assert_eq!(read(dir.path(), "rhd.log"), "new\n");
        assert_eq!(read(dir.path(), "rhd.1.log"), "0123456789");
    }

    #[test]
    fn test_rotating_file_never_splits_a_write_larger_than_the_limit() {
        let dir = TempDir::new().unwrap();
        let mut log = RotatingFile::open(dir.path(), "rhd", 4, 2).unwrap();
        log.write_all(b"a long line\n").unwrap();
        log.flush().unwrap();
        assert_eq!(read(dir.path(), "rhd.log"), "a long line\n");
    }
}
