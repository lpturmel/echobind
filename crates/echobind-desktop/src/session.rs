use crate::video::I420Frame;
use echobind_core::{
    protocol::{Packet, MAX_DATAGRAM_SIZE},
    video::{fragment_video_frame, VideoReassembler},
    AudioConfig, FrameRate as SessionFrameRate, SessionConfig, VideoCodec, VideoConfig,
};
use openh264::{
    decoder::Decoder,
    encoder::{
        BitRate, Complexity, Encoder, EncoderConfig, FrameRate as EncoderFrameRate,
        FrameType as EncodedFrameType, IntraFramePeriod, RateControlMode, UsageType,
    },
    formats::YUVSource,
    OpenH264API,
};
use scap::{
    capturer::{Capturer, Options, Resolution},
    frame::{BGRAFrame, Frame, FrameType, YUVFrame},
};
use std::{
    net::{SocketAddr, UdpSocket},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc, Condvar, Mutex,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
use tracing::{debug, warn};

#[path = "audio.rs"]
mod audio;

#[cfg(target_os = "macos")]
#[path = "hardware_macos.rs"]
mod hardware_macos;

#[cfg(target_os = "windows")]
#[path = "hardware_windows.rs"]
mod hardware_windows;

const CONNECTION_TIMEOUT: Duration = Duration::from_secs(3);
const SOCKET_TIMEOUT: Duration = Duration::from_millis(20);
const VIDEO_REASSEMBLY_AGE: Duration = Duration::from_millis(120);
const HELLO_INTERVAL: Duration = Duration::from_millis(400);
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VideoResolution {
    Native,
    P720,
    P1080,
}

impl VideoResolution {
    pub const fn dimensions(self) -> Option<(u32, u32)> {
        match self {
            Self::Native => None,
            Self::P720 => Some((1280, 720)),
            Self::P1080 => Some((1920, 1080)),
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::Native => "Native",
            Self::P720 => "720p",
            Self::P1080 => "1080p",
        }
    }

    fn capture_resolution(self) -> Resolution {
        match self {
            Self::Native => Resolution::Captured,
            Self::P720 => Resolution::_720p,
            Self::P1080 => Resolution::_1080p,
        }
    }
}

enum CapturedFrame {
    Nv12(YUVFrame),
    Bgra(BGRAFrame),
}

impl CapturedFrame {
    fn update_i420(
        &self,
        destination: &mut I420Frame,
        resolution: VideoResolution,
    ) -> Result<(), String> {
        match self {
            Self::Nv12(frame) => destination.update_from_nv12(frame),
            Self::Bgra(frame) => {
                let (width, height) = match resolution.dimensions() {
                    Some(dimensions) => dimensions,
                    None => (
                        u32::try_from(frame.width).map_err(|_| "negative capture width")?,
                        u32::try_from(frame.height).map_err(|_| "negative capture height")?,
                    ),
                };
                destination.update_from_bgra_scaled(frame, width, height)
            }
        }
    }
}

type CaptureSlot = Arc<(Mutex<Option<CapturedFrame>>, Condvar)>;
type LatestFrame = Arc<Mutex<Option<DisplayFrame>>>;

#[derive(Clone, Debug)]
pub struct DisplayFrame {
    pub width: usize,
    pub height: usize,
    pub rgba: Vec<u8>,
}

#[derive(Clone, Debug)]
pub enum SessionEvent {
    Listening(SocketAddr),
    AwaitingApproval,
    PendingConnection(SocketAddr),
    Connected(SocketAddr),
    ConnectionRejected(String),
    Disconnected(String),
    CaptureReady,
    VideoConfigured { width: u32, height: u32 },
    VideoBackend(String),
    AudioBackend(String),
    Stats { fps: f32, megabits_per_second: f32 },
    Error(String),
}

#[derive(Clone, Copy, Debug)]
enum HostCommand {
    Accept(SocketAddr),
    Reject(SocketAddr),
}

pub struct DesktopSession {
    running: Arc<AtomicBool>,
    host_commands: Option<mpsc::Sender<HostCommand>>,
    audio_commands: Option<mpsc::Sender<Option<String>>>,
    events: mpsc::Receiver<SessionEvent>,
    latest_frame: LatestFrame,
    capture_slot: Option<CaptureSlot>,
    handles: Vec<JoinHandle<()>>,
}

impl DesktopSession {
    pub fn audio_output_devices() -> Result<Vec<String>, String> {
        audio::output_device_names()
    }

    pub fn start_host(
        bind_addr: SocketAddr,
        frames_per_second: u32,
        bitrate_bps: u32,
        resolution: VideoResolution,
    ) -> Result<Self, String> {
        let (audio_capture, audio_probe_error) = match audio::discover_system_audio_capture() {
            Ok(settings) => (Some(settings), None),
            Err(error) => (None, Some(error)),
        };
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        {
            ensure_software_capture_available()?;
        }

        let socket = Arc::new(
            UdpSocket::bind(bind_addr)
                .map_err(|error| format!("Unable to bind {bind_addr}: {error}"))?,
        );
        socket
            .set_read_timeout(Some(SOCKET_TIMEOUT))
            .map_err(|error| format!("Unable to configure server socket: {error}"))?;
        let local_addr = socket
            .local_addr()
            .map_err(|error| format!("Unable to read server address: {error}"))?;

        let running = Arc::new(AtomicBool::new(true));
        let active_peer = Arc::new(Mutex::new(None::<SocketAddr>));
        let force_keyframe = Arc::new(AtomicBool::new(true));
        let latest_frame = Arc::new(Mutex::new(None));
        let (event_tx, event_rx) = mpsc::channel();
        let (command_tx, command_rx) = mpsc::channel();

        let (width, height) = configured_video_dimensions(resolution)?;
        let session_config = SessionConfig {
            audio: audio_capture
                .as_ref()
                .map(audio::AudioCaptureSettings::session_config),
            video: Some(VideoConfig {
                codec: VideoCodec::H264,
                width,
                height,
                frame_rate: SessionFrameRate {
                    numerator: frames_per_second,
                    denominator: 1,
                },
                bitrate_bps,
            }),
        };
        let config_json = serde_json::to_vec(&session_config)
            .map_err(|error| format!("Unable to serialize session config: {error}"))?;

        let network_handle = spawn_host_network(
            socket.clone(),
            running.clone(),
            active_peer.clone(),
            force_keyframe.clone(),
            command_rx,
            event_tx.clone(),
            config_json,
        );
        if let Some(error) = audio_probe_error {
            let _ = event_tx.send(SessionEvent::AudioBackend(format!(
                "Audio unavailable: {error}"
            )));
        }

        #[cfg(target_os = "macos")]
        let (capture_slot, mut media_handles) = {
            let hardware_handle = hardware_macos::spawn_hardware_pipeline(
                socket.clone(),
                running.clone(),
                active_peer.clone(),
                force_keyframe.clone(),
                event_tx.clone(),
                frames_per_second,
                bitrate_bps,
                resolution,
            );
            (None, vec![hardware_handle])
        };

        #[cfg(target_os = "windows")]
        let (capture_slot, mut media_handles) = {
            let hardware_handle = hardware_windows::spawn_hardware_pipeline(
                socket.clone(),
                running.clone(),
                active_peer.clone(),
                force_keyframe.clone(),
                event_tx.clone(),
                frames_per_second,
                bitrate_bps,
                width,
                height,
                resolution,
            );
            (None, vec![hardware_handle])
        };

        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        let (capture_slot, mut media_handles) = {
            let capture_slot: CaptureSlot = Arc::new((Mutex::new(None), Condvar::new()));
            let capture_handle = spawn_capture(
                running.clone(),
                capture_slot.clone(),
                event_tx.clone(),
                frames_per_second,
                resolution,
            );
            let encoder_handle = spawn_encoder(
                socket.clone(),
                running.clone(),
                active_peer.clone(),
                force_keyframe.clone(),
                capture_slot.clone(),
                event_tx.clone(),
                frames_per_second,
                bitrate_bps,
                resolution,
            );
            (Some(capture_slot), vec![capture_handle, encoder_handle])
        };

        if let Some(settings) = audio_capture {
            media_handles.push(audio::spawn_audio_capture(
                socket,
                running.clone(),
                active_peer,
                event_tx.clone(),
                settings,
            ));
        }
        let _ = event_tx.send(SessionEvent::Listening(local_addr));
        let _ = event_tx.send(SessionEvent::VideoConfigured { width, height });
        let mut handles = vec![network_handle];
        handles.append(&mut media_handles);
        Ok(Self {
            running,
            host_commands: Some(command_tx),
            audio_commands: None,
            events: event_rx,
            latest_frame,
            capture_slot,
            handles,
        })
    }

    pub fn start_client(
        server_addr: SocketAddr,
        audio_output_device: Option<String>,
    ) -> Result<Self, String> {
        let bind_addr = if server_addr.is_ipv4() {
            "0.0.0.0:0"
        } else {
            "[::]:0"
        };
        let socket = UdpSocket::bind(bind_addr)
            .map_err(|error| format!("Unable to create client socket: {error}"))?;
        socket
            .connect(server_addr)
            .map_err(|error| format!("Unable to connect to {server_addr}: {error}"))?;
        socket
            .set_read_timeout(Some(SOCKET_TIMEOUT))
            .map_err(|error| format!("Unable to configure client socket: {error}"))?;

        let running = Arc::new(AtomicBool::new(true));
        let latest_frame = Arc::new(Mutex::new(None));
        let (event_tx, event_rx) = mpsc::channel();
        let (audio_command_tx, audio_command_rx) = mpsc::channel();
        let handle = spawn_client(
            socket,
            server_addr,
            running.clone(),
            latest_frame.clone(),
            event_tx,
            audio_output_device,
            audio_command_rx,
        );

        Ok(Self {
            running,
            host_commands: None,
            audio_commands: Some(audio_command_tx),
            events: event_rx,
            latest_frame,
            capture_slot: None,
            handles: vec![handle],
        })
    }

    pub fn accept(&self, peer: SocketAddr) {
        if let Some(commands) = &self.host_commands {
            let _ = commands.send(HostCommand::Accept(peer));
        }
    }

    pub fn reject(&self, peer: SocketAddr) {
        if let Some(commands) = &self.host_commands {
            let _ = commands.send(HostCommand::Reject(peer));
        }
    }

    pub fn set_audio_output_device(&self, device: Option<String>) {
        if let Some(commands) = &self.audio_commands {
            let _ = commands.send(device);
        }
    }

    pub fn drain_events(&self) -> impl Iterator<Item = SessionEvent> + '_ {
        self.events.try_iter()
    }

    pub fn take_latest_frame(&self) -> Option<DisplayFrame> {
        self.latest_frame.lock().unwrap().take()
    }

    pub fn stop(&mut self) {
        self.running.store(false, Ordering::Relaxed);
        if let Some(slot) = &self.capture_slot {
            slot.1.notify_all();
        }
        self.host_commands.take();
        self.handles.clear();
    }
}

