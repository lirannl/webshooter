use std::path::PathBuf;

use http::StatusCode;
use salvo::Error;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum WebshooterError {
    #[error("The configuration path \"{0}\" is invalid")]
    InvalidConfigPath(String),
    #[error("Failed to read configuration at \"{0}\". Error:\n{1:#?}")]
    InvalidConfig(PathBuf, anyhow::Error),
    #[error("You have not been challenged yet. Please call /login first, to recieve a challenge")]
    NotChallenged,
    #[error("Login request lacks one of the following: verification key, challenge signature")]
    InvalidLogin,
    #[error("Challenge failed. Cannot authenticate")]
    ChallengeFailed,
    #[error("Cookie not permitted")]
    NotAuthorized,
    #[error("Webshooter isn't prepared to accept IPC connections yet")]
    IPCNotAvailable,
    #[error("Missing authentication")]
    NoAuthentication,
    #[error("Cancelled")]
    Cancelled,
    #[error("Internal server error")]
    InternalServerError,
}

impl WebshooterError {
    pub fn status(&self) -> StatusCode {
        match &self {
            Self::NotChallenged => StatusCode::FORBIDDEN,
            Self::InvalidLogin => StatusCode::BAD_REQUEST,
            Self::ChallengeFailed => StatusCode::FORBIDDEN,
            Self::NotAuthorized => StatusCode::FORBIDDEN,
            Self::NoAuthentication => StatusCode::UNAUTHORIZED,
            Self::InternalServerError => StatusCode::INTERNAL_SERVER_ERROR,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}

impl From<WebshooterError> for Error {
    fn from(err: WebshooterError) -> Self {
        let code = err.status();
        let status_error = salvo::http::StatusError::from_code(code)
            .unwrap_or_else(salvo::http::StatusError::internal_server_error)
            .brief(err.to_string());
        Error::from(status_error)
    }
}
