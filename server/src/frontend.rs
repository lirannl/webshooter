use rust_embed::RustEmbed;

#[derive(RustEmbed)]
#[folder = "../dist"]
pub struct Assets;