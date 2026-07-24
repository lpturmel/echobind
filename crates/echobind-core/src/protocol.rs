use std::{convert::TryInto, fmt};

const MAGIC: &[u8; 4] = b"ECB2";
const HEADER_LEN: usize = MAGIC.len() + 1;
const AUDIO_HEADER_LEN: usize = 12;
const VIDEO_HEADER_LEN: usize = 21;
const CLIPBOARD_HEADER_LEN: usize = 12;
const VIDEO_FLAG_KEYFRAME: u8 = 1;

pub const MAX_DATAGRAM_SIZE: usize = 1200;
pub const MAX_CLIPBOARD_CHUNK_PAYLOAD: usize = 1024;
pub const MAX_AUDIO_FRAME_PAYLOAD: usize = MAX_DATAGRAM_SIZE - HEADER_LEN - AUDIO_HEADER_LEN;
pub const MAX_VIDEO_FRAGMENT_PAYLOAD: usize = MAX_DATAGRAM_SIZE - HEADER_LEN - VIDEO_HEADER_LEN;
pub const MAX_VIDEO_FRAME_SIZE: usize = 8 * 1024 * 1024;
pub const MAX_VIDEO_FRAGMENTS: usize = MAX_VIDEO_FRAME_SIZE.div_ceil(MAX_VIDEO_FRAGMENT_PAYLOAD);

