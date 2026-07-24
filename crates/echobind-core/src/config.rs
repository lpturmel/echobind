use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio: Option<AudioConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub video: Option<VideoConfig>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AudioConfig {
    pub sample_format: String,
    pub sample_rate: u32,
    pub channels: u16,
    pub buffer_size: BufferSize,
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
    fn audio_only_session_round_trips() {
        let config = SessionConfig {
            audio: Some(AudioConfig {
                sample_format: "f32".to_owned(),
                sample_rate: 48_000,
                channels: 2,
                buffer_size: BufferSize {
                    min: 128,
                    max: 1024,
                },
            }),
            video: None,
        };

        let json = serde_json::to_vec(&config).unwrap();
        assert_eq!(
            serde_json::from_slice::<SessionConfig>(&json).unwrap(),
            config
        );
    }

    #[test]
    fn video_only_session_round_trips() {
        let config = SessionConfig {
            audio: None,
            video: Some(VideoConfig {
                codec: VideoCodec::H264,
                width: 1280,
                height: 720,
                frame_rate: FrameRate {
                    numerator: 60,
                    denominator: 1,
                },
                bitrate_bps: 6_000_000,
            }),
        };

        let json = serde_json::to_vec(&config).unwrap();
        assert_eq!(
            serde_json::from_slice::<SessionConfig>(&json).unwrap(),
            config
        );
    }
}
