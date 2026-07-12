use anyhow::anyhow;
use ashpd::desktop::{
    CreateSessionOptions, PersistMode,
    remote_desktop::{DeviceType, RemoteDesktop, SelectDevicesOptions, StartOptions},
    screencast::{CursorMode, Screencast, SelectSourcesOptions, SourceType},
};
use ashpd::enumflags2::BitFlags;
use portal_auth::accept_dialog;

use crate::{
    keyboard::Keyboard,
    pipewire::portal_auth::{get_portal_token, set_portal_token},
};

mod eis;
mod eis_keyboard;
mod portal_auth;
pub mod touch;
pub mod video;

pub async fn setup_pipewire() {
    pipewire::init();

    match create_auth_token().await {
        Ok(string) => set_portal_token(string).await,
        Err(err) => {
            eprintln!("{err:#?}");
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
        .set_devices(Some(BitFlags::from(
            DeviceType::Touchscreen | DeviceType::Pointer | DeviceType::Keyboard,
        )))
        .set_restore_token(get_portal_token().await.as_deref())
        .set_persist_mode(PersistMode::ExplicitlyRevoked);

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