#[derive(Debug, PartialEq, Eq)]
pub enum Packet<'a> {
    Hello,
    Config(&'a [u8]),
    Ping(u64),
    Pong(u64),
    Audio(AudioFrame<'a>),
    Clipboard(ClipboardChunk<'a>),
    Video(VideoFragment<'a>),
    VideoKeyframeRequest,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AudioFrame<'a> {
    pub sequence: u32,
    pub timestamp_us: u64,
    pub payload: &'a [u8],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VideoFragment<'a> {
    pub frame_id: u64,
    pub timestamp_us: u64,
    pub index: u16,
    pub total: u16,
    pub is_keyframe: bool,
    pub payload: &'a [u8],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClipboardChunk<'a> {
    pub transfer_id: u64,
    pub index: u16,
    pub total: u16,
    pub payload: &'a [u8],
}

impl Packet<'_> {
    const HELLO: u8 = 1;
    const CONFIG: u8 = 2;
    const PING: u8 = 3;
    const PONG: u8 = 4;
    const AUDIO: u8 = 5;
    const CLIPBOARD: u8 = 6;
    const VIDEO: u8 = 7;
    const VIDEO_KEYFRAME_REQUEST: u8 = 8;

    pub fn encode(&self, out: &mut Vec<u8>) {
        match self {
            Packet::Hello => Self::encode_packet(Self::HELLO, &[], out),
            Packet::Config(payload) => Self::encode_packet(Self::CONFIG, payload, out),
            Packet::Ping(id) => Self::encode_packet(Self::PING, &id.to_be_bytes(), out),
            Packet::Pong(id) => Self::encode_packet(Self::PONG, &id.to_be_bytes(), out),
            Packet::Audio(frame) => {
                Self::begin_packet(Self::AUDIO, out);
                out.extend_from_slice(&frame.sequence.to_be_bytes());
                out.extend_from_slice(&frame.timestamp_us.to_be_bytes());
                out.extend_from_slice(frame.payload);
            }
            Packet::Clipboard(chunk) => {
                Self::begin_packet(Self::CLIPBOARD, out);
                out.extend_from_slice(&chunk.transfer_id.to_be_bytes());
                out.extend_from_slice(&chunk.index.to_be_bytes());
                out.extend_from_slice(&chunk.total.to_be_bytes());
                out.extend_from_slice(chunk.payload);
            }
            Packet::Video(fragment) => {
                Self::begin_packet(Self::VIDEO, out);
                out.extend_from_slice(&fragment.frame_id.to_be_bytes());
                out.extend_from_slice(&fragment.timestamp_us.to_be_bytes());
                out.extend_from_slice(&fragment.index.to_be_bytes());
                out.extend_from_slice(&fragment.total.to_be_bytes());
                out.push(u8::from(fragment.is_keyframe));
                out.extend_from_slice(fragment.payload);
            }
            Packet::VideoKeyframeRequest => {
                Self::encode_packet(Self::VIDEO_KEYFRAME_REQUEST, &[], out);
            }
        }
    }

    fn begin_packet(kind: u8, out: &mut Vec<u8>) {
        out.clear();
        out.extend_from_slice(MAGIC);
        out.push(kind);
    }

    fn encode_packet(kind: u8, payload: &[u8], out: &mut Vec<u8>) {
        Self::begin_packet(kind, out);
        out.extend_from_slice(payload);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PacketParseError {
    Invalid,
    TooShort,
    TooLarge,
}

impl fmt::Display for PacketParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PacketParseError::Invalid => write!(f, "invalid packet"),
            PacketParseError::TooShort => write!(f, "packet is too short"),
            PacketParseError::TooLarge => write!(f, "packet exceeds protocol limits"),
        }
    }
}

impl std::error::Error for PacketParseError {}

impl<'a> TryFrom<&'a [u8]> for Packet<'a> {
    type Error = PacketParseError;

    fn try_from(data: &'a [u8]) -> Result<Self, Self::Error> {
        if data.len() < HEADER_LEN {
            return Err(PacketParseError::TooShort);
        }
        if &data[..MAGIC.len()] != MAGIC {
            return Err(PacketParseError::Invalid);
        }

        let payload = &data[HEADER_LEN..];
        match data[MAGIC.len()] {
            Packet::HELLO if payload.is_empty() => Ok(Packet::Hello),
            Packet::CONFIG => Ok(Packet::Config(payload)),
            Packet::PING => parse_u64(payload).map(Packet::Ping),
            Packet::PONG => parse_u64(payload).map(Packet::Pong),
            Packet::AUDIO => parse_audio_frame(payload).map(Packet::Audio),
            Packet::CLIPBOARD => parse_clipboard_chunk(payload).map(Packet::Clipboard),
            Packet::VIDEO => parse_video_fragment(payload).map(Packet::Video),
            Packet::VIDEO_KEYFRAME_REQUEST if payload.is_empty() => {
                Ok(Packet::VideoKeyframeRequest)
            }
            _ => Err(PacketParseError::Invalid),
        }
    }
}

fn parse_u64(payload: &[u8]) -> Result<u64, PacketParseError> {
    let bytes: [u8; 8] = payload.try_into().map_err(|_| PacketParseError::Invalid)?;
    Ok(u64::from_be_bytes(bytes))
}

fn parse_audio_frame(payload: &[u8]) -> Result<AudioFrame<'_>, PacketParseError> {
    if payload.len() <= AUDIO_HEADER_LEN {
        return Err(PacketParseError::TooShort);
    }
    if payload.len() - AUDIO_HEADER_LEN > MAX_AUDIO_FRAME_PAYLOAD {
        return Err(PacketParseError::TooLarge);
    }

    Ok(AudioFrame {
        sequence: u32::from_be_bytes(
            payload[..4]
                .try_into()
                .map_err(|_| PacketParseError::Invalid)?,
        ),
        timestamp_us: u64::from_be_bytes(
            payload[4..12]
                .try_into()
                .map_err(|_| PacketParseError::Invalid)?,
        ),
        payload: &payload[AUDIO_HEADER_LEN..],
    })
}

fn parse_video_fragment(payload: &[u8]) -> Result<VideoFragment<'_>, PacketParseError> {
    if payload.len() <= VIDEO_HEADER_LEN {
        return Err(PacketParseError::TooShort);
    }

    let index = u16::from_be_bytes(
        payload[16..18]
            .try_into()
            .map_err(|_| PacketParseError::Invalid)?,
    );
    let total = u16::from_be_bytes(
        payload[18..20]
            .try_into()
            .map_err(|_| PacketParseError::Invalid)?,
    );
    let flags = payload[20];

    if total == 0
        || index >= total
        || total as usize > MAX_VIDEO_FRAGMENTS
        || flags & !VIDEO_FLAG_KEYFRAME != 0
        || payload.len() - VIDEO_HEADER_LEN > MAX_VIDEO_FRAGMENT_PAYLOAD
    {
        return Err(PacketParseError::Invalid);
    }

    Ok(VideoFragment {
        frame_id: u64::from_be_bytes(
            payload[..8]
                .try_into()
                .map_err(|_| PacketParseError::Invalid)?,
        ),
        timestamp_us: u64::from_be_bytes(
            payload[8..16]
                .try_into()
                .map_err(|_| PacketParseError::Invalid)?,
        ),
        index,
        total,
        is_keyframe: flags & VIDEO_FLAG_KEYFRAME != 0,
        payload: &payload[VIDEO_HEADER_LEN..],
    })
}

