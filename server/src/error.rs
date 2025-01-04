use std::path::PathBuf;

use http::StatusCode;
use poem::error::ResponseError;
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
    #[error("Failure in encoder \"{0:?}\"")]
    EncoderFailure(ffmpeg_next::codec::Id),
    // #[error("System cryptography is not functioning correctly")]
    // SystemCrypto,
    // #[error("Desktop capture failed")]
    // CaptureFailed,
}

impl ResponseError for WebshooterError {
    fn status(&self) -> StatusCode {
        match &self {
            Self::NotChallenged => StatusCode::FORBIDDEN,
            Self::InvalidLogin => StatusCode::BAD_REQUEST,
            Self::ChallengeFailed => StatusCode::FORBIDDEN,
            Self::NotAuthorized => StatusCode::FORBIDDEN,
            Self::NoAuthentication => StatusCode::UNAUTHORIZED,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}
