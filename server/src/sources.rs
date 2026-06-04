#[cfg(target_os = "linux")]
use std::collections::HashMap;

use anyhow::{Result, anyhow};
use ashpd::desktop::{
    PersistMode,
    screencast::{CursorMode, Screencast, SelectSourcesOptions, SourceType},
};
#[cfg(target_os = "linux")]
use ashpd::desktop::{
    Session,
    screencast::{self, OpenPipeWireRemoteOptions},
};
#[cfg(target_os = "linux")]
use pipewire::{self as pw, context::ContextBox};
#[cfg(target_os = "linux")]
use pw::main_loop::MainLoopBox;

#[cfg(target_os = "linux")]
use crate::pipewire::{NodeInfo, get_nodes_on_fd};
use crate::{
    config::{CaptureSource, CaptureType},
    get_config, update_config,
};

/// Opens the XDG ScreenCast portal picker, waits for the user to
/// select a source, captures the restore token, then closes the portal session immediately.
#[cfg(target_os = "linux")]
pub async fn setup_sources() -> Result<()> {
    let screencast = Screencast::new().await?;
    let session = screencast.create_session(Default::default()).await?;

    // Attach screen sources to the same session (Monitor or Virtual displays).
    screencast
        .select_sources(
            &session,
            SelectSourcesOptions::default()
                .set_cursor_mode(CursorMode::Metadata)
                .set_sources(SourceType::Monitor | SourceType::Virtual)
                .set_persist_mode(PersistMode::ExplicitlyRevoked)
                .set_multiple(true),
        )
        .await?;

    // `start()` triggers the compositor's picker UI and returns the restore
    // token once the user confirms. The session is dropped immediately after —
    // we never connect to the PipeWire node, so no capture actually starts.
    let response = screencast
        .start(&session, None, Default::default())
        .await?
        .response()?;

    // let fd = screencast
    //     .open_pipe_wire_remote(&session, OpenPipeWireRemoteOptions::default())
    //     .await?;
    // let get_nodes = get_nodes_on_fd(fd);

    let restore_token = response
        .restore_token()
        .ok_or_else(|| {
            anyhow!(
                "Portal did not return a restore token. \
                 Ensure your portal backend (e.g. gnome-shell ≥ 43) supports PersistMode."
            )
        })?
        .to_string();

    let streams = response.streams();
    if streams.is_empty() {
        return Err(anyhow!("Portal returned no streams"));
    }

    let mut config = get_config().await;
    config.capture_sources.clear();
    // let nodes = get_nodes();
    for stream in streams {
        let capture_type = match stream.source_type() {
            Some(SourceType::Virtual) => CaptureType::Virtual,
            _ => CaptureType::Monitor,
        };

        let name = name_source(stream, &HashMap::new()).await;
        config.capture_sources.push(CaptureSource {
            session_token: restore_token.clone(),
            name: Some(format!(
                "{:?}_{}",
                &capture_type,
                config
                    .capture_sources
                    .iter()
                    .filter(|s| s.name.as_ref() != Some(&name) && s.type_ == capture_type)
                    .count()
                    + 1
            )),
            type_: capture_type,
        });
    }

    update_config(config).await?;
    session.close().await?;
    Ok(())
}

#[cfg(target_os = "linux")]
async fn name_source(source: &screencast::Stream, nodes: &HashMap<u32, NodeInfo>) -> String {
    // let node_info = nodes.get(&source.pipe_wire_node_id()).cloned();

    // node_info
    //     .map(|node_info| {
    //         node_info
    //             .props
    //             .keys()
    //             .map(|s| s.to_string())
    //             .collect::<Vec<_>>()
    //             .join(", ")
    //     })
    // .or_else
    (|| Some(source.id()?.to_string()))().unwrap_or_else(|| source.pipe_wire_node_id().to_string())
}
