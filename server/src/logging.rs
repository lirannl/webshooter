use std::fmt::Debug;

pub fn log<Err: Debug>(err: Err) {
    eprintln!("{err:#?}")
}