fn ensure_software_capture_available() -> Result<(), String> {
    if !scap::is_supported() {
        return Err("Screen capture is not supported on this system".to_owned());
    }
    if !scap::has_permission() && !scap::request_permission() {
        return Err(
            "Screen capture permission was not granted. On macOS, grant it in System Settings and restart Echobind."
                .to_owned(),
        );
    }
    Ok(())
}

fn configured_video_dimensions(resolution: VideoResolution) -> Result<(u32, u32), String> {
    #[cfg(target_os = "windows")]
    let (native_width, native_height) = {
        let monitor = windows_capture::monitor::Monitor::primary()
            .map_err(|error| format!("Unable to find the primary display: {error}"))?;
        let width = monitor
            .width()
            .map_err(|error| format!("Unable to detect display width: {error}"))?;
        let height = monitor
            .height()
            .map_err(|error| format!("Unable to detect display height: {error}"))?;
        (width, height)
    };

    #[cfg(not(target_os = "windows"))]
    let (native_width, native_height) = {
        let mut probe = Capturer::build(Options {
            output_resolution: Resolution::Captured,
            ..Default::default()
        })
        .map_err(|error| format!("Unable to detect native display resolution: {error}"))?;
        let [width, height] = probe.get_output_frame_size();
        (width, height)
    };

    let (width, height) = match resolution.dimensions() {
        Some((max_width, max_height)) => {
            fit_capture_dimensions(native_width, native_height, max_width, max_height)
        }
        None => (native_width & !1, native_height & !1),
    };
    if width == 0 || height == 0 {
        return Err("Native display has invalid dimensions".to_owned());
    }
    Ok((width, height))
}

