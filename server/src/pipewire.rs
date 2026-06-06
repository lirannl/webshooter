use std::{collections::HashMap, os::fd::OwnedFd, sync::mpsc, thread};

use pipewire::{context::ContextBox, main_loop::MainLoopRc, permissions::PermissionFlags};

struct Terminate;

#[derive(Clone)]
pub struct NodeInfo {
    pub permissions: PermissionFlags,
    pub props: HashMap<String, String>,
}

pub fn setup_pipewire() {
    pipewire::init();
}
