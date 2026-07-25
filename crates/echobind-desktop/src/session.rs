use crate::video::I420Frame;
use echobind_core::{
    protocol::{Packet, JUMBO_DATAGRAM_SIZE, MAX_DATAGRAM_SIZE, STANDARD_DATAGRAM_SIZE},
    video::{fragment_video_frame_with_datagram_size, VideoFrame, VideoReassembler},
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
use socket2::SockRef;
use std::{
    collections::{BTreeMap, VecDeque},
    net::{SocketAddr, UdpSocket},
    sync::{
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
        mpsc, Arc, Condvar, Mutex,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
use tracing::{debug, warn};

#[path = "audio.rs"]
mod audio;

#[cfg(target_os = "macos")]
#[path = "decoder_macos.rs"]
mod decoder_macos;

#[cfg(target_os = "macos")]
#[path = "hardware_macos.rs"]
mod hardware_macos;

#[cfg(target_os = "windows")]
#[path = "hardware_windows.rs"]
mod hardware_windows;

const CONNECTION_TIMEOUT: Duration = Duration::from_secs(10);
const RECONNECT_GRACE: Duration = Duration::from_secs(30);
const KEYFRAME_RETRY_INTERVAL: Duration = Duration::from_millis(500);
const SOCKET_TIMEOUT: Duration = Duration::from_millis(20);
const VIDEO_REASSEMBLY_AGE: Duration = Duration::from_millis(50);
const VIDEO_DECODE_QUEUE_CAPACITY: usize = 2;
const VIDEO_REORDER_WINDOW: usize = 2;
pub(super) const VIDEO_STALE_AGE: Duration = Duration::from_millis(25);
pub(super) const VIDEO_SEND_STALE_AGE: Duration = Duration::from_millis(50);
const CLIENT_SOCKET_BUFFER_SIZE: usize = 8 * 1024 * 1024;
const HOST_SOCKET_BUFFER_SIZE: usize = 4 * 1024 * 1024;
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

struct CapturedSample {
    frame: CapturedFrame,
    captured_at: Instant,
}

type CaptureSlot = Arc<(Mutex<Option<CapturedSample>>, Condvar)>;
pub(super) type LatestFrame = Arc<Mutex<Option<DisplayFrame>>>;
pub(super) type FrameNotifier = Arc<dyn Fn() + Send + Sync>;

#[derive(Clone, Debug)]
pub(super) struct DisplayFrame {
    pub width: usize,
    pub height: usize,
    pub data: DisplayFrameData,
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub published_at: Instant,
}

#[derive(Clone, Debug)]
pub(super) enum DisplayFrameData {
    Rgba(Vec<u8>),
    #[cfg(target_os = "macos")]
    Nv12(apple_cf::cv::CVPixelBuffer),
}

#[derive(Default)]
pub(super) struct ClientMetrics {
    received_bytes: AtomicU64,
    reassembled_frames: AtomicU64,
    decoded_frames: AtomicU64,
    dropped_frames: AtomicU64,
    reassembly_us: AtomicU64,
    reassembly_samples: AtomicU64,
    decode_us: AtomicU64,
    decode_samples: AtomicU64,
    decode_queue_us: AtomicU64,
    decode_queue_samples: AtomicU64,
    jitter_us: AtomicU64,
    jitter_samples: AtomicU64,
    rtt_us: AtomicU64,
    lost_frames: AtomicU64,
}

#[derive(Clone, Debug)]
pub(super) struct ReceivedVideoFrame {
    pub frame: VideoFrame,
    pub received_at: Instant,
}

#[derive(Clone)]
pub(super) struct DecodeQueue {
    inner: Arc<(Mutex<DecodeQueueState>, Condvar)>,
}

#[derive(Default)]
struct DecodeQueueState {
    frames: VecDeque<ReceivedVideoFrame>,
    waiting_for_keyframe: bool,
    generation: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DecodeQueuePush {
    Queued,
    WaitingForKeyframe,
    Overflowed,
    Stale,
}

impl DecodeQueue {
    fn new() -> Self {
        Self {
            inner: Arc::new((Mutex::new(DecodeQueueState::default()), Condvar::new())),
        }
    }

    fn push(&self, frame: ReceivedVideoFrame) -> DecodeQueuePush {
        if frame.received_at.elapsed() > VIDEO_STALE_AGE {
            return DecodeQueuePush::Stale;
        }
        let (lock, ready) = &*self.inner;
        let mut state = lock.lock().unwrap();
        if state.waiting_for_keyframe && !frame.frame.is_keyframe {
            return DecodeQueuePush::WaitingForKeyframe;
        }
        if state.frames.len() >= VIDEO_DECODE_QUEUE_CAPACITY {
            state.frames.clear();
            state.waiting_for_keyframe = true;
            if !frame.frame.is_keyframe {
                return DecodeQueuePush::Overflowed;
            }
        }
        if frame.frame.is_keyframe {
            state.waiting_for_keyframe = false;
        }
        state.frames.push_back(frame);
        ready.notify_one();
        DecodeQueuePush::Queued
    }

    pub(super) fn pop_timeout(&self, timeout: Duration) -> Option<ReceivedVideoFrame> {
        let (lock, ready) = &*self.inner;
        let mut state = lock.lock().unwrap();
        if state.frames.is_empty() {
            state = ready.wait_timeout(state, timeout).unwrap().0;
        }
        state.frames.pop_front()
    }

    fn require_keyframe(&self) {
        let mut state = self.inner.0.lock().unwrap();
        state.frames.clear();
        state.waiting_for_keyframe = true;
        state.generation = state.generation.wrapping_add(1);
        self.inner.1.notify_all();
    }

    fn generation(&self) -> u64 {
        self.inner.0.lock().unwrap().generation
    }
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
    VideoConfigured {
        width: u32,
        height: u32,
    },
    VideoBackend(String),
    AudioBackend(String),
    Stats {
        fps: f32,
        megabits_per_second: f32,
        capture_ms: f32,
        encode_ms: f32,
        send_ms: f32,
        encode_queue_ms: f32,
    },
    ClientStats {
        received_fps: f32,
        decoded_fps: f32,
        megabits_per_second: f32,
        dropped_frames: u64,
        reassembly_ms: f32,
        decode_ms: f32,
        decode_queue_ms: f32,
        jitter_ms: f32,
        rtt_ms: f32,
        lost_frames: u64,
    },
    TransportConfigured {
        datagram_size: usize,
    },
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
    decode_queue: Option<DecodeQueue>,
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
        jumbo_datagrams: bool,
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
        configure_socket_buffers(&socket, HOST_SOCKET_BUFFER_SIZE)?;
        socket
            .set_read_timeout(Some(SOCKET_TIMEOUT))
            .map_err(|error| format!("Unable to configure server socket: {error}"))?;
        let local_addr = socket
            .local_addr()
            .map_err(|error| format!("Unable to read server address: {error}"))?;

        let running = Arc::new(AtomicBool::new(true));
        let active_peer = Arc::new(Mutex::new(None::<SocketAddr>));
        let force_keyframe = Arc::new(AtomicBool::new(true));
        let active_datagram_size = Arc::new(AtomicUsize::new(STANDARD_DATAGRAM_SIZE));
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
                datagram_size: STANDARD_DATAGRAM_SIZE as u16,
            }),
        };
        let standard_config_json = serde_json::to_vec(&session_config)
            .map_err(|error| format!("Unable to serialize session config: {error}"))?;
        let mut jumbo_config = session_config.clone();
        jumbo_config.video.as_mut().unwrap().datagram_size = JUMBO_DATAGRAM_SIZE as u16;
        let jumbo_config_json = serde_json::to_vec(&jumbo_config)
            .map_err(|error| format!("Unable to serialize jumbo session config: {error}"))?;

        let network_handle = spawn_host_network(
            socket.clone(),
            running.clone(),
            active_peer.clone(),
            force_keyframe.clone(),
            command_rx,
            event_tx.clone(),
            standard_config_json,
            jumbo_config_json,
            jumbo_datagrams,
            active_datagram_size.clone(),
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
                active_datagram_size.clone(),
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
                active_datagram_size.clone(),
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
                active_datagram_size.clone(),
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
            decode_queue: None,
            handles,
        })
    }

    pub fn start_client(
        server_addr: SocketAddr,
        audio_output_device: Option<String>,
        frame_notifier: FrameNotifier,
    ) -> Result<Self, String> {
        let bind_addr = if server_addr.is_ipv4() {
            "0.0.0.0:0"
        } else {
            "[::]:0"
        };
        let socket = UdpSocket::bind(bind_addr)
            .map_err(|error| format!("Unable to create client socket: {error}"))?;
        configure_socket_buffers(&socket, CLIENT_SOCKET_BUFFER_SIZE)?;
        socket
            .connect(server_addr)
            .map_err(|error| format!("Unable to connect to {server_addr}: {error}"))?;
        socket
            .set_read_timeout(Some(SOCKET_TIMEOUT))
            .map_err(|error| format!("Unable to configure client socket: {error}"))?;

        let running = Arc::new(AtomicBool::new(true));
        let latest_frame = Arc::new(Mutex::new(None));
        let decode_queue = DecodeQueue::new();
        let metrics = Arc::new(ClientMetrics::default());
        let decoder_needs_keyframe = Arc::new(AtomicBool::new(true));
        let (event_tx, event_rx) = mpsc::channel();
        let (audio_command_tx, audio_command_rx) = mpsc::channel();
        let network_handle = spawn_client_network(
            socket,
            server_addr,
            running.clone(),
            decode_queue.clone(),
            metrics.clone(),
            decoder_needs_keyframe.clone(),
            event_tx.clone(),
            audio_output_device,
            audio_command_rx,
        );
        let decoder_handle = spawn_video_decoder(
            running.clone(),
            decode_queue.clone(),
            latest_frame.clone(),
            metrics,
            decoder_needs_keyframe,
            event_tx,
            frame_notifier,
        );

        Ok(Self {
            running,
            host_commands: None,
            audio_commands: Some(audio_command_tx),
            events: event_rx,
            latest_frame,
            capture_slot: None,
            decode_queue: Some(decode_queue),
            handles: vec![network_handle, decoder_handle],
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
        if let Some(queue) = &self.decode_queue {
            queue.inner.1.notify_all();
        }
        self.host_commands.take();
        self.audio_commands.take();
        // Stop is complete only after every worker has dropped its socket and
        // platform media resources. The app invokes this method from a
        // dedicated shutdown thread so these joins never block egui.
        for handle in self.handles.drain(..) {
            let _ = handle.join();
        }
    }
}

fn configure_socket_buffers(socket: &UdpSocket, requested_size: usize) -> Result<(), String> {
    let socket_ref = SockRef::from(socket);
    socket_ref
        .set_recv_buffer_size(requested_size)
        .map_err(|error| format!("Unable to enlarge UDP receive buffer: {error}"))?;
    socket_ref
        .set_send_buffer_size(requested_size)
        .map_err(|error| format!("Unable to enlarge UDP send buffer: {error}"))?;
    Ok(())
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
    standard_config_json: Vec<u8>,
    jumbo_config_json: Vec<u8>,
    jumbo_requested: bool,
    active_datagram_size: Arc<AtomicUsize>,
) -> JoinHandle<()> {
    thread::spawn(move || {
        let mut packet_buffer = [0_u8; MAX_DATAGRAM_SIZE];
        let mut response = Vec::with_capacity(MAX_DATAGRAM_SIZE);
        let mut pending_peer = None::<(SocketAddr, usize)>;
        let mut last_seen = None::<Instant>;
        let mut reconnect_peer = None::<(SocketAddr, usize, Instant)>;

        while running.load(Ordering::Relaxed) {
            while let Ok(command) = commands.try_recv() {
                match command {
                    HostCommand::Accept(peer)
                        if pending_peer.is_some_and(|(pending, _)| pending == peer) =>
                    {
                        let client_max =
                            pending_peer.map_or(STANDARD_DATAGRAM_SIZE, |(_, max)| max);
                        let datagram_size = if jumbo_requested && client_max >= JUMBO_DATAGRAM_SIZE
                        {
                            JUMBO_DATAGRAM_SIZE
                        } else {
                            STANDARD_DATAGRAM_SIZE
                        };
                        active_datagram_size.store(datagram_size, Ordering::Release);
                        *active_peer.lock().unwrap() = Some(peer);
                        pending_peer = None;
                        reconnect_peer = None;
                        last_seen = Some(Instant::now());
                        force_keyframe.store(true, Ordering::Relaxed);
                        let config_json = if datagram_size == JUMBO_DATAGRAM_SIZE {
                            &jumbo_config_json
                        } else {
                            &standard_config_json
                        };
                        Packet::Config(config_json).encode(&mut response);
                        if let Err(error) = socket.send_to(&response, peer) {
                            let _ = events.send(SessionEvent::Error(format!(
                                "Unable to accept {peer}: {error}"
                            )));
                            *active_peer.lock().unwrap() = None;
                        } else {
                            let _ = events.send(SessionEvent::Connected(peer));
                            let _ =
                                events.send(SessionEvent::TransportConfigured { datagram_size });
                        }
                    }
                    HostCommand::Reject(peer)
                        if pending_peer.is_some_and(|(pending, _)| pending == peer) =>
                    {
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
                        Packet::Hello { .. } if current_peer == Some(peer) => {
                            last_seen = Some(Instant::now());
                            let datagram_size = active_datagram_size.load(Ordering::Acquire);
                            let config_json = if datagram_size == JUMBO_DATAGRAM_SIZE {
                                &jumbo_config_json
                            } else {
                                &standard_config_json
                            };
                            Packet::Config(config_json).encode(&mut response);
                            let _ = socket.send_to(&response, peer);
                        }
                        Packet::Hello { max_datagram_size }
                            if current_peer.is_some_and(|current| {
                                current != peer && current.ip() == peer.ip()
                            }) =>
                        {
                            // A manually restarted client keeps the same host
                            // address but receives a new ephemeral UDP port.
                            // Treat that Hello as an endpoint migration instead
                            // of rejecting the user's own Mac as a second peer.
                            let client_max = usize::from(max_datagram_size)
                                .clamp(STANDARD_DATAGRAM_SIZE, MAX_DATAGRAM_SIZE);
                            let datagram_size =
                                if jumbo_requested && client_max >= JUMBO_DATAGRAM_SIZE {
                                    JUMBO_DATAGRAM_SIZE
                                } else {
                                    STANDARD_DATAGRAM_SIZE
                                };
                            active_datagram_size.store(datagram_size, Ordering::Release);
                            *active_peer.lock().unwrap() = Some(peer);
                            last_seen = Some(Instant::now());
                            reconnect_peer = None;
                            force_keyframe.store(true, Ordering::Release);
                            let config_json = if datagram_size == JUMBO_DATAGRAM_SIZE {
                                &jumbo_config_json
                            } else {
                                &standard_config_json
                            };
                            Packet::Config(config_json).encode(&mut response);
                            if socket.send_to(&response, peer).is_ok() {
                                let _ = events.send(SessionEvent::Connected(peer));
                            }
                        }
                        Packet::Hello { .. }
                            if current_peer.is_none()
                                && reconnect_peer.is_some_and(|(trusted, _, since)| {
                                    trusted == peer && since.elapsed() < RECONNECT_GRACE
                                }) =>
                        {
                            let (_, datagram_size, _) = reconnect_peer
                                .take()
                                .expect("the reconnect peer was just matched");
                            active_datagram_size.store(datagram_size, Ordering::Release);
                            *active_peer.lock().unwrap() = Some(peer);
                            last_seen = Some(Instant::now());
                            force_keyframe.store(true, Ordering::Release);
                            let config_json = if datagram_size == JUMBO_DATAGRAM_SIZE {
                                &jumbo_config_json
                            } else {
                                &standard_config_json
                            };
                            Packet::Config(config_json).encode(&mut response);
                            if let Err(error) = socket.send_to(&response, peer) {
                                *active_peer.lock().unwrap() = None;
                                reconnect_peer = Some((peer, datagram_size, Instant::now()));
                                last_seen = None;
                                let _ = events.send(SessionEvent::Error(format!(
                                    "Unable to restore {peer}: {error}"
                                )));
                            } else {
                                let _ = events.send(SessionEvent::Connected(peer));
                            }
                        }
                        Packet::Hello { max_datagram_size } if current_peer.is_none() => {
                            let advertised = usize::from(max_datagram_size)
                                .clamp(STANDARD_DATAGRAM_SIZE, MAX_DATAGRAM_SIZE);
                            let is_new = pending_peer.map(|(pending, _)| pending) != Some(peer);
                            pending_peer = Some((peer, advertised));
                            if is_new {
                                let _ = events.send(SessionEvent::PendingConnection(peer));
                            }
                        }
                        Packet::Hello { .. } => {
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
                    reconnect_peer = Some((
                        peer,
                        active_datagram_size.load(Ordering::Acquire),
                        Instant::now(),
                    ));
                    let _ = events.send(SessionEvent::Disconnected(format!(
                        "{peer} stopped responding"
                    )));
                }
                last_seen = None;
            }
            if reconnect_peer.is_some_and(|(_, _, since)| since.elapsed() >= RECONNECT_GRACE) {
                reconnect_peer = None;
                active_datagram_size.store(STANDARD_DATAGRAM_SIZE, Ordering::Release);
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
                    *slot.lock().unwrap() = Some(CapturedSample {
                        frame: CapturedFrame::Nv12(frame),
                        captured_at: Instant::now(),
                    });
                    available.notify_one();
                }
                Ok(Frame::BGRA(frame)) => {
                    let (slot, available) = &*capture_slot;
                    *slot.lock().unwrap() = Some(CapturedSample {
                        frame: CapturedFrame::Bgra(frame),
                        captured_at: Instant::now(),
                    });
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
    active_datagram_size: Arc<AtomicUsize>,
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
            .skip_frames(true)
            .intra_frame_period(IntraFramePeriod::from_num_frames(u32::MAX));
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
        let mut stats_capture_us = 0_u64;
        let mut stats_encode_us = 0_u64;
        let mut stats_send_us = 0_u64;
        let mut stats_encode_queue_us = 0_u64;
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
            if capture.captured_at.elapsed() > VIDEO_SEND_STALE_AGE {
                force_keyframe.store(true, Ordering::Release);
                continue;
            }
            let Some(peer) = *active_peer.lock().unwrap() else {
                continue;
            };

            let capture_started = Instant::now();
            if let Err(error) = capture.frame.update_i420(&mut frame, resolution) {
                let _ = events.send(SessionEvent::Error(format!(
                    "Unable to convert captured frame: {error}"
                )));
                continue;
            }
            let capture_us = capture_started
                .elapsed()
                .as_micros()
                .min(u128::from(u64::MAX)) as u64;

            if encoded_any && force_keyframe.swap(false, Ordering::Relaxed) {
                encoder.force_intra_frame();
            }

            let encode_started = Instant::now();
            let encoded = match encoder.encode(&frame) {
                Ok(encoded) => encoded,
                Err(error) => {
                    let _ = events.send(SessionEvent::Error(format!(
                        "H.264 encoding failed: {error}"
                    )));
                    continue;
                }
            };
            let encode_us = encode_started
                .elapsed()
                .as_micros()
                .min(u128::from(u64::MAX)) as u64;
            if capture.captured_at.elapsed() > VIDEO_SEND_STALE_AGE {
                force_keyframe.store(true, Ordering::Release);
                continue;
            }
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
            let encoded_at = Instant::now();
            let fragments = match fragment_video_frame_with_datagram_size(
                frame_id,
                timestamp_us,
                is_keyframe,
                &encoded,
                active_datagram_size.load(Ordering::Acquire),
            ) {
                Ok(fragments) => fragments,
                Err(error) => {
                    let _ = events.send(SessionEvent::Error(format!(
                        "Encoded frame cannot be packetized: {error}"
                    )));
                    continue;
                }
            };
            let encode_queue_us = encoded_at.elapsed().as_micros().min(u128::from(u64::MAX)) as u64;

            let send_started = Instant::now();
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
                stats_capture_us = stats_capture_us.saturating_add(capture_us);
                stats_encode_us = stats_encode_us.saturating_add(encode_us);
                stats_send_us = stats_send_us.saturating_add(
                    send_started.elapsed().as_micros().min(u128::from(u64::MAX)) as u64,
                );
                stats_encode_queue_us = stats_encode_queue_us.saturating_add(encode_queue_us);
            }

            let elapsed = stats_started.elapsed();
            if elapsed >= Duration::from_secs(1) {
                let seconds = elapsed.as_secs_f32();
                let _ = events.send(SessionEvent::Stats {
                    fps: stats_frames as f32 / seconds,
                    megabits_per_second: stats_bytes as f32 * 8.0 / seconds / 1_000_000.0,
                    capture_ms: average_milliseconds(stats_capture_us, stats_frames),
                    encode_ms: average_milliseconds(stats_encode_us, stats_frames),
                    send_ms: average_milliseconds(stats_send_us, stats_frames),
                    encode_queue_ms: average_milliseconds(stats_encode_queue_us, stats_frames),
                });
                stats_started = Instant::now();
                stats_frames = 0;
                stats_bytes = 0;
                stats_capture_us = 0;
                stats_encode_us = 0;
                stats_send_us = 0;
                stats_encode_queue_us = 0;
            }
        }
    })
}

#[allow(clippy::too_many_arguments)]
fn spawn_client_network(
    socket: UdpSocket,
    server_addr: SocketAddr,
    running: Arc<AtomicBool>,
    decode_queue: DecodeQueue,
    metrics: Arc<ClientMetrics>,
    decoder_needs_keyframe: Arc<AtomicBool>,
    events: mpsc::Sender<SessionEvent>,
    mut audio_output_device: Option<String>,
    audio_commands: mpsc::Receiver<Option<String>>,
) -> JoinHandle<()> {
    thread::spawn(move || {
        let mut reassembler = VideoReassembler::new(8, VIDEO_REASSEMBLY_AGE);
        let mut packet_buffer = [0_u8; MAX_DATAGRAM_SIZE];
        let mut outgoing = Vec::with_capacity(MAX_DATAGRAM_SIZE);
        let mut accepted = false;
        let mut last_hello = Instant::now() - HELLO_INTERVAL;
        let mut last_ping = Instant::now() - HEARTBEAT_INTERVAL;
        let mut last_server_response = Instant::now();
        let mut ping_id = 0_u64;
        let mut last_keyframe_request = Instant::now() - Duration::from_secs(1);
        let mut stats_started = Instant::now();
        let mut next_frame_id = None::<u64>;
        let mut completed_frames = BTreeMap::<u64, ReceivedVideoFrame>::new();
        let mut reassembly_started = BTreeMap::<u64, Instant>::new();
        let mut ping_sent = BTreeMap::<u64, Instant>::new();
        let mut last_frame_arrival = None::<(Instant, u64)>;
        let mut negotiated_datagram_size = STANDARD_DATAGRAM_SIZE;
        let mut waiting_for_keyframe = true;
        let mut audio_config = None::<AudioConfig>;
        let mut audio_configured = false;
        let mut audio_playback = None::<audio::AudioPlayback>;
        let _ = events.send(SessionEvent::AwaitingApproval);

        while running.load(Ordering::Relaxed) {
            let expired = reassembler.expire_stale() as u64;
            if expired > 0 {
                metrics.dropped_frames.fetch_add(expired, Ordering::Relaxed);
                metrics.lost_frames.fetch_add(expired, Ordering::Relaxed);
                reassembly_started.retain(|_, started| started.elapsed() <= VIDEO_REASSEMBLY_AGE);
                // A missing H.264 reference frame invalidates every dependent
                // frame after it. Do not let VideoToolbox conceal that damage
                // as colored bands or blocky smearing: discard the chain and
                // immediately resume from a new IDR.
                decode_queue.require_keyframe();
                completed_frames.clear();
                next_frame_id = None;
                waiting_for_keyframe = true;
                if accepted && last_keyframe_request.elapsed() >= KEYFRAME_RETRY_INTERVAL {
                    request_keyframe(&socket, &mut outgoing);
                    last_keyframe_request = Instant::now();
                }
            }
            if decoder_needs_keyframe.swap(false, Ordering::AcqRel) {
                decode_queue.require_keyframe();
                completed_frames.clear();
                next_frame_id = None;
                waiting_for_keyframe = true;
                if accepted && last_keyframe_request.elapsed() >= KEYFRAME_RETRY_INTERVAL {
                    request_keyframe(&socket, &mut outgoing);
                    last_keyframe_request = Instant::now();
                }
            }
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
                // Send the legacy empty hello first so standard-MTU ECB2 hosts
                // still see the request, then advertise jumbo capability to
                // upgraded hosts.
                Packet::Hello {
                    max_datagram_size: STANDARD_DATAGRAM_SIZE as u16,
                }
                .encode(&mut outgoing);
                let _ = socket.send(&outgoing);
                Packet::Hello {
                    max_datagram_size: MAX_DATAGRAM_SIZE as u16,
                }
                .encode(&mut outgoing);
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
                ping_sent.insert(ping_id, Instant::now());
                while ping_sent.len() > 4 {
                    ping_sent.pop_first();
                }
                ping_id = ping_id.wrapping_add(1);
                last_ping = Instant::now();
            }

            match socket.recv(&mut packet_buffer) {
                Ok(size) => {
                    metrics
                        .received_bytes
                        .fetch_add(size as u64, Ordering::Relaxed);
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
                            let offered_datagram_size = usize::from(video.datagram_size);
                            if offered_datagram_size != STANDARD_DATAGRAM_SIZE
                                && offered_datagram_size != JUMBO_DATAGRAM_SIZE
                            {
                                let _ = events.send(SessionEvent::Error(format!(
                                    "Host selected unsupported datagram size {offered_datagram_size}"
                                )));
                                return;
                            }
                            negotiated_datagram_size = offered_datagram_size;
                            let _ = events.send(SessionEvent::TransportConfigured {
                                datagram_size: negotiated_datagram_size,
                            });
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
                        Packet::Pong(id) => {
                            if let Some(sent) = ping_sent.remove(&id) {
                                metrics.rtt_us.store(
                                    sent.elapsed().as_micros().min(u128::from(u64::MAX)) as u64,
                                    Ordering::Relaxed,
                                );
                            }
                        }
                        Packet::Ping(id) => {
                            Packet::Pong(id).encode(&mut outgoing);
                            let _ = socket.send(&outgoing);
                        }
                        Packet::Audio(frame) if accepted => {
                            if let Some(playback) = &audio_playback {
                                playback.push(frame);
                            }
                        }
                        Packet::Video(fragment) if accepted => {
                            if size > negotiated_datagram_size {
                                metrics.dropped_frames.fetch_add(1, Ordering::Relaxed);
                                metrics.lost_frames.fetch_add(1, Ordering::Relaxed);
                                continue;
                            }
                            let fragmented_frame_id = fragment.frame_id;
                            reassembly_started
                                .entry(fragmented_frame_id)
                                .or_insert_with(Instant::now);
                            while reassembly_started.len() > 16 {
                                reassembly_started.pop_first();
                            }
                            match reassembler.push(fragment) {
                                Ok(Some(frame)) => {
                                    let completed_at = Instant::now();
                                    if let Some(started) =
                                        reassembly_started.remove(&frame.frame_id)
                                    {
                                        metrics.reassembly_us.fetch_add(
                                            started.elapsed().as_micros().min(u128::from(u64::MAX))
                                                as u64,
                                            Ordering::Relaxed,
                                        );
                                        metrics.reassembly_samples.fetch_add(1, Ordering::Relaxed);
                                    }
                                    metrics.reassembled_frames.fetch_add(1, Ordering::Relaxed);
                                    if let Some((previous_arrival, previous_timestamp)) =
                                        last_frame_arrival.filter(|(_, timestamp)| {
                                            frame.timestamp_us > *timestamp
                                        })
                                    {
                                        let arrival_delta = completed_at
                                            .saturating_duration_since(previous_arrival)
                                            .as_micros()
                                            .min(u128::from(u64::MAX))
                                            as u64;
                                        let source_delta =
                                            frame.timestamp_us.saturating_sub(previous_timestamp);
                                        metrics.jitter_us.fetch_add(
                                            arrival_delta.abs_diff(source_delta),
                                            Ordering::Relaxed,
                                        );
                                        metrics.jitter_samples.fetch_add(1, Ordering::Relaxed);
                                        last_frame_arrival =
                                            Some((completed_at, frame.timestamp_us));
                                    } else if last_frame_arrival.is_none() {
                                        last_frame_arrival =
                                            Some((completed_at, frame.timestamp_us));
                                    }
                                    if waiting_for_keyframe && !frame.is_keyframe {
                                        metrics.dropped_frames.fetch_add(1, Ordering::Relaxed);
                                        if last_keyframe_request.elapsed()
                                            >= KEYFRAME_RETRY_INTERVAL
                                        {
                                            request_keyframe(&socket, &mut outgoing);
                                            last_keyframe_request = Instant::now();
                                        }
                                        continue;
                                    }

                                    // A complete later frame can arrive before an earlier frame when
                                    // UDP reorders datagrams. Hold a tiny reorder window instead of
                                    // treating that as packet loss immediately.
                                    if frame.is_keyframe && next_frame_id != Some(frame.frame_id) {
                                        completed_frames.clear();
                                        next_frame_id = Some(frame.frame_id);
                                        waiting_for_keyframe = false;
                                    } else if waiting_for_keyframe {
                                        next_frame_id = Some(frame.frame_id);
                                        waiting_for_keyframe = false;
                                    }
                                    let frame_id = frame.frame_id;
                                    if next_frame_id.is_some_and(|next| frame_id < next) {
                                        metrics.dropped_frames.fetch_add(1, Ordering::Relaxed);
                                        continue;
                                    }
                                    completed_frames.entry(frame_id).or_insert(
                                        ReceivedVideoFrame {
                                            frame,
                                            received_at: completed_at,
                                        },
                                    );

                                    let expected =
                                        next_frame_id.expect("a completed frame sets order");
                                    if !completed_frames.contains_key(&expected)
                                        && completed_frames.len() >= VIDEO_REORDER_WINDOW
                                    {
                                        let newest = completed_frames
                                            .last_key_value()
                                            .map_or(expected, |(id, _)| *id);
                                        let span =
                                            newest.saturating_sub(expected).saturating_add(1);
                                        let lost = span
                                            .saturating_sub(completed_frames.len() as u64)
                                            .max(1);
                                        let unusable =
                                            lost.saturating_add(completed_frames.len() as u64);
                                        metrics
                                            .dropped_frames
                                            .fetch_add(unusable, Ordering::Relaxed);
                                        metrics.lost_frames.fetch_add(lost, Ordering::Relaxed);
                                        completed_frames.clear();
                                        decode_queue.require_keyframe();
                                        next_frame_id = None;
                                        waiting_for_keyframe = true;
                                    }

                                    while let Some(expected) = next_frame_id {
                                        let Some(ordered_frame) =
                                            completed_frames.remove(&expected)
                                        else {
                                            break;
                                        };
                                        match decode_queue.push(ordered_frame) {
                                            DecodeQueuePush::Queued => {
                                                next_frame_id = Some(expected.wrapping_add(1));
                                            }
                                            DecodeQueuePush::WaitingForKeyframe
                                            | DecodeQueuePush::Overflowed
                                            | DecodeQueuePush::Stale => {
                                                metrics
                                                    .dropped_frames
                                                    .fetch_add(1, Ordering::Relaxed);
                                                completed_frames.clear();
                                                next_frame_id = None;
                                                waiting_for_keyframe = true;
                                                break;
                                            }
                                        }
                                    }
                                    if waiting_for_keyframe
                                        && last_keyframe_request.elapsed()
                                            >= KEYFRAME_RETRY_INTERVAL
                                    {
                                        request_keyframe(&socket, &mut outgoing);
                                        last_keyframe_request = Instant::now();
                                    }
                                }
                                Ok(None) => {}
                                Err(error) => {
                                    reassembly_started.remove(&fragmented_frame_id);
                                    warn!("Video reassembly failed: {error}");
                                    metrics.dropped_frames.fetch_add(1, Ordering::Relaxed);
                                    metrics.lost_frames.fetch_add(1, Ordering::Relaxed);
                                    decode_queue.require_keyframe();
                                    completed_frames.clear();
                                    next_frame_id = None;
                                    waiting_for_keyframe = true;
                                    if last_keyframe_request.elapsed() >= KEYFRAME_RETRY_INTERVAL {
                                        request_keyframe(&socket, &mut outgoing);
                                        last_keyframe_request = Instant::now();
                                    }
                                }
                            }
                        }
                        Packet::Hello { .. }
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
                    "Host stopped responding; reconnecting…".to_owned(),
                ));
                accepted = false;
                reassembler = VideoReassembler::new(8, VIDEO_REASSEMBLY_AGE);
                reassembly_started.clear();
                completed_frames.clear();
                decode_queue.require_keyframe();
                next_frame_id = None;
                waiting_for_keyframe = true;
                last_hello = Instant::now() - HELLO_INTERVAL;
                last_server_response = Instant::now();
            }

            let elapsed = stats_started.elapsed();
            if elapsed >= Duration::from_secs(1) {
                let seconds = elapsed.as_secs_f32();
                let received_frames = metrics.reassembled_frames.swap(0, Ordering::Relaxed);
                let decoded_frames = metrics.decoded_frames.swap(0, Ordering::Relaxed);
                let received_bytes = metrics.received_bytes.swap(0, Ordering::Relaxed);
                let dropped_frames = metrics.dropped_frames.swap(0, Ordering::Relaxed);
                let reassembly_us = metrics.reassembly_us.swap(0, Ordering::Relaxed);
                let reassembly_samples = metrics.reassembly_samples.swap(0, Ordering::Relaxed);
                let decode_us = metrics.decode_us.swap(0, Ordering::Relaxed);
                let decode_samples = metrics.decode_samples.swap(0, Ordering::Relaxed);
                let decode_queue_us = metrics.decode_queue_us.swap(0, Ordering::Relaxed);
                let decode_queue_samples = metrics.decode_queue_samples.swap(0, Ordering::Relaxed);
                let jitter_us = metrics.jitter_us.swap(0, Ordering::Relaxed);
                let jitter_samples = metrics.jitter_samples.swap(0, Ordering::Relaxed);
                let lost_frames = metrics.lost_frames.swap(0, Ordering::Relaxed);
                let _ = events.send(SessionEvent::ClientStats {
                    received_fps: received_frames as f32 / seconds,
                    decoded_fps: decoded_frames as f32 / seconds,
                    megabits_per_second: received_bytes as f32 * 8.0 / seconds / 1_000_000.0,
                    dropped_frames,
                    reassembly_ms: average_milliseconds(reassembly_us, reassembly_samples),
                    decode_ms: average_milliseconds(decode_us, decode_samples),
                    decode_queue_ms: average_milliseconds(decode_queue_us, decode_queue_samples),
                    jitter_ms: average_milliseconds(jitter_us, jitter_samples),
                    rtt_ms: metrics.rtt_us.load(Ordering::Relaxed) as f32 / 1_000.0,
                    lost_frames,
                });
                stats_started = Instant::now();
            }
        }
    })
}

fn spawn_video_decoder(
    running: Arc<AtomicBool>,
    decode_queue: DecodeQueue,
    latest_frame: LatestFrame,
    metrics: Arc<ClientMetrics>,
    needs_keyframe: Arc<AtomicBool>,
    events: mpsc::Sender<SessionEvent>,
    frame_notifier: FrameNotifier,
) -> JoinHandle<()> {
    thread::spawn(move || {
        #[cfg(target_os = "macos")]
        {
            match decoder_macos::run_decoder(
                running.clone(),
                decode_queue.clone(),
                latest_frame.clone(),
                metrics.clone(),
                needs_keyframe.clone(),
                events.clone(),
                frame_notifier.clone(),
            ) {
                Ok(()) => return,
                Err(error) => {
                    warn!("VideoToolbox decoder unavailable: {error}");
                    let _ = events.send(SessionEvent::VideoBackend(format!(
                        "OpenH264 fallback (VideoToolbox unavailable: {error})"
                    )));
                    needs_keyframe.store(true, Ordering::Release);
                }
            }
        }

        if let Err(error) = run_software_video_decoder(
            running,
            decode_queue,
            latest_frame,
            metrics,
            needs_keyframe,
            events.clone(),
            frame_notifier,
        ) {
            let _ = events.send(SessionEvent::Error(error));
        }
    })
}

fn run_software_video_decoder(
    running: Arc<AtomicBool>,
    decode_queue: DecodeQueue,
    latest_frame: LatestFrame,
    metrics: Arc<ClientMetrics>,
    needs_keyframe: Arc<AtomicBool>,
    events: mpsc::Sender<SessionEvent>,
    frame_notifier: FrameNotifier,
) -> Result<(), String> {
    let mut decoder =
        Decoder::new().map_err(|error| format!("Unable to initialize H.264 decoder: {error}"))?;
    let mut decoder_generation = decode_queue.generation();
    let _ = events.send(SessionEvent::VideoBackend(
        "OpenH264 software decoder · separate receive thread".to_owned(),
    ));

    while running.load(Ordering::Relaxed) {
        let generation = decode_queue.generation();
        if generation != decoder_generation {
            decoder = Decoder::new()
                .map_err(|error| format!("Unable to reset H.264 decoder: {error}"))?;
            decoder_generation = generation;
        }
        let Some(received) = decode_queue.pop_timeout(Duration::from_millis(20)) else {
            continue;
        };
        let queue_elapsed = received.received_at.elapsed();
        if queue_elapsed > VIDEO_STALE_AGE {
            metrics.dropped_frames.fetch_add(1, Ordering::Relaxed);
            needs_keyframe.store(true, Ordering::Release);
            continue;
        }
        metrics.decode_queue_us.fetch_add(
            queue_elapsed.as_micros().min(u128::from(u64::MAX)) as u64,
            Ordering::Relaxed,
        );
        metrics.decode_queue_samples.fetch_add(1, Ordering::Relaxed);
        let frame = received.frame;
        let decode_started = Instant::now();
        match decoder.decode(&frame.payload) {
            Ok(Some(decoded)) => {
                let (width, height) = decoded.dimensions();
                let mut rgba = vec![0; width * height * 4];
                decoded.write_rgba8(&mut rgba);
                *latest_frame.lock().unwrap() = Some(DisplayFrame {
                    width,
                    height,
                    data: DisplayFrameData::Rgba(rgba),
                    published_at: Instant::now(),
                });
                frame_notifier();
                metrics.decoded_frames.fetch_add(1, Ordering::Relaxed);
                metrics.decode_us.fetch_add(
                    decode_started
                        .elapsed()
                        .as_micros()
                        .min(u128::from(u64::MAX)) as u64,
                    Ordering::Relaxed,
                );
                metrics.decode_samples.fetch_add(1, Ordering::Relaxed);
            }
            Ok(None) => {}
            Err(error) => {
                warn!("H.264 decode failed: {error}");
                metrics.dropped_frames.fetch_add(1, Ordering::Relaxed);
                needs_keyframe.store(true, Ordering::Release);
            }
        }
    }
    Ok(())
}

fn request_keyframe(socket: &UdpSocket, outgoing: &mut Vec<u8>) {
    Packet::VideoKeyframeRequest.encode(outgoing);
    let _ = socket.send(outgoing);
}

fn average_milliseconds(total_us: u64, samples: u64) -> f32 {
    if samples == 0 {
        0.0
    } else {
        total_us as f32 / samples as f32 / 1_000.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn received(frame_id: u64, is_keyframe: bool, age: Duration) -> ReceivedVideoFrame {
        ReceivedVideoFrame {
            frame: VideoFrame {
                frame_id,
                timestamp_us: frame_id,
                is_keyframe,
                payload: vec![1],
            },
            received_at: Instant::now() - age,
        }
    }

    #[test]
    fn decode_queue_discards_backlog_until_a_fresh_keyframe() {
        let queue = DecodeQueue::new();
        assert_eq!(
            queue.push(received(0, true, Duration::ZERO)),
            DecodeQueuePush::Queued
        );
        assert_eq!(
            queue.push(received(1, false, Duration::ZERO)),
            DecodeQueuePush::Queued
        );
        assert_eq!(
            queue.push(received(2, false, Duration::ZERO)),
            DecodeQueuePush::Overflowed
        );
        assert_eq!(
            queue.push(received(3, false, Duration::ZERO)),
            DecodeQueuePush::WaitingForKeyframe
        );
        assert_eq!(
            queue.push(received(4, true, Duration::ZERO)),
            DecodeQueuePush::Queued
        );
    }

    #[test]
    fn decode_queue_rejects_stale_frames() {
        let queue = DecodeQueue::new();
        assert_eq!(
            queue.push(received(
                0,
                true,
                VIDEO_STALE_AGE + Duration::from_millis(1)
            )),
            DecodeQueuePush::Stale
        );
    }
}
