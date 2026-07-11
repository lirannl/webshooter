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
use tokio::time::{sleep, Duration};

// ---------------------------------------------------------------------------
// Feature-gated inner module
// ---------------------------------------------------------------------------

#[cfg(target_os = "linux")]
mod inner {
    use inputtino::{DeviceDefinition, Keyboard};

    pub struct PortalAuthKb {
        _definition: DeviceDefinition,
        kb: Keyboard,
    }

    impl PortalAuthKb {
        pub fn new(name: &str) -> Option<Self> {
            println!("[portal_auth] creating keyboard");
            let def = DeviceDefinition::new(name, 0xAB, 0xCD, 0xEF, "", "");
            let kb = match Keyboard::new(&def) {
                Ok(kb) => kb,
                Err(e) => {
                    println!("[portal_auth] failed to create keyboard — {e:?}");
                    return None;
                }
            };
            match kb.get_nodes() {
                Ok(nodes) => println!(
                    "[portal_auth] keyboard created at nodes: {:?}",
                    nodes
                ),
                Err(e) => println!(
                    "[portal_auth] keyboard created but get_nodes failed: {e:?}"
                ),
            }
            Some(Self { _definition: def, kb })
        }

        pub fn press_key(&mut self, hid_keycode: i16) {
            println!("[portal_auth] press key 0x{hid_keycode:02X}");
            self.kb.press_key(hid_keycode);
        }

        pub fn release_key(&mut self, hid_keycode: i16) {
            println!("[portal_auth] release key 0x{hid_keycode:02X}");
            self.kb.release_key(hid_keycode);
        }

        pub fn press_enter(&mut self) {
            self.press_key(0x0D_i16);
        }

        pub fn release_enter(&mut self) {
            self.release_key(0x0D_i16);
        }
    }
}

#[cfg(not(target_os = "linux"))]
mod inner {
    pub struct PortalAuthKb;

    impl PortalAuthKb {
        pub fn new(_name: &str) -> Option<Self> {
            None
        }
        #[allow(dead_code)]
        pub fn press_key(&mut self, _hid_keycode: i16) {}
        #[allow(dead_code)]
        pub fn release_key(&mut self, _hid_keycode: i16) {}
        #[allow(dead_code)]
        pub fn press_enter(&mut self) {}
        #[allow(dead_code)]
        pub fn release_enter(&mut self) {}
    }
}

pub use inner::PortalAuthKb;

// ---------------------------------------------------------------------------
// Auto-accept helper
// ---------------------------------------------------------------------------

/// Run `portal_fut` while repeatedly pressing and releasing Enter every
/// ~150 ms.  Returns the portal call's result.
///
/// When no keyboard is available (creation failed or feature disabled)
/// the portal call runs without any key injection.
pub async fn accept_dialog<T>(
    kb: &mut Option<PortalAuthKb>,
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
