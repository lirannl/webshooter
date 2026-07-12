use anyhow::anyhow;
use ashpd::desktop::{
    CreateSessionOptions, PersistMode,
    remote_desktop::{DeviceType, RemoteDesktop, SelectDevicesOptions, StartOptions},
    screencast::{CursorMode, Screencast, SelectSourcesOptions, SourceType},
};
use ashpd::enumflags2::BitFlags;
use portal_auth::{PORTAL_AUTH_TOKEN, accept_dialog};

use crate::keyboard::Keyboard;

mod portal_auth;
pub mod touch;
pub mod video;

pub async fn setup_pipewire() {
    pipewire::init();

    *PORTAL_AUTH_TOKEN.lock().await = match create_auth_token().await {
        Ok(string) => Some(string),
        Err(err) => {
            eprintln!("{err:#?}");
            None
        }
    };
}

async fn create_auth_token() -> Result<String, anyhow::Error> {
    let mut kb = Keyboard::new("Webshooter Auth Token");

    let remote_desktop = RemoteDesktop::new().await?;

    let session = remote_desktop
        .create_session(CreateSessionOptions::default())
        .await?;

    let select_opts = SelectDevicesOptions::default()
        .set_devices(Some(BitFlags::from(DeviceType::Touchscreen)))
        .set_persist_mode(PersistMode::Application);

    accept_dialog(&mut kb, async {
        remote_desktop
            .select_devices(&session, select_opts)
            .await
            .map_err(Into::into)
    })
    .await?;

    let screencast = Screencast::new().await?;

    accept_dialog(&mut kb, async {
        screencast
            .select_sources(
                &session,
                SelectSourcesOptions::default()
                    .set_multiple(true)
                    .set_sources(Some(
                        BitFlags::from_flag(SourceType::Virtual) | SourceType::Monitor,
                    ))
                    .set_cursor_mode(CursorMode::Embedded),
            )
            .await
            .map_err(Into::into)
    })
    .await?;

    let request = accept_dialog(&mut kb, async {
        remote_desktop
            .start(&session, None, StartOptions::default())
            .await
            .map_err(Into::into)
    })
    .await?;

    let started = request.response()?;

    let token = started.restore_token().ok_or(anyhow!("No token"))?;

    session.close().await?;

    Ok(token.to_string())
}
