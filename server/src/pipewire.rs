use std::collections::HashMap;

use pipewire::permissions::PermissionFlags;

struct Terminate;

#[derive(Clone)]
pub struct NodeInfo {
    pub permissions: PermissionFlags,
    pub props: HashMap<String, String>,
}

pub fn setup_pipewire() {
    pipewire::init();
}