fn fit_capture_dimensions(width: u32, height: u32, max_width: u32, max_height: u32) -> (u32, u32) {
    if width == 0 || height == 0 {
        return (max_width, max_height);
    }
    let scale = (max_width as f64 / width as f64)
        .min(max_height as f64 / height as f64)
        .min(1.0);
    let fitted_width = ((width as f64 * scale).floor() as u32).max(2) & !1;
    let fitted_height = ((height as f64 * scale).floor() as u32).max(2) & !1;
    (fitted_width, fitted_height)
}

impl Drop for DesktopSession {
    fn drop(&mut self) {
        self.stop();
    }
}

#[allow(clippy::too_many_arguments)]
fn spawn_host_network(
    socket: Arc<UdpSocket>,
    running: Arc<AtomicBool>,
    active_peer: Arc<Mutex<Option<SocketAddr>>>,
    force_keyframe: Arc<AtomicBool>,
    commands: mpsc::Receiver<HostCommand>,
    events: mpsc::Sender<SessionEvent>,
    config_json: Vec<u8>,
) -> JoinHandle<()> {
    thread::spawn(move || {
        let mut packet_buffer = [0_u8; MAX_DATAGRAM_SIZE];
        let mut response = Vec::with_capacity(MAX_DATAGRAM_SIZE);
        let mut pending_peer = None;
        let mut last_seen = None::<Instant>;

        while running.load(Ordering::Relaxed) {
            while let Ok(command) = commands.try_recv() {
                match command {
                    HostCommand::Accept(peer) if pending_peer == Some(peer) => {
                        *active_peer.lock().unwrap() = Some(peer);
                        pending_peer = None;
                        last_seen = Some(Instant::now());
                        force_keyframe.store(true, Ordering::Relaxed);
                        Packet::Config(&config_json).encode(&mut response);
                        if let Err(error) = socket.send_to(&response, peer) {
                            let _ = events.send(SessionEvent::Error(format!(
                                "Unable to accept {peer}: {error}"
                            )));
                            *active_peer.lock().unwrap() = None;
                        } else {
                            let _ = events.send(SessionEvent::Connected(peer));
                        }
                    }
                    HostCommand::Reject(peer) if pending_peer == Some(peer) => {
                        Packet::ConnectionRejected(b"Connection rejected by host")
                            .encode(&mut response);
                        let _ = socket.send_to(&response, peer);
                        pending_peer = None;
                    }
                    HostCommand::Accept(_) | HostCommand::Reject(_) => {}
                }
            }

            match socket.recv_from(&mut packet_buffer) {
                Ok((size, peer)) => {
                    let packet = match Packet::try_from(&packet_buffer[..size]) {
                        Ok(packet) => packet,
                        Err(error) => {
                            debug!("Ignoring invalid packet from {peer}: {error}");
                            continue;
                        }
                    };
                    let current_peer = *active_peer.lock().unwrap();

                    match packet {
                        Packet::Hello if current_peer == Some(peer) => {
                            last_seen = Some(Instant::now());
                            Packet::Config(&config_json).encode(&mut response);
                            let _ = socket.send_to(&response, peer);
                        }
                        Packet::Hello if current_peer.is_none() => {
                            if pending_peer != Some(peer) {
                                pending_peer = Some(peer);
                                let _ = events.send(SessionEvent::PendingConnection(peer));
                            }
                        }
                        Packet::Hello => {
                            Packet::ConnectionRejected(b"Host is already connected")
                                .encode(&mut response);
                            let _ = socket.send_to(&response, peer);
                        }
                        Packet::Ping(id) if current_peer == Some(peer) => {
                            last_seen = Some(Instant::now());
                            Packet::Pong(id).encode(&mut response);
                            let _ = socket.send_to(&response, peer);
                        }
                        Packet::VideoKeyframeRequest if current_peer == Some(peer) => {
                            last_seen = Some(Instant::now());
                            force_keyframe.store(true, Ordering::Relaxed);
                        }
                        Packet::Clipboard(_)
                        | Packet::Audio(_)
                        | Packet::Video(_)
                        | Packet::Config(_)
                        | Packet::Pong(_)
                        | Packet::Ping(_)
                        | Packet::VideoKeyframeRequest
                        | Packet::ConnectionRejected(_) => {}
                    }
                }
                Err(error)
                    if error.kind() == std::io::ErrorKind::WouldBlock
                        || error.kind() == std::io::ErrorKind::TimedOut => {}
                Err(error) => {
                    let _ = events.send(SessionEvent::Error(format!(
                        "Server receive failed: {error}"
                    )));
                    break;
                }
            }

            if last_seen.is_some_and(|seen| seen.elapsed() >= CONNECTION_TIMEOUT) {
                if let Some(peer) = active_peer.lock().unwrap().take() {
                    let _ = events.send(SessionEvent::Disconnected(format!(
                        "{peer} stopped responding"
                    )));
                }
                last_seen = None;
            }
        }
    })
}

