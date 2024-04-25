use std::path::PathBuf;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum WebshooterError {
    #[error("The configuration path \"{0}\" is invalid")]
    InvalidConfigPath(String),
    #[error("Failed to read configuration at \"{0}\". Error:\n{1:#?}")]
    InvalidConfig(PathBuf, anyhow::Error),
    #[error("You have not been challenged yet. Please call /login first, to recieve a challenge")]
    NotChallenged,
    #[error("Connections to webshooter must provide a public key for authentication")]
    MissingPubkey,
}
