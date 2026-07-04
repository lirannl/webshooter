pub mod touch;
pub mod video;

pub fn setup_pipewire() {
    pipewire::init();
}