fn spawn_capture(
    running: Arc<AtomicBool>,
    capture_slot: CaptureSlot,
    events: mpsc::Sender<SessionEvent>,
    frames_per_second: u32,
    resolution: VideoResolution,
) -> JoinHandle<()> {
    thread::spawn(move || {
        let options = Options {
            fps: frames_per_second,
            show_cursor: true,
            show_highlight: false,
            output_type: if cfg!(target_os = "windows") {
                FrameType::BGRAFrame
            } else {
                FrameType::YUVFrame
            },
            output_resolution: resolution.capture_resolution(),
            ..Default::default()
        };

        let mut capturer = match Capturer::build(options) {
            Ok(capturer) => capturer,
            Err(error) => {
                let _ = events.send(SessionEvent::Error(format!(
                    "Unable to start screen capture: {error}"
                )));
                return;
            }
        };
        capturer.start_capture();
        let _ = events.send(SessionEvent::CaptureReady);

        while running.load(Ordering::Relaxed) {
            match capturer.get_next_frame() {
                Ok(Frame::YUVFrame(frame)) => {
                    let (slot, available) = &*capture_slot;
                    *slot.lock().unwrap() = Some(CapturedFrame::Nv12(frame));
                    available.notify_one();
                }
                Ok(Frame::BGRA(frame)) => {
                    let (slot, available) = &*capture_slot;
                    *slot.lock().unwrap() = Some(CapturedFrame::Bgra(frame));
                    available.notify_one();
                }
                Ok(
                    Frame::RGB(_)
                    | Frame::RGBx(_)
                    | Frame::XBGR(_)
                    | Frame::BGRx(_)
                    | Frame::BGR0(_),
                ) => {}
                Err(error) => {
                    let _ = events.send(SessionEvent::Error(format!(
                        "Screen capture stopped: {error}"
                    )));
                    break;
                }
            }
        }

        capturer.stop_capture();
    })
}

