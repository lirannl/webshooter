use std::error::Error;

use anyhow::Result;
use tokio_util::sync::CancellationToken;

use crate::error::WebshooterError::Cancelled;

pub trait CancellationTokenExt {
    async fn r<F, Ok, Err>(&self, f: F) -> Result<Ok>
    where
        F: Future<Output = Result<Ok, Err>>,
        Err: Error + Sync + Send + 'static;
}

impl CancellationTokenExt for CancellationToken {
    async fn r<F, Ok, Err>(&self, f: F) -> Result<Ok>
    where
        F: Future<Output = Result<Ok, Err>>,
        Err: Error + Sync + Send + 'static,
    {
        match self.run_until_cancelled(f).await {
            Some(res) => Ok(res?),
            None => Err(Cancelled.into()),
        }
    }
}


