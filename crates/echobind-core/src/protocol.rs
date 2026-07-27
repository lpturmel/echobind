use std::{borrow::Cow, convert::TryInto, fmt};

const MAGIC: &[u8; 4] = b"ECB2";
const HEADER_LEN: usize = MAGIC.len() + 1;
const AUDIO_HEADER_LEN: usize = 12;
const VIDEO_HEADER_LEN: usize = 21;
const CLIPBOARD_HEADER_LEN: usize = 12;
const VIDEO_FLAG_KEYFRAME: u8 = 1;
const VIDEO_FLAG_RECOVERY: u8 = 1 << 1;
const LEGACY_SERVER_STATS_LEN: usize = 9 * size_of::<f32>() + 7 * size_of::<u64>();
const SERVER_STATS_LEN: usize = 15 * size_of::<f32>() + 9 * size_of::<u64>();
pub const VIDEO_RECOVERY_HEADER_LEN: usize = 4;

// The standard mode keeps the complete IPv6 packet below an Ethernet 1500-byte
// MTU. Jumbo mode is only used after the client advertises support and the host
// explicitly enables it; 8 KiB leaves headroom inside a 9000-byte jumbo frame.
pub const STANDARD_DATAGRAM_SIZE: usize = 1400;
pub const JUMBO_DATAGRAM_SIZE: usize = 8192;
pub const MAX_DATAGRAM_SIZE: usize = JUMBO_DATAGRAM_SIZE;
pub const MAX_CLIPBOARD_CHUNK_PAYLOAD: usize = 1024;
pub const MAX_AUDIO_FRAME_PAYLOAD: usize = STANDARD_DATAGRAM_SIZE - HEADER_LEN - AUDIO_HEADER_LEN;
pub const STANDARD_VIDEO_FRAGMENT_PAYLOAD: usize =
    STANDARD_DATAGRAM_SIZE - HEADER_LEN - VIDEO_HEADER_LEN;
pub const MAX_VIDEO_FRAGMENT_PAYLOAD: usize = MAX_DATAGRAM_SIZE - HEADER_LEN - VIDEO_HEADER_LEN;
pub const MAX_VIDEO_FRAME_SIZE: usize = 8 * 1024 * 1024;
pub const MAX_VIDEO_FRAGMENTS: usize =
    MAX_VIDEO_FRAME_SIZE.div_ceil(STANDARD_VIDEO_FRAGMENT_PAYLOAD - VIDEO_RECOVERY_HEADER_LEN);

