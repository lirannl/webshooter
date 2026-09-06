//! Desktop tray icon (Linux/StatusNotifierItem).
//!
//! Provides a system-tray entry whose menu lists one submenu per connected
//! browser client (labelled `{name}-{id}`), each exposing per-client
//! Fullscreen / Release Mouse / Disconnect actions.  The actions are routed
//! through the same per-client controls (`ipc::send_client_control_to`) that
//! the IPC `fullscreen`/`release_mouse` commands use, so only the selected
//! client receives the corresponding `ServerDatagram`.
//!
//! The menu is kept in sync with the connected-client registry: a task
//! subscribes to registry changes and calls `Handle::update` to re-render the
//! menu whenever a client connects or disconnects.
//!
//! The tray is desktop-environment specific, so it is only built on Linux.  On
//! other platforms `setup_tray` is a no-op; the readme tracks Windows/macOS
//! hosting as future work.

#[cfg(target_os = "linux")]
mod imp {
    use std::sync::OnceLock;

    use ksni::{Category, Icon, Tray, TrayMethods, menu::{MenuItem, StandardItem, SubMenu}};
    use shared::server_datagram::ServerDatagram;

    use crate::ipc::send_client_control_to;

    struct WebshooterTray;

    impl Tray for WebshooterTray {
        /// Make the icon itself a menu so a single left click opens the
        /// submenus (GNOME/KDE treat `ItemIsMenu` as "open menu on activate").
        const MENU_ON_ACTIVATE: bool = true;

        fn id(&self) -> String {
            "webshooter".into()
        }

        fn title(&self) -> String {
            "Webshooter".into()
        }

        fn category(&self) -> Category {
            Category::ApplicationStatus
        }

        fn icon_pixmap(&self) -> Vec<Icon> {
            vec![tray_icon()]
        }

        fn menu(&self) -> Vec<MenuItem<Self>> {
            let mut items: Vec<MenuItem<Self>> = Vec::new();

            // One submenu per connected client, labelled "{name}-{id}".
            for (id, name) in crate::ipc::list_clients() {
                items.push(MenuItem::SubMenu(SubMenu {
                    label: format!("{name}-{id}"),
                    submenu: vec![
                        MenuItem::Standard(StandardItem {
                            label: "Fullscreen".into(),
                            activate: Box::new(move |_this| {
                                send_client_control_to(id, ServerDatagram::ToggleFullscreen);
                            }),
                            ..Default::default()
                        }),
                        MenuItem::Standard(StandardItem {
                            label: "Release Mouse".into(),
                            activate: Box::new(move |_this| {
                                send_client_control_to(id, ServerDatagram::ReleaseMouse);
                            }),
                            ..Default::default()
                        }),
                        MenuItem::Standard(StandardItem {
                            label: "Disconnect".into(),
                            activate: Box::new(move |_this| {
                                crate::ipc::disconnect_client(id);
                            }),
                            ..Default::default()
                        }),
                    ],
                    ..Default::default()
                }));
            }

            items.push(MenuItem::Separator);
            items.push(MenuItem::Standard(StandardItem {
                label: "Quit".into(),
                activate: Box::new(|_this| std::process::exit(0)),
                ..Default::default()
            }));

            items
        }
    }

    /// The running tray handle, kept alive for the process lifetime and used
    /// to trigger menu refreshes.  Storing it in a `OnceLock` (rather than
    /// `mem::forget`ting it) is what lets us re-render the menu when the set
    /// of connected clients changes.
    static HANDLE: OnceLock<ksni::Handle<WebshooterTray>> = OnceLock::new();

    /// A small ARGB32 icon so the tray has something to show without shipping a
    /// binary PNG.  Magenta matches the project's `webshooter.svg` branding.
    fn tray_icon() -> Icon {
        const SIZE: i32 = 32;
        let mut data = Vec::with_capacity((SIZE * SIZE) as usize * 4);
        let (cx, cy) = (SIZE as f32 / 2.0, SIZE as f32 / 2.0);
        let max_r = SIZE as f32 / 2.0;
        for y in 0..SIZE {
            for x in 0..SIZE {
                let dx = x as f32 - cx;
                let dy = y as f32 - cy;
                let dist = (dx * dx + dy * dy).sqrt();
                // Soft circular vignette: fully opaque in the disc, fading out.
                let (r, g, b) = (200u8, 40u8, 180u8);
                let alpha = if dist > max_r { 0 } else { 255u8 };
                data.extend_from_slice(&[alpha, r, g, b]);
            }
        }
        Icon {
            width: SIZE,
            height: SIZE,
            data,
        }
    }

    /// A task watching the connected-client registry and re-rendering the menu
    /// whenever the set of sessions changes.
    fn menu_refresher(handle: ksni::Handle<WebshooterTray>) {
        tokio::spawn(async move {
            let mut changes = crate::ipc::subscribe_registry_changes();
            // Refresh once on startup so any already-connected sessions appear.
            loop {
                let _ = changes.changed().await;
                if handle.update(|_| {}).await.is_none() {
                    // The tray service has shut down; stop refreshing.
                    break;
                }
            }
        });
    }

    pub fn setup() {
        tokio::spawn(async move {
            let handle = match WebshooterTray.spawn().await {
                Ok(handle) => handle,
                Err(err) => {
                    log::warn!("Tray icon unavailable: {err:#}");
                    return;
                }
            };
            // Keep the handle alive for the process lifetime so the tray isn't
            // torn down, and drive menu refreshes from the registry.
            let _ = HANDLE.set(handle.clone());
            menu_refresher(handle);
        });
    }
}

#[cfg(target_os = "linux")]
pub fn setup_tray() {
    imp::setup();
}

#[cfg(not(target_os = "linux"))]
pub fn setup_tray() {}
