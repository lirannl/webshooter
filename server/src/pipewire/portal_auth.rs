/// Virtual keyboard that auto-accepts XDG Desktop Portal dialogs
/// by simulating Enter via `inputtino`.
///
/// Create a single `PortalAuthKb` at the start of the capture session,
/// then call `accept_dialog` for each portal dialog.  The keyboard is
/// destroyed when it goes out of scope.
///
/// On non-Linux platforms all methods are no-ops.
use anyhow::Result;
use std::future::Future;
#[cfg(target_os = "linux")]
use std::sync::LazyLock;
#[cfg(target_os = "linux")]
use tokio::sync::Mutex;
use tokio::time::{Duration, sleep};

use crate::keyboard::Keyboard;

#[cfg(target_os = "linux")]
pub static PORTAL_AUTH_TOKEN: LazyLock<Mutex<Option<String>>> = Default::default();

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