#[derive(Debug, PartialEq)]
pub enum Packet<'a> {
    Hello { max_datagram_size: u16 },
    Config(&'a [u8]),
    Ping(u64),
    Pong(u64),
    Audio(AudioFrame<'a>),
    Clipboard(ClipboardChunk<'a>),
    Video(VideoFragment<'a>),
    CursorPosition(CursorPosition),
    ServerStats(ServerStats),
    VideoKeyframeRequest,
    ConnectionRejected(&'a [u8]),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AudioFrame<'a> {
    pub sequence: u32,
    pub timestamp_us: u64,
    pub payload: &'a [u8],
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VideoFragment<'a> {
    pub frame_id: u64,
    pub timestamp_us: u64,
    pub index: u16,
    pub total: u16,
    pub is_keyframe: bool,
    pub is_recovery: bool,
    pub payload: Cow<'a, [u8]>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClipboardChunk<'a> {
    pub transfer_id: u64,
    pub index: u16,
    pub total: u16,
    pub payload: &'a [u8],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CursorPosition {
    pub x: i32,
    pub y: i32,
    pub visible: bool,
}

/// A one-second snapshot of the host's capture and encode pipeline.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ServerStats {
    pub fps: f32,
    pub source_fps: f32,
    pub megabits_per_second: f32,
    pub capture_ms: f32,
    pub gpu_wait_ms: f32,
    pub gpu_lock_ms: f32,
    pub encode_ms: f32,
    pub send_ms: f32,
    pub encode_queue_ms: f32,
    pub copy_wait_ms: f32,
    pub convert_wait_ms: f32,
    pub map_ms: f32,
    pub submit_ms: f32,
    pub completion_wait_ms: f32,
    pub bitstream_ms: f32,
    pub dxgi_timeouts: u64,
    pub dxgi_backlog: u64,
    pub dxgi_backlog_max: u64,
    pub pacing_skips: u64,
    pub slot_busy_skips: u64,
    pub cursor_only_frames: u64,
    pub stale_frames: u64,
    pub preprocess_busy_skips: u64,
    pub no_free_slot_skips: u64,
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
    const CONNECTION_REJECTED: u8 = 9;
    const CURSOR_POSITION: u8 = 10;
    const SERVER_STATS: u8 = 11;

    pub fn encode(&self, out: &mut Vec<u8>) {
        match self {
            Packet::Hello { max_datagram_size } => {
                if usize::from(*max_datagram_size) <= STANDARD_DATAGRAM_SIZE {
                    Self::encode_packet(Self::HELLO, &[], out);
                } else {
                    Self::encode_packet(Self::HELLO, &max_datagram_size.to_be_bytes(), out);
                }
            }
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
                out.push(
                    u8::from(fragment.is_keyframe)
                        | (u8::from(fragment.is_recovery) * VIDEO_FLAG_RECOVERY),
                );
                out.extend_from_slice(fragment.payload.as_ref());
            }
            Packet::CursorPosition(position) => {
                Self::begin_packet(Self::CURSOR_POSITION, out);
                out.extend_from_slice(&position.x.to_be_bytes());
                out.extend_from_slice(&position.y.to_be_bytes());
                out.push(u8::from(position.visible));
            }
            Packet::ServerStats(stats) => {
                Self::begin_packet(Self::SERVER_STATS, out);
                for value in [
                    stats.fps,
                    stats.source_fps,
                    stats.megabits_per_second,
                    stats.capture_ms,
                    stats.gpu_wait_ms,
                    stats.gpu_lock_ms,
                    stats.encode_ms,
                    stats.send_ms,
                    stats.encode_queue_ms,
                ] {
                    out.extend_from_slice(&value.to_bits().to_be_bytes());
                }
                for value in [
                    stats.dxgi_timeouts,
                    stats.dxgi_backlog,
                    stats.dxgi_backlog_max,
                    stats.pacing_skips,
                    stats.slot_busy_skips,
                    stats.cursor_only_frames,
                    stats.stale_frames,
                ] {
                    out.extend_from_slice(&value.to_be_bytes());
                }
                for value in [
                    stats.copy_wait_ms,
                    stats.convert_wait_ms,
                    stats.map_ms,
                    stats.submit_ms,
                    stats.completion_wait_ms,
                    stats.bitstream_ms,
                ] {
                    out.extend_from_slice(&value.to_bits().to_be_bytes());
                }
                for value in [stats.preprocess_busy_skips, stats.no_free_slot_skips] {
                    out.extend_from_slice(&value.to_be_bytes());
                }
            }
            Packet::VideoKeyframeRequest => {
                Self::encode_packet(Self::VIDEO_KEYFRAME_REQUEST, &[], out);
            }
            Packet::ConnectionRejected(reason) => {
                Self::encode_packet(Self::CONNECTION_REJECTED, reason, out);
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
            Packet::HELLO if payload.is_empty() => Ok(Packet::Hello {
                // Legacy peers did not advertise transport capabilities.
                max_datagram_size: STANDARD_DATAGRAM_SIZE as u16,
            }),
            Packet::HELLO if payload.len() == 2 => Ok(Packet::Hello {
                max_datagram_size: u16::from_be_bytes(
                    payload.try_into().map_err(|_| PacketParseError::Invalid)?,
                ),
            }),
            Packet::CONFIG => Ok(Packet::Config(payload)),
            Packet::PING => parse_u64(payload).map(Packet::Ping),
            Packet::PONG => parse_u64(payload).map(Packet::Pong),
            Packet::AUDIO => parse_audio_frame(payload).map(Packet::Audio),
            Packet::CLIPBOARD => parse_clipboard_chunk(payload).map(Packet::Clipboard),
            Packet::VIDEO => parse_video_fragment(payload).map(Packet::Video),
            Packet::CURSOR_POSITION => parse_cursor_position(payload).map(Packet::CursorPosition),
            Packet::SERVER_STATS => parse_server_stats(payload).map(Packet::ServerStats),
            Packet::VIDEO_KEYFRAME_REQUEST if payload.is_empty() => {
                Ok(Packet::VideoKeyframeRequest)
            }
            Packet::CONNECTION_REJECTED => Ok(Packet::ConnectionRejected(payload)),
            _ => Err(PacketParseError::Invalid),
        }
    }
}

fn parse_server_stats(payload: &[u8]) -> Result<ServerStats, PacketParseError> {
    if payload.len() != LEGACY_SERVER_STATS_LEN && payload.len() != SERVER_STATS_LEN {
        return Err(PacketParseError::Invalid);
    }

    let f32_at = |offset: usize| {
        payload
            .get(offset..offset + 4)
            .and_then(|bytes| bytes.try_into().ok())
            .map(u32::from_be_bytes)
            .map(f32::from_bits)
            .ok_or(PacketParseError::Invalid)
    };
    let u64_at = |offset: usize| {
        payload
            .get(offset..offset + 8)
            .and_then(|bytes| bytes.try_into().ok())
            .map(u64::from_be_bytes)
            .ok_or(PacketParseError::Invalid)
    };
    let stats = ServerStats {
        fps: f32_at(0)?,
        source_fps: f32_at(4)?,
        megabits_per_second: f32_at(8)?,
        capture_ms: f32_at(12)?,
        gpu_wait_ms: f32_at(16)?,
        gpu_lock_ms: f32_at(20)?,
        encode_ms: f32_at(24)?,
        send_ms: f32_at(28)?,
        encode_queue_ms: f32_at(32)?,
        dxgi_timeouts: u64_at(36)?,
        dxgi_backlog: u64_at(44)?,
        dxgi_backlog_max: u64_at(52)?,
        pacing_skips: u64_at(60)?,
        slot_busy_skips: u64_at(68)?,
        cursor_only_frames: u64_at(76)?,
        stale_frames: u64_at(84)?,
        copy_wait_ms: 0.0,
        convert_wait_ms: 0.0,
        map_ms: 0.0,
        submit_ms: 0.0,
        completion_wait_ms: 0.0,
        bitstream_ms: 0.0,
        preprocess_busy_skips: 0,
        no_free_slot_skips: 0,
    };
    let stats = if payload.len() == SERVER_STATS_LEN {
        ServerStats {
            copy_wait_ms: f32_at(92)?,
            convert_wait_ms: f32_at(96)?,
            map_ms: f32_at(100)?,
            submit_ms: f32_at(104)?,
            completion_wait_ms: f32_at(108)?,
            bitstream_ms: f32_at(112)?,
            preprocess_busy_skips: u64_at(116)?,
            no_free_slot_skips: u64_at(124)?,
            ..stats
        }
    } else {
        stats
    };
    if [
        stats.fps,
        stats.source_fps,
        stats.megabits_per_second,
        stats.capture_ms,
        stats.gpu_wait_ms,
        stats.gpu_lock_ms,
        stats.encode_ms,
        stats.send_ms,
        stats.encode_queue_ms,
        stats.copy_wait_ms,
        stats.convert_wait_ms,
        stats.map_ms,
        stats.submit_ms,
        stats.completion_wait_ms,
        stats.bitstream_ms,
    ]
    .into_iter()
    .any(|value| !value.is_finite())
    {
        return Err(PacketParseError::Invalid);
    }
    Ok(stats)
}

fn parse_cursor_position(payload: &[u8]) -> Result<CursorPosition, PacketParseError> {
    if payload.len() != 9 || payload[8] > 1 {
        return Err(PacketParseError::Invalid);
    }
    Ok(CursorPosition {
        x: i32::from_be_bytes(
            payload[..4]
                .try_into()
                .map_err(|_| PacketParseError::Invalid)?,
        ),
        y: i32::from_be_bytes(
            payload[4..8]
                .try_into()
                .map_err(|_| PacketParseError::Invalid)?,
        ),
        visible: payload[8] != 0,
    })
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

    let is_recovery = flags & VIDEO_FLAG_RECOVERY != 0;
    if total == 0
        || (!is_recovery && index >= total)
        || (is_recovery && index != total)
        || total as usize > MAX_VIDEO_FRAGMENTS
        || flags & !(VIDEO_FLAG_KEYFRAME | VIDEO_FLAG_RECOVERY) != 0
        || payload.len() - VIDEO_HEADER_LEN > MAX_VIDEO_FRAGMENT_PAYLOAD
        || (is_recovery && payload.len() - VIDEO_HEADER_LEN <= VIDEO_RECOVERY_HEADER_LEN)
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
        is_recovery,
        payload: Cow::Borrowed(&payload[VIDEO_HEADER_LEN..]),
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
    fn hello_advertises_datagram_capability_and_accepts_legacy() {
        let hello = Packet::Hello {
            max_datagram_size: JUMBO_DATAGRAM_SIZE as u16,
        };
        let encoded = round_trip(hello);
        assert_eq!(
            Packet::try_from(encoded.as_slice()),
            Ok(Packet::Hello {
                max_datagram_size: JUMBO_DATAGRAM_SIZE as u16
            })
        );

        let mut legacy = MAGIC.to_vec();
        legacy.push(Packet::HELLO);
        assert_eq!(
            Packet::try_from(legacy.as_slice()),
            Ok(Packet::Hello {
                max_datagram_size: STANDARD_DATAGRAM_SIZE as u16
            })
        );
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
            is_recovery: false,
            payload: Cow::Borrowed(&[4, 5, 6]),
        };
        let encoded = round_trip(Packet::Video(fragment.clone()));
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
            is_recovery: false,
            payload: Cow::Borrowed(&payload),
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

    #[test]
    fn encodes_and_parses_connection_rejection() {
        let encoded = round_trip(Packet::ConnectionRejected(b"busy"));
        assert_eq!(
            Packet::try_from(encoded.as_slice()),
            Ok(Packet::ConnectionRejected(b"busy"))
        );
    }

    #[test]
    fn encodes_and_parses_cursor_position() {
        let position = CursorPosition {
            x: 1234,
            y: -7,
            visible: true,
        };
        let encoded = round_trip(Packet::CursorPosition(position));
        assert_eq!(
            Packet::try_from(encoded.as_slice()),
            Ok(Packet::CursorPosition(position))
        );
    }

    #[test]
    fn encodes_and_parses_server_stats() {
        let stats = ServerStats {
            fps: 119.5,
            source_fps: 120.0,
            megabits_per_second: 87.25,
            capture_ms: 1.2,
            gpu_wait_ms: 0.3,
            gpu_lock_ms: 0.1,
            encode_ms: 2.4,
            send_ms: 0.8,
            encode_queue_ms: 0.2,
            copy_wait_ms: 0.4,
            convert_wait_ms: 0.5,
            map_ms: 0.6,
            submit_ms: 0.7,
            completion_wait_ms: 0.8,
            bitstream_ms: 0.9,
            dxgi_timeouts: 3,
            dxgi_backlog: 4,
            dxgi_backlog_max: 2,
            pacing_skips: 5,
            slot_busy_skips: 6,
            cursor_only_frames: 7,
            stale_frames: 8,
            preprocess_busy_skips: 9,
            no_free_slot_skips: 10,
        };
        let encoded = round_trip(Packet::ServerStats(stats));
        assert_eq!(
            Packet::try_from(encoded.as_slice()),
            Ok(Packet::ServerStats(stats))
        );
    }

    #[test]
    fn parses_legacy_server_stats_without_stage_breakdown() {
        let stats = ServerStats {
            fps: 60.0,
            source_fps: 90.0,
            copy_wait_ms: 4.0,
            preprocess_busy_skips: 12,
            ..ServerStats::default()
        };
        let mut encoded = Vec::new();
        Packet::ServerStats(stats).encode(&mut encoded);
        encoded.truncate(HEADER_LEN + LEGACY_SERVER_STATS_LEN);
        let Packet::ServerStats(parsed) = Packet::try_from(encoded.as_slice()).unwrap() else {
            panic!("legacy server stats changed packet type");
        };
        assert_eq!(parsed.fps, stats.fps);
        assert_eq!(parsed.source_fps, stats.source_fps);
        assert_eq!(parsed.copy_wait_ms, 0.0);
        assert_eq!(parsed.preprocess_busy_skips, 0);
    }

    #[test]
    fn rejects_non_finite_server_stats() {
        let encoded = round_trip(Packet::ServerStats(ServerStats {
            fps: f32::NAN,
            ..ServerStats::default()
        }));
        assert_eq!(
            Packet::try_from(encoded.as_slice()),
            Err(PacketParseError::Invalid)
        );
    }
}
