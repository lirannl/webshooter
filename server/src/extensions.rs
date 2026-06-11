use std::error::Error;

use anyhow::Result;
use tokio::sync::broadcast::Receiver;
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

pub trait AsyncRecvExt<I> {
    async fn recv_matching<O>(
        &mut self,
        matcher: impl Fn(I) -> Option<O>,
    ) -> Result<O, Box<dyn Error + Send + Sync>>;
}

impl<T> AsyncRecvExt<T> for Receiver<T>
where
    T: Clone,
{
    async fn recv_matching<O>(
        &mut self,
        matcher: impl Fn(T) -> Option<O>,
    ) -> Result<O, Box<dyn Error + Send + Sync>> {
        loop {
            match self.recv().await {
                Err(e) => return Err(e.into()),
                Ok(item) => {
                    if let Some(result) = matcher(item) {
                        return Ok(result);
                    }
                }
            }
        }
    }
}
