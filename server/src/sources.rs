use crate::{config::CaptureSource, get_config, update_config};
use anyhow::{Result, anyhow};
use ashpd::desktop::{
    CreateSessionOptions, PersistMode,
    remote_desktop::{DeviceType, RemoteDesktop, SelectDevicesOptions, StartOptions},
    screencast::{CursorMode, Screencast, SelectSourcesOptions, SourceType},
};
use ashpd::enumflags2::BitFlags;

/// Opens the XDG RemoteDesktop+ScreenCast portal picker, waits for the user to
/// select a source, captures the restore token, then closes the portal session
/// immediately.  The token is compatible with the RemoteDesktop session used
/// during actual capture (video.rs), so future sessions skip the picker.
#[cfg(target_os = "linux")]
pub async fn setup_sources() -> Result<()> {
    let remote_desktop = RemoteDesktop::new().await?;
    let screencast = Screencast::new().await?;
    let session = remote_desktop
        .create_session(CreateSessionOptions::default())
        .await?;

    // Persist the device selection so the token returned from Start can be
    // reused to skip the picker.  No prior token — this is the setup flow.
    remote_desktop
        .select_devices(
            &session,
            SelectDevicesOptions::default()
                .set_devices(Some(BitFlags::from(DeviceType::Touchscreen)))
                .set_persist_mode(Some(PersistMode::ExplicitlyRevoked)),
        )
        .await?;

    // Single monitor source, matching what the capture session requests.
    screencast
        .select_sources(
            &session,
            SelectSourcesOptions::default()
                .set_cursor_mode(CursorMode::Metadata)
                .set_sources(Some(BitFlags::from(SourceType::Monitor))),
        )
        .await?;

    // `start()` triggers the compositor's picker UI and returns the restore
    // token once the user confirms.  The session is dropped immediately after —
    // we never open the PipeWire remote, so no capture actually starts.
    let response = remote_desktop
        .start(&session, None, StartOptions::default())
        .await?
        .response()?;

    let restore_token = response
        .restore_token()
        .ok_or_else(|| {
            anyhow!(
                "Portal did not return a restore token. \
                 Ensure your portal backend (e.g. gnome-shell ≥ 43, KWin ≥ 5.27) \
                 supports PersistMode on RemoteDesktop sessions."
            )
        })?
        .to_string();

    let mut config = get_config().await;
    config.capture_sources = vec![CaptureSource {
        session_token: restore_token,
    }];
    update_config(config).await?;

    session.close().await?;
    Ok(())
}
