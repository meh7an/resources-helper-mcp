use std::path::PathBuf;

/// Everything a client call can fail with.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ClientError {
    /// No connection to the daemon could be made, so the request was never sent.
    #[error("rhd is not reachable at {url}: {reason}")]
    Unreachable {
        /// The address the client tried.
        url: String,
        /// What the transport reported.
        reason: String,
    },
    /// The request was sent but no answer came back: the daemon stopped, restarted, or took
    /// too long. The request may have taken effect, so it is not repeated.
    #[error(
        "rhd at {url} did not answer ({reason}); it may have stopped or restarted, and the \
         request may still have taken effect"
    )]
    Interrupted {
        /// The address the client used.
        url: String,
        /// What the transport reported.
        reason: String,
    },
    /// The token file does not exist yet, so rhd has never started on this machine.
    #[error("no token at {}; rhd has not run yet, start it with `rh daemon start`", .0.display())]
    NoToken(PathBuf),
    /// The daemon answered with an error status.
    #[error("rhd answered {status}: {message}")]
    Http {
        /// HTTP status code.
        status: u16,
        /// The response body.
        message: String,
    },
    /// The daemon answered with a body the client could not read.
    #[error(
        "unexpected response from rhd: {0}; rh, rh-mcp, and rhd may come from different \
         builds, so install them together"
    )]
    Decode(String),
    /// Starting rhd in the background failed or it never became ready.
    #[error("could not start rhd: {0}")]
    Spawn(String),
    /// The HTTP client or runtime could not be set up.
    #[error("client setup failed: {0}")]
    Setup(String),
    /// Local configuration or files.
    #[error(transparent)]
    Local(#[from] rh_shared::Error),
}

impl ClientError {
    /// `true` when the daemon is not running, not set up, or stopped answering, as opposed to
    /// refusing the request. The hook fails open on these.
    pub fn is_unreachable(&self) -> bool {
        matches!(
            self,
            Self::Unreachable { .. } | Self::Interrupted { .. } | Self::NoToken(_)
        )
    }

    /// `true` when the request never reached a daemon, so starting one and sending it again
    /// cannot apply it twice.
    pub fn is_unsent(&self) -> bool {
        matches!(self, Self::Unreachable { .. } | Self::NoToken(_))
    }

    /// The HTTP status for [`ClientError::Http`].
    pub fn status(&self) -> Option<u16> {
        match self {
            Self::Http { status, .. } => Some(*status),
            _ => None,
        }
    }
}
