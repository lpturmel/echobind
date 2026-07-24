use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Config {
    pub sample_format: String,
    pub sample_rate: u32,
    pub channels: u16,
    pub buffer_size: BufferSize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub video: Option<VideoConfig>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BufferSize {
    pub min: u32,
    pub max: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VideoConfig {
    pub codec: VideoCodec,
    pub width: u32,
    pub height: u32,
    pub frame_rate: FrameRate,
    pub bitrate_bps: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VideoCodec {
    H264,
    H265,
    Av1,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrameRate {
    pub numerator: u32,
    pub denominator: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_audio_config_deserializes_without_video() {
        let json = r#"{
            "sample_format":"f32",
            "sample_rate":48000,
            "channels":2,
            "buffer_size":{"min":128,"max":1024}
        }"#;

        let config: Config = serde_json::from_str(json).unwrap();
        assert_eq!(config.video, None);
    }
}
