/// Virtual keyboard that auto-accepts XDG Desktop Portal dialogs
/// by simulating Enter via `inputtino`.
///
/// Create a single `PortalAuthKb` at the start of the capture session,
/// then call `accept_dialog` for each portal dialog.  The keyboard is
/// destroyed when it goes out of scope.
use anyhow::Result;
use std::future::Future;
use std::ops::Deref;
use std::path::PathBuf;
use std::sync::LazyLock;
use tokio::fs::File;
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::time::{Duration, sleep};

use crate::config::CONFIG_DIR;
use crate::keyboard::Keyboard;

static PORTAL_TOKEN_FILE: LazyLock<PathBuf> =
    LazyLock::new(|| CONFIG_DIR.get().unwrap().join("portal_token"));

pub async fn set_portal_token(token: String) {
    if let Ok(mut file) = File::create(PORTAL_TOKEN_FILE.deref()).await {
        let _ = file.write_all(token.as_bytes()).await;
    }
}
pub async fn get_portal_token() -> Option<String> {
    let file = File::open(PORTAL_TOKEN_FILE.deref()).await.ok()?;
    let mut string = String::default();
    let _ = BufReader::new(file).read_to_string(&mut string).await;
    Some(string)
}

// ---------------------------------------------------------------------------
// Auto-accept helper
// ---------------------------------------------------------------------------

/// Run `portal_fut` while repeatedly pressing and releasing Enter every
/// ~150 ms.  Returns the portal call's result.
///
/// When no keyboard is available (creation failed or feature disabled)
/// the portal call runs without any key injection.
pub async fn accept_dialog<T>(
    kb: &mut Option<Keyboard>,
    portal_fut: impl Future<Output = Result<T>>,
) -> Result<T> {
    #[cfg(target_os = "linux")]
    {
        if kb.is_none() {
            println!("[portal_auth] no keyboard — running portal call without injection");
            return portal_fut.await;
        }

        println!("[portal_auth] portal call started");
        tokio::pin!(portal_fut);

        // Give the dialog 300ms to appear and gain keyboard focus, then
        // press Enter once.  If the portal completes before the timeout
        // (e.g. the call doesn't show a dialog) we return immediately
        // without injecting anything.
        tokio::select! {
            result = portal_fut.as_mut() => {
                println!("[portal_auth] portal completed before press");
                return result;
            }
            _ = sleep(Duration::from_millis(300)) => {},
        }

        println!("[portal_auth] pressing Enter once");
        if let Some(k) = kb.as_mut() {
            k.press_enter();
            sleep(Duration::from_millis(50)).await;
            k.release_enter();
        }

        let result = portal_fut.await;
        println!("[portal_auth] portal call completed");
        result
    }

    #[cfg(not(target_os = "linux"))]
    {
        println!("[portal_auth] not Linux — running portal call without injection");
        let _ = kb;
        portal_fut.await
    }
}
