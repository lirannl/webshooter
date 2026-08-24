use futures_util::{Stream, StreamExt};
use notify::{EventKind, Watcher};
use poem::listener::{RustlsCertificate, RustlsConfig};
use std::{
    path::{Path, PathBuf},
    time::Duration,
};
use x509_parser::prelude::*;

/// Watches cert and key PEM files on disk and yields a new [`RustlsConfig`]
/// whenever either file changes. The initial item is emitted immediately so
/// the TLS listener can start accepting connections right away.
///
/// Subsequent updates are debounced so that atomic writes (temp + rename)
/// don't cause redundant reloads.
pub fn watch_cert_files(
    cert_path: PathBuf,
    key_path: PathBuf,
) -> impl Stream<Item = RustlsConfig> + Send + 'static {
    let (tx, rx) = async_channel::bounded::<()>(1);

    let watch_cert = cert_path.clone();
    let watch_key = key_path.clone();
    std::thread::Builder::new()
        .name("cert-watcher".into())
        .spawn(move || {
            let mut watcher = notify::recommended_watcher(move |res: Result<notify::Event, notify::Error>| {
                if let Ok(event) = res {
                    if matches!(event.kind, EventKind::Modify(_)) {
                        let _ = tx.clone().try_send(());
                    }
                }
            })
            .expect("failed to create cert file watcher");

            if let Err(e) = watcher.watch(&watch_cert, notify::RecursiveMode::NonRecursive) {
                eprintln!("cert watcher: failed to watch {watch_cert:?}: {e}");
                return;
            }
            if let Err(e) = watcher.watch(&watch_key, notify::RecursiveMode::NonRecursive) {
                eprintln!("cert watcher: failed to watch {watch_key:?}: {e}");
                return;
            }

            // Keep the OS thread (and watcher) alive for the process lifetime.
            loop {
                std::thread::sleep(Duration::from_secs(3600));
            }
        })
        .expect("failed to spawn cert-watcher thread");

    let initial_cert = cert_path.clone();
    let initial_key = key_path.clone();

    // Emit the initial config immediately, then reload on each file-change event.
    futures_util::stream::once(async move {
        load_config_fallible(&initial_cert, &initial_key)
            .await
            .expect("failed to load initial TLS certificates")
    })
    .chain(
        futures_util::stream::unfold(rx, move |rx| {
        let cert = cert_path.clone();
        let key = key_path.clone();
        async move {
            match rx.recv().await {
                Ok(()) => {
                    // Debounce: atomic writes may fire multiple fs events in quick succession.
                    tokio::time::sleep(Duration::from_secs(4)).await;
                    let config = load_config(&cert, &key).await;
                    Some((config, rx))
                }
                Err(_) => None,
            }
        }
    }),
    )
}

async fn load_config(cert_path: &Path, key_path: &Path) -> RustlsConfig {
    match load_config_fallible(cert_path, key_path).await {
        Ok(config) => config,
        Err(e) => {
            eprintln!("cert watcher: failed to reload certs: {e}");
            // Return an empty config — poem will log an error and keep the
            // previous TlsAcceptor, so existing connections stay alive.
            RustlsConfig::new()
        }
    }
}

async fn load_config_fallible(cert_path: &Path, key_path: &Path) -> anyhow::Result<RustlsConfig> {
    let cert = tokio::fs::read(cert_path).await?;
    let key = tokio::fs::read(key_path).await?;
    Ok(RustlsConfig::new().fallback(
        RustlsCertificate::new().cert(cert).key(key),
    ))
}

pub fn sans_from_cert(cert_path: &Path) -> anyhow::Result<Vec<String>> {
    let pem_bytes = std::fs::read(cert_path)?;
    let pem = x509_parser::pem::Pem::read(std::io::Cursor::new(&pem_bytes))?.0;
    let cert = pem.parse_x509()?;
    let mut sans = Vec::new();
    if let Some(ext) = cert.subject_alternative_name()? {
        for name in &ext.value.general_names {
            match name {
                GeneralName::DNSName(dns) => sans.push(dns.to_string()),
                GeneralName::IPAddress(ip) => match ip.len() {
                    4 => {
                        let mut octets = [0u8; 4];
                        octets.copy_from_slice(ip);
                        sans.push(std::net::Ipv4Addr::from(octets).to_string());
                    }
                    16 => {
                        let mut octets = [0u8; 16];
                        octets.copy_from_slice(ip);
                        sans.push(std::net::Ipv6Addr::from(octets).to_string());
                    }
                    _ => {}
                },
                _ => {}
            }
        }
    }
    Ok(sans)
}