#[allow(clippy::too_many_arguments)]
fn spawn_encoder(
    socket: Arc<UdpSocket>,
    running: Arc<AtomicBool>,
    active_peer: Arc<Mutex<Option<SocketAddr>>>,
    force_keyframe: Arc<AtomicBool>,
    capture_slot: CaptureSlot,
    events: mpsc::Sender<SessionEvent>,
    frames_per_second: u32,
    bitrate_bps: u32,
    resolution: VideoResolution,
) -> JoinHandle<()> {
    thread::spawn(move || {
        let encoder_config = EncoderConfig::new()
            .bitrate(BitRate::from_bps(bitrate_bps))
            .max_frame_rate(EncoderFrameRate::from_hz(frames_per_second as f32))
            .rate_control_mode(RateControlMode::Bitrate)
            .usage_type(UsageType::ScreenContentRealTime)
            .complexity(Complexity::Low)
            .adaptive_quantization(false)
            .background_detection(false)
            .skip_frames(false)
            .intra_frame_period(IntraFramePeriod::from_num_frames(frames_per_second))
            .max_slice_len(32 * 1024);
        let mut encoder = match Encoder::with_api_config(OpenH264API::from_source(), encoder_config)
        {
            Ok(encoder) => encoder,
            Err(error) => {
                let _ = events.send(SessionEvent::Error(format!(
                    "Unable to initialize H.264 encoder: {error}"
                )));
                return;
            }
        };
        let _ = events.send(SessionEvent::VideoBackend(
            "OpenH264 software encoder".to_owned(),
        ));

        let started = Instant::now();
        let mut frame_id = 0_u64;
        let mut encoded_any = false;
        let mut packet = Vec::with_capacity(MAX_DATAGRAM_SIZE);
        let mut stats_started = Instant::now();
        let mut stats_frames = 0_u64;
        let mut stats_bytes = 0_u64;
        let mut frame = I420Frame::default();

        while running.load(Ordering::Relaxed) {
            let capture = {
                let (slot, available) = &*capture_slot;
                let guard = slot.lock().unwrap();
                let (mut guard, _) = available
                    .wait_timeout_while(guard, Duration::from_millis(50), |frame| {
                        frame.is_none() && running.load(Ordering::Relaxed)
                    })
                    .unwrap();
                guard.take()
            };
            let Some(capture) = capture else {
                continue;
            };
            let Some(peer) = *active_peer.lock().unwrap() else {
                continue;
            };

            if let Err(error) = capture.update_i420(&mut frame, resolution) {
                let _ = events.send(SessionEvent::Error(format!(
                    "Unable to convert captured frame: {error}"
                )));
                continue;
            }

            if encoded_any && force_keyframe.swap(false, Ordering::Relaxed) {
                encoder.force_intra_frame();
            }

            let encoded = match encoder.encode(&frame) {
                Ok(encoded) => encoded,
                Err(error) => {
                    let _ = events.send(SessionEvent::Error(format!(
                        "H.264 encoding failed: {error}"
                    )));
                    continue;
                }
            };
            let frame_type = encoded.frame_type();
            if frame_type == EncodedFrameType::Skip {
                continue;
            }
            let encoded = encoded.to_vec();
            if encoded.is_empty() {
                continue;
            }
            if !encoded_any {
                force_keyframe.store(false, Ordering::Relaxed);
            }
            encoded_any = true;

            let is_keyframe = matches!(frame_type, EncodedFrameType::IDR | EncodedFrameType::I);
            let timestamp_us = started.elapsed().as_micros().min(u128::from(u64::MAX)) as u64;
            let fragments =
                match fragment_video_frame(frame_id, timestamp_us, is_keyframe, &encoded) {
                    Ok(fragments) => fragments,
                    Err(error) => {
                        let _ = events.send(SessionEvent::Error(format!(
                            "Encoded frame cannot be packetized: {error}"
                        )));
                        continue;
                    }
                };

            let mut frame_sent = true;
            for fragment in fragments {
                Packet::Video(fragment).encode(&mut packet);
                if let Err(error) = socket.send_to(&packet, peer) {
                    warn!("Video send to {peer} failed: {error}");
                    frame_sent = false;
                    break;
                }
                stats_bytes = stats_bytes.saturating_add(packet.len() as u64);
            }
            frame_id = frame_id.wrapping_add(1);
            if frame_sent {
                stats_frames = stats_frames.saturating_add(1);
            }

            let elapsed = stats_started.elapsed();
            if elapsed >= Duration::from_secs(1) {
                let seconds = elapsed.as_secs_f32();
                let _ = events.send(SessionEvent::Stats {
                    fps: stats_frames as f32 / seconds,
                    megabits_per_second: stats_bytes as f32 * 8.0 / seconds / 1_000_000.0,
                });
                stats_started = Instant::now();
                stats_frames = 0;
                stats_bytes = 0;
            }
        }
    })
}

