//! One-time setup before the daemon serves: the single-instance lock, the bearer token, and
//! logging.

use std::{
    fs::{self, File, OpenOptions, TryLockError},
    io,
    path::Path,
    sync::Mutex,
};

use anyhow::{Context, anyhow, bail};

use crate::logfile::{KEPT_LOG_FILES, MAX_LOG_BYTES, RotatingFile};

const TOKEN_BYTES: usize = 32;
const HEX_DIGITS: &[u8; 16] = b"0123456789abcdef";

/// Holds the daemon's lock file for as long as it lives; dropping it releases the lock.
#[derive(Debug)]
pub struct InstanceLock {
    _file: File,
}

impl InstanceLock {
    /// Takes the exclusive lock on `path`, creating the file when missing.
    ///
    /// # Errors
    ///
    /// Fails when another daemon holds the lock or the file cannot be opened.
    pub fn acquire(path: &Path) -> anyhow::Result<Self> {
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(path)
            .with_context(|| format!("opening lock file {}", path.display()))?;
        match file.try_lock() {
            Ok(()) => Ok(Self { _file: file }),
            Err(TryLockError::WouldBlock) => {
                bail!(
                    "another rhd is already running (it holds {})",
                    path.display()
                )
            }
            Err(TryLockError::Error(error)) => {
                Err(error).with_context(|| format!("locking {}", path.display()))
            }
        }
    }
}

/// Reads the bearer token from `path`, or generates a random one and writes it there.
///
/// # Errors
///
/// Fails when the file cannot be read or written, or the OS has no randomness to offer.
pub fn load_or_create_token(path: &Path) -> anyhow::Result<String> {
    match fs::read_to_string(path) {
        Ok(token) if !token.trim().is_empty() => return Ok(token.trim().to_owned()),
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).with_context(|| format!("reading {}", path.display())),
    }
    let mut bytes = [0u8; TOKEN_BYTES];
    getrandom::fill(&mut bytes).map_err(|error| anyhow!("generating the token: {error}"))?;
    let token: String = bytes
        .iter()
        .flat_map(|byte| {
            [
                HEX_DIGITS[usize::from(byte >> 4)],
                HEX_DIGITS[usize::from(byte & 0x0f)],
            ]
        })
        .map(char::from)
        .collect();
    fs::write(path, &token).with_context(|| format!("writing {}", path.display()))?;
    Ok(token)
}

/// Sends `tracing` output at INFO and above to `rhd.log` in `log_dir`, which rotates at
/// [`MAX_LOG_BYTES`] and keeps [`KEPT_LOG_FILES`] files.
///
/// # Errors
///
/// Fails when the log file cannot be opened or a global subscriber is already set.
pub fn init_logging(log_dir: &Path) -> anyhow::Result<()> {
    fs::create_dir_all(log_dir).with_context(|| format!("creating {}", log_dir.display()))?;
    let file = RotatingFile::open(log_dir, "rhd", MAX_LOG_BYTES, KEPT_LOG_FILES)
        .with_context(|| format!("opening rhd.log in {}", log_dir.display()))?;
    tracing_subscriber::fmt()
        .with_writer(Mutex::new(file))
        .with_ansi(false)
        .with_max_level(tracing::Level::INFO)
        .try_init()
        .map_err(|error| anyhow!("initializing logging: {error}"))
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    #[test]
    fn test_instance_lock_second_acquire_refuses_until_first_is_dropped() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("rhd.lock");
        let first = InstanceLock::acquire(&path).unwrap();

        let second = InstanceLock::acquire(&path).unwrap_err();
        assert!(second.to_string().contains("already running"), "{second}");

        drop(first);
        InstanceLock::acquire(&path).unwrap();
    }

    #[test]
    fn test_token_is_generated_once_then_reused() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("token");
        let token = load_or_create_token(&path).unwrap();
        assert_eq!(token.len(), TOKEN_BYTES * 2);
        assert!(token.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(load_or_create_token(&path).unwrap(), token);
    }

    #[test]
    fn test_token_blank_file_is_replaced() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("token");
        fs::write(&path, "  \n").unwrap();
        assert_eq!(load_or_create_token(&path).unwrap().len(), TOKEN_BYTES * 2);
    }
}
