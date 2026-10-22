use std::path::PathBuf;

/// Errors shared by every crate in the workspace.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// The configuration file is missing a value or holds an invalid one.
    #[error("invalid configuration: {0}")]
    Config(String),
    /// The configuration file is not valid TOML, has an unknown key, or has a wrongly typed value.
    #[error("invalid configuration file: {0}")]
    ConfigParse(#[from] toml::de::Error),
    /// A configuration file could not be read or holds an invalid value; names the file so
    /// the user knows what to fix.
    #[error("{}: {reason}", path.display())]
    ConfigFile {
        /// The file that was read.
        path: PathBuf,
        /// What was wrong with it.
        reason: Box<Error>,
    },
    /// Reading system or GPU counters failed.
    #[error("sampling failed: {0}")]
    Sampler(String),
    /// A filesystem or process operation failed.
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// Result alias using the shared [`Error`].
pub type Result<T> = std::result::Result<T, Error>;

#[cfg(test)]
mod tests {
    use std::io;

    use super::*;

    fn fail_with_io() -> Result<()> {
        Err(io::Error::new(io::ErrorKind::NotFound, "token file"))?;
        Ok(())
    }

    #[test]
    fn test_error_from_io_converts_with_question_mark() {
        let error = fail_with_io().unwrap_err();
        assert!(matches!(error, Error::Io(_)));
        assert_eq!(error.to_string(), "token file");
    }

    #[test]
    fn test_error_config_display_includes_reason() {
        let error = Error::Config("heartbeat_ttl must be positive".into());
        assert_eq!(
            error.to_string(),
            "invalid configuration: heartbeat_ttl must be positive"
        );
    }
}
