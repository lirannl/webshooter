use anyhow::Result;
use notify::{EventKind, Watcher};
use std::{
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::{spawn, sync::Mutex, task::JoinHandle, time::sleep};
use crate::config::Config;

use crate::APP_CONFIG;

pub async fn watch_config(file: &Path) {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();

    let file_clone = file.to_owned();
    std::thread::spawn(move || {
        let mut watcher = notify::recommended_watcher(move |res| match res {
            Ok(event) => {
                let _ = tx.send(event);
            }
            Err(e) => eprintln!("watch error: {:?}", e),
        })?;
        watcher.watch(&file_clone, notify::RecursiveMode::NonRecursive)?;
        Ok::<_, anyhow::Error>(())
    });

    let file = file.to_owned();
    spawn(async move {
        let future_execution = Mutex::new(None::<JoinHandle<Result<()>>>);
        while let Some(event) = rx.recv().await {
            match event.kind {
                EventKind::Modify(_) => {
                    if let Some(future_execution) = future_execution.lock().await.take() {
                        future_execution.abort();
                    }
                    *future_execution.lock().await = Some(watch_respond(file.to_owned()));
                }
                _ => {}
            }
        }
    });
}

pub fn watch_respond(file: PathBuf) -> JoinHandle<Result<()>> {
    spawn(async move {
        sleep(Duration::from_secs(4)).await;
        let string = tokio::fs::read_to_string(file).await?;
        if let Ok(config) = toml::from_str::<Config>(&string)
            .or_else(|_| serde_yaml::from_str(&string))
            .or_else(|_| serde_json::from_str(&string))
        {
            *APP_CONFIG.lock().await = Some(config);
        }
        Ok(())
    })
}
