pub use echobind_core::config::{AudioConfig, BufferSize, SessionConfig};

pub fn count_to_channels(count: u16) -> opus::Channels {
    match count {
        1 => opus::Channels::Mono,
        2 => opus::Channels::Stereo,
        _ => panic!("Unsupported channel count"),
    }
}
