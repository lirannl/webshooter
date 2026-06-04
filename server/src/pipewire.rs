use std::{
    collections::HashMap,
    os::fd::OwnedFd,
    sync::{mpsc, oneshot},
    thread,
};

use anyhow::Result;
use pipewire::{
    context::ContextBox,
    main_loop::{MainLoopBox, MainLoopRc},
    permissions::PermissionFlags,
};
use tokio::{spawn, sync::Mutex};

use crate::logging::log;

struct Terminate;

#[derive(Clone)]
pub struct NodeInfo {
    pub permissions: PermissionFlags,
    pub props: HashMap<String, String>,
}

pub fn setup_pipewire() {
    pipewire::init();
}

pub fn get_nodes_on_fd(fd: OwnedFd) -> impl FnOnce() -> HashMap<u32, NodeInfo> {
    let (quit, receive_quit) = pipewire::channel::channel();
    let (tx, rx) = mpsc::channel();
    let loop_thread = thread::spawn(move || {
        (|| -> Result<()> {
            let mainloop = MainLoopRc::new(None)?;
            let context = ContextBox::new(mainloop.loop_(), None)?;
            let core = context.connect_fd(fd, None)?;
            let registry = core.get_registry()?;
            let nodeinfo_tx = tx.clone();
            let _ = receive_quit.attach(mainloop.loop_(), {
                let mainloop = mainloop.clone();
                move |_| mainloop.quit()
            });
            let _ = registry.add_listener_local().global(move |global| {
                nodeinfo_tx
                    .send((
                        global.id,
                        NodeInfo {
                            permissions: global.permissions,
                            props: match global.props {
                                Some(props) => props
                                    .iter()
                                    .map(|(k, v)| (k.to_string(), v.to_string()))
                                    .collect(),
                                None => HashMap::new(),
                            },
                        },
                    ))
                    .unwrap();
            });

            mainloop.run();
            drop(tx);
            Ok(())
        })()
        .unwrap_or_else(|err| log(err));
    });

    move || {
        let _ = quit.send(Terminate);
        loop_thread.join().unwrap();
        rx.iter().collect::<HashMap<_, _>>()
    }
}
