//! The one error type handlers return, and its HTTP mapping.

use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
};
use rh_core::ledger::LedgerError;

/// Everything a handler can fail with.
#[derive(Debug, thiserror::Error)]
pub enum AppError {
    /// The bearer token is missing or wrong.
    #[error(
        "missing or invalid bearer token; clients read it from the token file in \
         %USERPROFILE%\\.resources-helper"
    )]
    Unauthorized,
    /// The lease or resource does not exist.
    #[error("{0}")]
    NotFound(String),
    /// The lease belongs to another session.
    #[error("{0}")]
    Forbidden(String),
    /// The request is well-formed but its values are unusable.
    #[error("{0}")]
    Validation(String),
    /// No machine sample has arrived yet.
    #[error("rhd has just started and has no machine sample yet; retry in a second")]
    Unavailable,
    /// Anything else; logged, never shown to the client.
    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}

impl From<LedgerError> for AppError {
    fn from(error: LedgerError) -> Self {
        match error {
            LedgerError::NotFound(_) => Self::NotFound(error.to_string()),
            LedgerError::NotOwner(_) => Self::Forbidden(error.to_string()),
            _ => Self::Internal(error.into()),
        }
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let status = match &self {
            Self::Unauthorized => StatusCode::UNAUTHORIZED,
            Self::NotFound(_) => StatusCode::NOT_FOUND,
            Self::Forbidden(_) => StatusCode::FORBIDDEN,
            Self::Validation(_) => StatusCode::UNPROCESSABLE_ENTITY,
            Self::Unavailable => StatusCode::SERVICE_UNAVAILABLE,
            Self::Internal(error) => {
                tracing::error!("request failed: {error:#}");
                let message = "internal error; the details are in \
                               %USERPROFILE%\\.resources-helper\\logs\\rhd.log";
                return (StatusCode::INTERNAL_SERVER_ERROR, message).into_response();
            }
        };
        (status, self.to_string()).into_response()
    }
}
