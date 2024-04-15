use std::path::PathBuf;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum WebshooterError {
    #[error("The configuration path \"{0}\" is invalid")]
    InvalidConfigPath(String),
    #[error("Failed to read configuration at \"{0}\". Error:\n{1:#?}")]
    InvalidConfig(PathBuf, anyhow::Error),
}
