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

        pub fn press_enter(&mut self) {
            println!("[portal_auth] press KEY_ENTER");
            self.kb.press_key(0x0D);
        }

        pub fn release_enter(&mut self) {
            println!("[portal_auth] release KEY_ENTER");
            self.kb.release_key(0x0D);
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
        use tokio::time::Duration;

        if kb.is_none() {
            println!("[portal_auth] no keyboard — running portal call without injection");
            return portal_fut.await;
        }

        println!("[portal_auth] starting press loop concurrent with portal call");
        let press = async {
            loop {
                if kb.is_some() {
                    kb.as_mut().map(|k| k.press_enter());
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    kb.as_mut().map(|k| k.release_enter());
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        };
        tokio::pin!(press);

        tokio::select! {
            result = portal_fut => {
                println!("[portal_auth] portal call completed");
                result
            },
            _ = &mut press => unreachable!(),
        }
    }

    #[cfg(not(target_os = "linux"))]
    {
        println!("[portal_auth] not Linux — running portal call without injection");
        let _ = kb;
        portal_fut.await
    }
}