fn parse_clipboard_chunk(payload: &[u8]) -> Result<ClipboardChunk<'_>, PacketParseError> {
    if payload.len() < CLIPBOARD_HEADER_LEN {
        return Err(PacketParseError::TooShort);
    }

    let transfer_id = u64::from_be_bytes(
        payload[..8]
            .try_into()
            .map_err(|_| PacketParseError::Invalid)?,
    );
    let index = u16::from_be_bytes(
        payload[8..10]
            .try_into()
            .map_err(|_| PacketParseError::Invalid)?,
    );
    let total = u16::from_be_bytes(
        payload[10..12]
            .try_into()
            .map_err(|_| PacketParseError::Invalid)?,
    );

    if total == 0 || index >= total {
        return Err(PacketParseError::Invalid);
    }

    Ok(ClipboardChunk {
        transfer_id,
        index,
        total,
        payload: &payload[CLIPBOARD_HEADER_LEN..],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(packet: Packet<'_>) -> Vec<u8> {
        let mut encoded = Vec::new();
        packet.encode(&mut encoded);
        encoded
    }

    #[test]
    fn encodes_and_parses_ping() {
        let encoded = round_trip(Packet::Ping(42));
        assert_eq!(Packet::try_from(encoded.as_slice()), Ok(Packet::Ping(42)));
    }

    #[test]
    fn rejects_v1_packets() {
        assert_eq!(
            Packet::try_from(&b"ECB1\x01"[..]),
            Err(PacketParseError::Invalid)
        );
    }

    #[test]
    fn encodes_and_parses_audio() {
        let frame = AudioFrame {
            sequence: 7,
            timestamp_us: 140_000,
            payload: &[1, 2, 3],
        };
        let encoded = round_trip(Packet::Audio(frame));
        assert_eq!(
            Packet::try_from(encoded.as_slice()),
            Ok(Packet::Audio(frame))
        );
    }

    #[test]
    fn rejects_oversized_audio_frame() {
        let payload = vec![0; MAX_AUDIO_FRAME_PAYLOAD + 1];
        let frame = AudioFrame {
            sequence: 1,
            timestamp_us: 0,
            payload: &payload,
        };
        let encoded = round_trip(Packet::Audio(frame));
        assert_eq!(
            Packet::try_from(encoded.as_slice()),
            Err(PacketParseError::TooLarge)
        );
    }

    #[test]
    fn encodes_and_parses_video_fragment() {
        let fragment = VideoFragment {
            frame_id: 9,
            timestamp_us: 150_000,
            index: 1,
            total: 3,
            is_keyframe: true,
            payload: &[4, 5, 6],
        };
        let encoded = round_trip(Packet::Video(fragment));
        assert_eq!(
            Packet::try_from(encoded.as_slice()),
            Ok(Packet::Video(fragment))
        );
        assert!(encoded.len() <= MAX_DATAGRAM_SIZE);
    }

    #[test]
    fn rejects_oversized_video_fragment() {
        let payload = vec![0; MAX_VIDEO_FRAGMENT_PAYLOAD + 1];
        let fragment = VideoFragment {
            frame_id: 1,
            timestamp_us: 0,
            index: 0,
            total: 1,
            is_keyframe: false,
            payload: &payload,
        };
        let encoded = round_trip(Packet::Video(fragment));
        assert_eq!(
            Packet::try_from(encoded.as_slice()),
            Err(PacketParseError::Invalid)
        );
    }

    #[test]
    fn encodes_and_parses_clipboard_chunk() {
        let chunk = ClipboardChunk {
            transfer_id: 7,
            index: 1,
            total: 3,
            payload: b"hello",
        };
        let encoded = round_trip(Packet::Clipboard(chunk));
        assert_eq!(
            Packet::try_from(encoded.as_slice()),
            Ok(Packet::Clipboard(chunk))
        );
    }
}