fn spawn_client(
    socket: UdpSocket,
    server_addr: SocketAddr,
    running: Arc<AtomicBool>,
    latest_frame: LatestFrame,
    events: mpsc::Sender<SessionEvent>,
    mut audio_output_device: Option<String>,
    audio_commands: mpsc::Receiver<Option<String>>,
) -> JoinHandle<()> {
    thread::spawn(move || {
        let mut decoder = match Decoder::new() {
            Ok(decoder) => decoder,
            Err(error) => {
                let _ = events.send(SessionEvent::Error(format!(
                    "Unable to initialize H.264 decoder: {error}"
                )));
                return;
            }
        };
        let _ = events.send(SessionEvent::VideoBackend(
            "OpenH264 software decoder".to_owned(),
        ));
        let mut reassembler = VideoReassembler::new(3, VIDEO_REASSEMBLY_AGE);
        let mut packet_buffer = [0_u8; MAX_DATAGRAM_SIZE];
        let mut outgoing = Vec::with_capacity(MAX_DATAGRAM_SIZE);
        let mut accepted = false;
        let mut last_hello = Instant::now() - HELLO_INTERVAL;
        let mut last_ping = Instant::now() - HEARTBEAT_INTERVAL;
        let mut last_server_response = Instant::now();
        let mut ping_id = 0_u64;
        let mut last_keyframe_request = Instant::now() - Duration::from_secs(1);
        let mut stats_started = Instant::now();
        let mut stats_frames = 0_u64;
        let mut stats_bytes = 0_u64;
        let mut last_decoded_frame_id = None::<u64>;
        let mut waiting_for_keyframe = true;
        let mut audio_config = None::<AudioConfig>;
        let mut audio_configured = false;
        let mut audio_playback = None::<audio::AudioPlayback>;
        let _ = events.send(SessionEvent::AwaitingApproval);

        while running.load(Ordering::Relaxed) {
            while let Ok(device) = audio_commands.try_recv() {
                audio_output_device = device;
                audio_playback.take();
                if let Some(config) = audio_config.clone() {
                    audio_playback = Some(audio::spawn_audio_playback(
                        config,
                        audio_output_device.clone(),
                        events.clone(),
                    ));
                }
            }
            if !accepted && last_hello.elapsed() >= HELLO_INTERVAL {
                Packet::Hello.encode(&mut outgoing);
                if let Err(error) = socket.send(&outgoing) {
                    let _ = events.send(SessionEvent::Error(format!(
                        "Connection request failed: {error}"
                    )));
                    return;
                }
                last_hello = Instant::now();
            }
            if accepted && last_ping.elapsed() >= HEARTBEAT_INTERVAL {
                Packet::Ping(ping_id).encode(&mut outgoing);
                let _ = socket.send(&outgoing);
                ping_id = ping_id.wrapping_add(1);
                last_ping = Instant::now();
            }

            match socket.recv(&mut packet_buffer) {
                Ok(size) => {
                    stats_bytes = stats_bytes.saturating_add(size as u64);
                    let packet = match Packet::try_from(&packet_buffer[..size]) {
                        Ok(packet) => packet,
                        Err(error) => {
                            debug!("Ignoring invalid packet from {server_addr}: {error}");
                            continue;
                        }
                    };
                    last_server_response = Instant::now();

                    match packet {
                        Packet::Config(json) => {
                            let config: SessionConfig = match serde_json::from_slice(json) {
                                Ok(config) => config,
                                Err(error) => {
                                    let _ = events.send(SessionEvent::Error(format!(
                                        "Host sent an invalid session config: {error}"
                                    )));
                                    return;
                                }
                            };
                            if !audio_configured {
                                audio_configured = true;
                                audio_config = config.audio.clone();
                                if let Some(config) = audio_config.clone() {
                                    audio_playback = Some(audio::spawn_audio_playback(
                                        config,
                                        audio_output_device.clone(),
                                        events.clone(),
                                    ));
                                } else {
                                    let _ = events.send(SessionEvent::AudioBackend(
                                        "Host did not offer audio".to_owned(),
                                    ));
                                }
                            }
                            let Some(video) = config.video else {
                                let _ = events.send(SessionEvent::Error(
                                    "Host did not offer a video stream".to_owned(),
                                ));
                                return;
                            };
                            if video.codec != VideoCodec::H264 {
                                let _ = events.send(SessionEvent::Error(format!(
                                    "Unsupported video codec: {:?}",
                                    video.codec
                                )));
                                return;
                            }
                            let _ = events.send(SessionEvent::VideoConfigured {
                                width: video.width,
                                height: video.height,
                            });
                            if !accepted {
                                accepted = true;
                                let _ = events.send(SessionEvent::Connected(server_addr));
                                request_keyframe(&socket, &mut outgoing);
                            }
                        }
                        Packet::ConnectionRejected(reason) => {
                            let reason = String::from_utf8_lossy(reason).into_owned();
                            let _ = events.send(SessionEvent::ConnectionRejected(reason));
                            return;
                        }
                        Packet::Pong(_) => {}
                        Packet::Ping(id) => {
                            Packet::Pong(id).encode(&mut outgoing);
                            let _ = socket.send(&outgoing);
                        }
                        Packet::Audio(frame) if accepted => {
                            if let Some(playback) = &audio_playback {
                                playback.push(frame);
                            }
                        }
                        Packet::Video(fragment) if accepted => match reassembler.push(fragment) {
                            Ok(Some(frame)) => {
                                let frame_gap = last_decoded_frame_id
                                    .is_some_and(|last| frame.frame_id != last.wrapping_add(1));
                                if (waiting_for_keyframe || frame_gap) && !frame.is_keyframe {
                                    waiting_for_keyframe = true;
                                    if last_keyframe_request.elapsed() >= Duration::from_millis(250)
                                    {
                                        request_keyframe(&socket, &mut outgoing);
                                        last_keyframe_request = Instant::now();
                                    }
                                    continue;
                                }

                                match decoder.decode(&frame.payload) {
                                    Ok(Some(decoded)) => {
                                        let (width, height) = decoded.dimensions();
                                        let mut rgba = vec![0; width * height * 4];
                                        decoded.write_rgba8(&mut rgba);
                                        *latest_frame.lock().unwrap() = Some(DisplayFrame {
                                            width,
                                            height,
                                            rgba,
                                        });
                                        last_decoded_frame_id = Some(frame.frame_id);
                                        waiting_for_keyframe = false;
                                        stats_frames = stats_frames.saturating_add(1);
                                    }
                                    Ok(None) => {}
                                    Err(error) => {
                                        warn!("H.264 decode failed: {error}");
                                        waiting_for_keyframe = true;
                                        if last_keyframe_request.elapsed()
                                            >= Duration::from_millis(250)
                                        {
                                            request_keyframe(&socket, &mut outgoing);
                                            last_keyframe_request = Instant::now();
                                        }
                                    }
                                }
                            }
                            Ok(None) => {}
                            Err(error) => {
                                warn!("Video reassembly failed: {error}");
                                waiting_for_keyframe = true;
                                if last_keyframe_request.elapsed() >= Duration::from_millis(250) {
                                    request_keyframe(&socket, &mut outgoing);
                                    last_keyframe_request = Instant::now();
                                }
                            }
                        },
                        Packet::Hello
                        | Packet::Clipboard(_)
                        | Packet::Audio(_)
                        | Packet::Video(_)
                        | Packet::VideoKeyframeRequest => {}
                    }
                }
                Err(error)
                    if error.kind() == std::io::ErrorKind::WouldBlock
                        || error.kind() == std::io::ErrorKind::TimedOut => {}
                Err(error) => {
                    let _ = events.send(SessionEvent::Error(format!(
                        "Video receive failed: {error}"
                    )));
                    return;
                }
            }

            if accepted && last_server_response.elapsed() >= CONNECTION_TIMEOUT {
                let _ = events.send(SessionEvent::Disconnected(
                    "Host stopped responding".to_owned(),
                ));
                return;
            }

            let elapsed = stats_started.elapsed();
            if elapsed >= Duration::from_secs(1) {
                let seconds = elapsed.as_secs_f32();
                let _ = events.send(SessionEvent::Stats {
                    fps: stats_frames as f32 / seconds,
                    megabits_per_second: stats_bytes as f32 * 8.0 / seconds / 1_000_000.0,
                });
                stats_started = Instant::now();
                stats_frames = 0;
                stats_bytes = 0;
            }
        }
    })
}

fn request_keyframe(socket: &UdpSocket, outgoing: &mut Vec<u8>) {
    Packet::VideoKeyframeRequest.encode(outgoing);
    let _ = socket.send(outgoing);
}
