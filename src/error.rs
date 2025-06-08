pub(crate) type Result<T> = std::result::Result<T, Error>;

#[derive(Debug)]
pub enum Error {
    Io(std::io::Error),
    Config(cpal::DefaultStreamConfigError),
    BuildStream(cpal::BuildStreamError),
    PlayStream(cpal::PlayStreamError),
    SupportedConfigs(cpal::SupportedStreamConfigsError),
    Json(serde_json::Error),
    NoAudioDevice,
    UnsupportedSampleFormat(cpal::SampleFormat),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Io(e) => write!(f, "IO error: {}", e),
            Error::Config(e) => write!(f, "Failed to get default config: {}", e),
            Error::BuildStream(e) => write!(f, "Failed to build stream: {}", e),
            Error::PlayStream(e) => write!(f, "Failed to play stream: {}", e),
            Error::SupportedConfigs(e) => write!(f, "Failed to get supported configs: {}", e),
            Error::Json(e) => write!(f, "Failed to parse JSON: {}", e),
            Error::NoAudioDevice => write!(f, "No audio device found"),
            Error::UnsupportedSampleFormat(e) => {
                write!(f, "Unsupported sample format: {:?}", e)
            }
        }
    }
}

impl From<cpal::SupportedStreamConfigsError> for Error {
    fn from(err: cpal::SupportedStreamConfigsError) -> Self {
        Error::SupportedConfigs(err)
    }
}

impl From<std::io::Error> for Error {
    fn from(err: std::io::Error) -> Self {
        Error::Io(err)
    }
}

impl From<serde_json::Error> for Error {
    fn from(value: serde_json::Error) -> Self {
        Error::Json(value)
    }
}

impl From<cpal::DefaultStreamConfigError> for Error {
    fn from(err: cpal::DefaultStreamConfigError) -> Self {
        Error::Config(err)
    }
}

impl From<cpal::BuildStreamError> for Error {
    fn from(err: cpal::BuildStreamError) -> Self {
        Error::BuildStream(err)
    }
}

impl From<cpal::PlayStreamError> for Error {
    fn from(err: cpal::PlayStreamError) -> Self {
        Error::PlayStream(err)
    }
}
