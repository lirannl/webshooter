/// Virtual keyboard that auto-accepts XDG Desktop Portal dialogs
/// by simulating Enter via `inputtino`.
///
/// Create a single `PortalAuthKb` at the start of the capture session,
/// then call `accept_dialog` for each portal dialog.  The keyboard is
/// destroyed when it goes out of scope.
use anyhow::{Result, anyhow};
use std::{
    future::Future,
    ops::Deref,
    path::PathBuf,
    sync::{Arc, LazyLock, Mutex},
};
use tokio::fs::File;
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::time::{Duration, sleep, timeout};

use crate::config::CONFIG_DIR;
use crate::keyboard::{self, Keyboard};

static PORTAL_TOKEN_FILE: LazyLock<PathBuf> =
    LazyLock::new(|| CONFIG_DIR.get().unwrap().join("portal_token"));

/// Per-capture restore-token state.  Each concurrent capture owns its own
/// `Arc<Mutex<Option<String>>>` so two clients can't clobber each other's
/// portal session token (the previous implementation shared a single global
/// file, so a second client would overwrite the first's token mid-session).
pub type PortalToken = Arc<Mutex<Option<String>>>;

/// Seed a fresh per-capture token state from the on-disk token written by
/// `setup_pipewire` at startup, so the very first `select_devices` dialog is
/// still skipped.  Each capture then maintains its own copy in memory.
pub async fn load_persisted_portal_token() -> PortalToken {
    PortalToken::new(Mutex::new(get_persisted_portal_token().await))
}

pub fn get_portal_token(state: &PortalToken) -> Option<String> {
    state.lock().unwrap().clone()
}

pub fn set_portal_token(state: &PortalToken, token: String) {
    *state.lock().unwrap() = Some(token);
}

async fn get_persisted_portal_token() -> Option<String> {
    let file = File::open(PORTAL_TOKEN_FILE.deref()).await.ok()?;
    let mut string = String::default();
    let _ = BufReader::new(file).read_to_string(&mut string).await;
    Some(string)
}

/// Read the startup auth token from disk (without wrapping it in capture
/// state).  Used by `setup_pipewire` to seed its own `select_devices` call.
pub async fn read_persisted_portal_token() -> Option<String> {
    get_persisted_portal_token().await
}

/// Persist the startup auth token to disk so subsequent launches (and the
/// initial seed of every capture) can skip the first `select_devices` dialog.
pub async fn persist_portal_token(token: String) {
    if let Ok(mut file) = File::create(PORTAL_TOKEN_FILE.deref()).await {
        let _ = file.write_all(token.as_bytes()).await;
    }
}

// ---------------------------------------------------------------------------
// Auto-accept helper
// ---------------------------------------------------------------------------

/// Run `portal_fut` while repeatedly pressing and releasing Enter every
/// ~150 ms.  Returns the portal call's result.
///
/// When no keyboard is available (creation failed or feature disabled)
/// the portal call runs without any key injection but with a timeout;
/// if the dialog is not approved in time, the application exits.
pub async fn accept_dialog<T>(
    kb: &mut Option<Keyboard>,
    portal_fut: impl Future<Output = Result<T>>,
) -> Result<T> {
    if kb.is_none() {
        println!("[portal_auth] no keyboard — manual approval required (30s timeout)");
        tokio::pin!(portal_fut);
        return tokio::time::timeout(Duration::from_secs(30), portal_fut.as_mut())
            .await
            .map_err(|_| {
                eprintln!("[portal_auth] timed out waiting for manual portal approval");
                std::process::exit(1);
            })?;
    }

    println!("[portal_auth] portal call started");
    tokio::pin!(portal_fut);

    // Give the dialog 300ms to appear and gain keyboard focus, then
    // start pressing Enter.  If the portal completes before the timeout
    // (e.g. the call doesn't show a dialog) we return immediately
    // without injecting anything.
    tokio::select! {
        result = portal_fut.as_mut() => {
            println!("[portal_auth] portal completed before press");
            return result;
        }
        _ = sleep(Duration::from_millis(300)) => {},
    }

    timeout(
        Duration::from_secs(30),
        // Press and release Enter every ~150 ms until the dialog is accepted,
        // so a dialog that is slow to appear or gain focus is still caught.
        // Give up (and exit) after 30s rather than blocking startup forever.
        async {
            loop {
                println!("[portal_auth] pressing Enter");
                if let Some(k) = kb.as_mut() {
                    k.press_key(keyboard::ENTER);
                    sleep(Duration::from_millis(50)).await;
                    k.release_key(keyboard::ENTER);
                }
                tokio::select! {
                    result = portal_fut.as_mut() => {
                        println!("[portal_auth] portal call completed");
                        return result;
                    }
                    _ = sleep(Duration::from_millis(100)) => {},
                }
            }
        },
    )
    .await
    .unwrap_or_else(|err| {
        eprintln!("[portal_auth] timed out waiting for portal approval");
        Err(anyhow!(err))
    })
}
