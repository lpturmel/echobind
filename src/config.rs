use cpal::SupportedStreamConfig;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Debug)]
pub struct Config {
    pub sample_format: String,
    pub sample_rate: u32,
    pub channels: u16,
    pub buffer_size: BufferSize,
}
impl From<Config> for SupportedStreamConfig {
    fn from(value: Config) -> Self {
        let BufferSize { min, max } = value.buffer_size;
        let buffer_size = cpal::SupportedBufferSize::Range { min, max };
        let sample_format = match value.sample_format.as_str() {
            "i8" => cpal::SampleFormat::I8,
            "i16" => cpal::SampleFormat::I16,
            "i32" => cpal::SampleFormat::I32,
            "i64" => cpal::SampleFormat::I64,
            "f32" => cpal::SampleFormat::F32,
            "f64" => cpal::SampleFormat::F64,
            sample_format => panic!("Invalid sample format {}", sample_format),
        };
        let sample_rate = cpal::SampleRate(value.sample_rate);
        SupportedStreamConfig::new(value.channels, sample_rate, buffer_size, sample_format)
    }
}

#[derive(Serialize, Deserialize, Debug)]
pub struct BufferSize {
    pub min: u32,
    pub max: u32,
}

pub fn count_to_channels(count: u16) -> opus::Channels {
    match count {
        1 => opus::Channels::Mono,
        2 => opus::Channels::Stereo,
        _ => panic!("Unsupported channel count"),
    }
}
