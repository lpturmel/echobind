mod views;

use crate::{
    session::{DesktopSession, DisplayFrameData, SessionEvent, VideoResolution},
    theme::{self, Tone},
};
use echobind_core::protocol::{CursorPosition, ServerStats};
use eframe::egui;
use std::{
    net::{IpAddr, SocketAddr},
    sync::{
        atomic::{AtomicU64, Ordering},
        mpsc, Arc,
    },
    thread,
    time::{Duration, Instant},
};

const SERVER_STATS_STALE_AFTER: Duration = Duration::from_secs(3);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    Host,
    Connect,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WatchPhase {
    Idle,
    Waiting,
    Live,
    /// Connected to an audio-only session.
    Listening,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SharePhase {
    Idle,
    Listening,
    Live,
}

/// A message shown as a banner until the next session starts.
struct Notice {
    tone: Tone,
    title: String,
    body: String,
}

impl Notice {
    fn new(tone: Tone, title: impl Into<String>, body: impl Into<String>) -> Self {
        Self {
            tone,
            title: title.into(),
            body: body.into(),
        }
    }
}

struct GpuVideoTexture {
    texture: eframe::wgpu::Texture,
    id: egui::TextureId,
    width: usize,
    height: usize,
}

struct SessionShutdown {
    complete: mpsc::Receiver<()>,
    handle: Option<thread::JoinHandle<()>>,
}

pub struct EchobindApp {
    mode: Mode,
    host_ip: String,
    server_ip: String,
    port_text: String,
    port: u16,
    ip_error: Option<String>,
    port_error: Option<String>,
    frames_per_second: u32,
    bitrate_mbps: u32,
    resolution: VideoResolution,
    jumbo_datagrams: bool,
    transport: String,
    notice: Option<Notice>,
    listen_address: Option<SocketAddr>,
    capture_ready: bool,
    target: Option<SocketAddr>,
    peer: Option<SocketAddr>,
    reconnecting: bool,
    pending_peer: Option<SocketAddr>,
    health_open: bool,
    drawer_open: bool,
    watch_audio_only: bool,
    share_audio_only: bool,
    watch_volume: u32,
    watch_muted: bool,
    share_volume: u32,
    share_muted: bool,
    /// Whether the connected session carries video, once the peer says so.
    peer_video: Option<bool>,
    pending_video: bool,
    session: Option<DesktopSession>,
    shutdown: Option<SessionShutdown>,
    texture: Option<GpuVideoTexture>,
    nv12_ready: bool,
    remote_cursor: Option<CursorPosition>,
    render_state: eframe::egui_wgpu::RenderState,
    stream_fps: f32,
    received_fps: f32,
    dropped_frames: u64,
    stream_mbps: f32,
    stream_width: u32,
    stream_height: u32,
    video_backend: String,
    audio_output_devices: Vec<String>,
    audio_output_device: Option<String>,
    audio_backend: String,
    fullscreen: bool,
    presented_frames: Arc<AtomicU64>,
    present_latency_us: Arc<AtomicU64>,
    present_fps: f32,
    present_latency_ms: f32,
    present_stats_at: Instant,
    source_fps: f32,
    capture_ms: f32,
    gpu_wait_ms: f32,
    gpu_lock_ms: f32,
    encode_ms: f32,
    send_ms: f32,
    encode_queue_ms: f32,
    copy_wait_ms: f32,
    convert_wait_ms: f32,
    map_ms: f32,
    submit_ms: f32,
    submit_api_ms: f32,
    submit_busy_wait_ms: f32,
    submit_busy_retries: u64,
    completion_wait_ms: f32,
    bitstream_ms: f32,
    bitstream_lock_ms: f32,
    bitstream_copy_ms: f32,
    bitstream_unlock_ms: f32,
    unmap_ms: f32,
    dxgi_timeouts: u64,
    dxgi_backlog: u64,
    dxgi_backlog_max: u64,
    pacing_skips: u64,
    slot_busy_skips: u64,
    preprocess_busy_skips: u64,
    no_free_slot_skips: u64,
    nvenc_submissions: u64,
    nvenc_completions: u64,
    video_send_failures: u64,
    cursor_only_frames: u64,
    stale_frames: u64,
    reassembly_ms: f32,
    decode_ms: f32,
    decode_queue_ms: f32,
    jitter_ms: f32,
    rtt_ms: f32,
    lost_frames: u64,
    server_stats: Option<ServerStats>,
    server_stats_received_at: Option<Instant>,
}

impl EchobindApp {
    pub fn new(creation_context: &eframe::CreationContext<'_>) -> Self {
        theme::install(&creation_context.egui_ctx);
        let audio_output_devices = DesktopSession::audio_output_devices().unwrap_or_default();
        let render_state = creation_context
            .wgpu_render_state
            .clone()
            .expect("Echobind requires the wgpu renderer");
        let presented_frames = Arc::new(AtomicU64::new(0));
        let present_latency_us = Arc::new(AtomicU64::new(0));
        #[cfg(target_os = "macos")]
        {
            let video_renderer = crate::video_renderer_macos::MacVideoRenderer::new(
                &render_state.device,
                render_state.target_format,
                presented_frames.clone(),
                present_latency_us.clone(),
            )
            .expect("Echobind requires Metal video-texture interop");
            render_state
                .renderer
                .write()
                .callback_resources
                .insert(video_renderer);
        }

        Self {
            mode: Mode::Connect,
            host_ip: "0.0.0.0".to_owned(),
            server_ip: "127.0.0.1".to_owned(),
            port_text: "3013".to_owned(),
            port: 3013,
            ip_error: None,
            port_error: None,
            frames_per_second: 60,
            // Native high-refresh game streaming needs substantially more than
            // 20 Mbps to avoid macroblocking during rapid full-screen motion.
            bitrate_mbps: 100,
            resolution: VideoResolution::Native,
            jumbo_datagrams: false,
            transport: "Standard MTU".to_owned(),
            notice: None,
            listen_address: None,
            capture_ready: false,
            target: None,
            peer: None,
            reconnecting: false,
            pending_peer: None,
            health_open: true,
            drawer_open: false,
            watch_audio_only: false,
            share_audio_only: false,
            watch_volume: 100,
            watch_muted: false,
            share_volume: 100,
            share_muted: false,
            peer_video: None,
            pending_video: true,
            session: None,
            shutdown: None,
            texture: None,
            nv12_ready: false,
            remote_cursor: None,
            render_state,
            stream_fps: 0.0,
            received_fps: 0.0,
            dropped_frames: 0,
            stream_mbps: 0.0,
            stream_width: 0,
            stream_height: 0,
            video_backend: "initializing".to_owned(),
            audio_output_devices,
            audio_output_device: None,
            audio_backend: "System default output".to_owned(),
            fullscreen: false,
            presented_frames,
            present_latency_us,
            present_fps: 0.0,
            present_latency_ms: 0.0,
            present_stats_at: Instant::now(),
            source_fps: 0.0,
            capture_ms: 0.0,
            gpu_wait_ms: 0.0,
            gpu_lock_ms: 0.0,
            encode_ms: 0.0,
            send_ms: 0.0,
            encode_queue_ms: 0.0,
            copy_wait_ms: 0.0,
            convert_wait_ms: 0.0,
            map_ms: 0.0,
            submit_ms: 0.0,
            submit_api_ms: 0.0,
            submit_busy_wait_ms: 0.0,
            submit_busy_retries: 0,
            completion_wait_ms: 0.0,
            bitstream_ms: 0.0,
            bitstream_lock_ms: 0.0,
            bitstream_copy_ms: 0.0,
            bitstream_unlock_ms: 0.0,
            unmap_ms: 0.0,
            dxgi_timeouts: 0,
            dxgi_backlog: 0,
            dxgi_backlog_max: 0,
            pacing_skips: 0,
            slot_busy_skips: 0,
            preprocess_busy_skips: 0,
            no_free_slot_skips: 0,
            nvenc_submissions: 0,
            nvenc_completions: 0,
            video_send_failures: 0,
            cursor_only_frames: 0,
            stale_frames: 0,
            reassembly_ms: 0.0,
            decode_ms: 0.0,
            decode_queue_ms: 0.0,
            jitter_ms: 0.0,
            rtt_ms: 0.0,
            lost_frames: 0,
            server_stats: None,
            server_stats_received_at: None,
        }
    }

    fn has_video(&self) -> bool {
        self.texture.is_some() || self.nv12_ready
    }

    fn watch_phase(&self) -> WatchPhase {
        if self.mode != Mode::Connect || self.session.is_none() {
            WatchPhase::Idle
        } else if self.has_video() {
            WatchPhase::Live
        } else if self.peer.is_some() && self.audio_only_session() {
            WatchPhase::Listening
        } else {
            WatchPhase::Waiting
        }
    }

    /// True when the current or next session carries no video: either side
    /// chose audio only, or the peer reported it has none.
    fn audio_only_session(&self) -> bool {
        let chosen = match self.mode {
            Mode::Connect => self.watch_audio_only,
            Mode::Host => self.share_audio_only,
        };
        chosen || self.peer_video == Some(false)
    }

    fn volume_gain(&self) -> f32 {
        match self.mode {
            Mode::Connect => volume_gain(self.watch_volume, self.watch_muted),
            Mode::Host => volume_gain(self.share_volume, self.share_muted),
        }
    }

    fn apply_volume(&self) {
        if let Some(session) = &self.session {
            session.set_volume(self.volume_gain());
        }
    }

    fn share_phase(&self) -> SharePhase {
        if self.mode != Mode::Host || self.session.is_none() {
            SharePhase::Idle
        } else if self.peer.is_some() {
            SharePhase::Live
        } else {
            SharePhase::Listening
        }
    }

    fn process_session_updates(&mut self, context: &egui::Context) {
        let stopped = self.shutdown.as_ref().is_some_and(|shutdown| {
            matches!(
                shutdown.complete.try_recv(),
                Ok(()) | Err(mpsc::TryRecvError::Disconnected)
            )
        });
        if stopped {
            if let Some(mut shutdown) = self.shutdown.take() {
                if let Some(handle) = shutdown.handle.take() {
                    let _ = handle.join();
                }
            }
        }
        let Some(session) = &self.session else {
            if self.shutdown.is_some() {
                context.request_repaint_after(std::time::Duration::from_millis(20));
            }
            return;
        };

        let events: Vec<_> = session.drain_events().collect();
        let newest_frame = session.take_latest_frame();
        let mut rejected = false;

        for event in events {
            match event {
                SessionEvent::Listening(address) => {
                    self.listen_address = Some(address);
                }
                SessionEvent::AwaitingApproval => {}
                SessionEvent::PendingConnection { peer, video } => {
                    self.pending_peer = Some(peer);
                    self.pending_video = video;
                }
                SessionEvent::PeerMedia { video } => {
                    self.peer_video = Some(video);
                }
                SessionEvent::Connected(peer) => {
                    self.peer = Some(peer);
                    self.pending_peer = None;
                    self.reconnecting = false;
                    self.notice = None;
                }
                SessionEvent::ConnectionRejected(reason) => {
                    self.notice = Some(Notice::new(Tone::Danger, "The host declined", reason));
                    self.server_stats = None;
                    self.server_stats_received_at = None;
                    rejected = true;
                }
                SessionEvent::Disconnected(reason) => {
                    let title = if self.mode == Mode::Host {
                        self.peer = None;
                        "Viewer disconnected"
                    } else {
                        self.reconnecting = true;
                        "Connection lost"
                    };
                    self.notice = Some(Notice::new(Tone::Caution, title, reason));
                    self.pending_peer = None;
                    self.server_stats = None;
                    self.server_stats_received_at = None;
                }
                SessionEvent::CaptureReady => {
                    self.capture_ready = true;
                }
                SessionEvent::VideoConfigured { width, height } => {
                    self.stream_width = width;
                    self.stream_height = height;
                }
                SessionEvent::VideoBackend(backend) => {
                    self.video_backend = backend;
                }
                SessionEvent::AudioBackend(backend) => {
                    self.audio_backend = backend;
                }
                SessionEvent::CursorPosition(position) => {
                    self.remote_cursor = Some(position);
                }
                SessionEvent::Stats(stats) => {
                    self.stream_fps = stats.fps;
                    self.received_fps = stats.fps;
                    self.stream_mbps = stats.megabits_per_second;
                    self.source_fps = stats.source_fps;
                    self.capture_ms = stats.capture_ms;
                    self.gpu_wait_ms = stats.gpu_wait_ms;
                    self.gpu_lock_ms = stats.gpu_lock_ms;
                    self.encode_ms = stats.encode_ms;
                    self.send_ms = stats.send_ms;
                    self.encode_queue_ms = stats.encode_queue_ms;
                    self.copy_wait_ms = stats.copy_wait_ms;
                    self.convert_wait_ms = stats.convert_wait_ms;
                    self.map_ms = stats.map_ms;
                    self.submit_ms = stats.submit_ms;
                    self.submit_api_ms = stats.submit_api_ms;
                    self.submit_busy_wait_ms = stats.submit_busy_wait_ms;
                    self.submit_busy_retries = stats.submit_busy_retries;
                    self.completion_wait_ms = stats.completion_wait_ms;
                    self.bitstream_ms = stats.bitstream_ms;
                    self.bitstream_lock_ms = stats.bitstream_lock_ms;
                    self.bitstream_copy_ms = stats.bitstream_copy_ms;
                    self.bitstream_unlock_ms = stats.bitstream_unlock_ms;
                    self.unmap_ms = stats.unmap_ms;
                    self.dxgi_timeouts = stats.dxgi_timeouts;
                    self.dxgi_backlog = stats.dxgi_backlog;
                    self.dxgi_backlog_max = stats.dxgi_backlog_max;
                    self.pacing_skips = stats.pacing_skips;
                    self.slot_busy_skips = stats.slot_busy_skips;
                    self.preprocess_busy_skips = stats.preprocess_busy_skips;
                    self.no_free_slot_skips = stats.no_free_slot_skips;
                    self.nvenc_submissions = stats.nvenc_submissions;
                    self.nvenc_completions = stats.nvenc_completions;
                    self.video_send_failures = stats.video_send_failures;
                    self.cursor_only_frames = stats.cursor_only_frames;
                    self.stale_frames = stats.stale_frames;
                }
                SessionEvent::ServerStats(stats) => {
                    self.server_stats = Some(stats);
                    self.server_stats_received_at = Some(Instant::now());
                }
                SessionEvent::ClientStats {
                    received_fps,
                    decoded_fps,
                    megabits_per_second,
                    dropped_frames,
                    reassembly_ms,
                    decode_ms,
                    decode_queue_ms,
                    jitter_ms,
                    rtt_ms,
                    lost_frames,
                } => {
                    self.received_fps = received_fps;
                    self.stream_fps = decoded_fps;
                    self.stream_mbps = megabits_per_second;
                    self.dropped_frames = dropped_frames;
                    self.reassembly_ms = reassembly_ms;
                    self.decode_ms = decode_ms;
                    self.decode_queue_ms = decode_queue_ms;
                    self.jitter_ms = jitter_ms;
                    self.rtt_ms = rtt_ms;
                    self.lost_frames = lost_frames;
                }
                SessionEvent::TransportConfigured { datagram_size } => {
                    self.transport = transport_label(datagram_size);
                }
                SessionEvent::Error(error) => {
                    self.notice = Some(Notice::new(Tone::Danger, "Something went wrong", error));
                }
            }
        }

        if let Some(frame) = newest_frame {
            self.stream_width = frame.width as u32;
            self.stream_height = frame.height as u32;
            self.upload_frame(frame);
        }
        let present_elapsed = self.present_stats_at.elapsed();
        if present_elapsed >= std::time::Duration::from_secs(1) {
            let frames = self.presented_frames.swap(0, Ordering::Relaxed);
            let latency_us = self.present_latency_us.swap(0, Ordering::Relaxed);
            self.present_fps = frames as f32 / present_elapsed.as_secs_f32();
            self.present_latency_ms = if frames == 0 {
                0.0
            } else {
                latency_us as f32 / frames as f32 / 1_000.0
            };
            self.present_stats_at = Instant::now();
        }

        // A rejected viewer's network thread has already exited; release the
        // socket and audio device instead of leaving a dead session running.
        if rejected {
            self.stop();
        }

        context.request_repaint_after(std::time::Duration::from_millis(100));
    }

    /// Validates the IP and port fields, flagging the one that is wrong.
    fn parse_fields(&mut self, ip: &str) -> Option<SocketAddr> {
        self.ip_error = None;
        self.port_error = None;
        let port = match parse_port(&self.port_text) {
            Ok(port) => port,
            Err(error) => {
                self.port_error = Some(error);
                return None;
            }
        };
        self.port = port;
        match parse_address(ip, port) {
            Ok(address) => Some(address),
            Err(error) => {
                self.ip_error = Some(error);
                None
            }
        }
    }

    fn start_host(&mut self) {
        if self.shutdown.is_some() {
            self.notice = Some(still_stopping_notice());
            return;
        }
        let host_ip = self.host_ip.clone();
        let Some(address) = self.parse_fields(&host_ip) else {
            return;
        };
        self.stop();
        self.notice = None;

        match DesktopSession::start_host(
            address,
            self.frames_per_second,
            self.bitrate_mbps.saturating_mul(1_000_000),
            self.resolution,
            self.jumbo_datagrams,
            !self.share_audio_only,
            self.volume_gain(),
        ) {
            Ok(session) => {
                self.session = Some(session);
            }
            Err(error) => {
                self.notice = Some(Notice::new(Tone::Danger, "Couldn't start sharing", error));
            }
        }
    }

    fn connect(&mut self, context: &egui::Context) {
        if self.shutdown.is_some() {
            self.notice = Some(still_stopping_notice());
            return;
        }
        let server_ip = self.server_ip.clone();
        let Some(address) = self.parse_fields(&server_ip) else {
            return;
        };
        self.stop();
        self.notice = None;

        let repaint_context = context.clone();
        let frame_notifier = Arc::new(move || repaint_context.request_repaint());
        match DesktopSession::start_client(
            address,
            self.audio_output_device.clone(),
            frame_notifier,
            self.watch_audio_only,
            self.volume_gain(),
        ) {
            Ok(session) => {
                self.target = Some(address);
                self.session = Some(session);
            }
            Err(error) => {
                self.notice = Some(Notice::new(Tone::Danger, "Couldn't connect", error));
            }
        }
    }

    fn stop(&mut self) {
        if let Some(mut session) = self.session.take() {
            let (complete_tx, complete_rx) = mpsc::channel();
            let handle = thread::Builder::new()
                .name("echobind-session-shutdown".to_owned())
                .spawn(move || {
                    session.stop();
                    let _ = complete_tx.send(());
                })
                .expect("unable to start the session shutdown worker");
            self.shutdown = Some(SessionShutdown {
                complete: complete_rx,
                handle: Some(handle),
            });
        }
        self.listen_address = None;
        self.capture_ready = false;
        self.target = None;
        self.peer = None;
        self.reconnecting = false;
        self.pending_peer = None;
        self.pending_video = true;
        self.peer_video = None;
        self.remote_cursor = None;
        self.clear_texture();
        self.stream_fps = 0.0;
        self.received_fps = 0.0;
        self.dropped_frames = 0;
        self.stream_mbps = 0.0;
        self.stream_width = 0;
        self.stream_height = 0;
        self.video_backend = "initializing".to_owned();
        self.audio_backend = "System default output".to_owned();
        self.present_fps = 0.0;
        self.present_latency_ms = 0.0;
        self.source_fps = 0.0;
        self.capture_ms = 0.0;
        self.gpu_wait_ms = 0.0;
        self.gpu_lock_ms = 0.0;
        self.encode_ms = 0.0;
        self.send_ms = 0.0;
        self.reassembly_ms = 0.0;
        self.decode_ms = 0.0;
        self.encode_queue_ms = 0.0;
        self.copy_wait_ms = 0.0;
        self.convert_wait_ms = 0.0;
        self.map_ms = 0.0;
        self.submit_ms = 0.0;
        self.submit_api_ms = 0.0;
        self.submit_busy_wait_ms = 0.0;
        self.submit_busy_retries = 0;
        self.completion_wait_ms = 0.0;
        self.bitstream_ms = 0.0;
        self.bitstream_lock_ms = 0.0;
        self.bitstream_copy_ms = 0.0;
        self.bitstream_unlock_ms = 0.0;
        self.unmap_ms = 0.0;
        self.decode_queue_ms = 0.0;
        self.jitter_ms = 0.0;
        self.rtt_ms = 0.0;
        self.lost_frames = 0;
        self.server_stats = None;
        self.server_stats_received_at = None;
        self.dxgi_timeouts = 0;
        self.dxgi_backlog = 0;
        self.dxgi_backlog_max = 0;
        self.pacing_skips = 0;
        self.slot_busy_skips = 0;
        self.preprocess_busy_skips = 0;
        self.no_free_slot_skips = 0;
        self.nvenc_submissions = 0;
        self.nvenc_completions = 0;
        self.video_send_failures = 0;
        self.cursor_only_frames = 0;
        self.stale_frames = 0;
        self.transport = "Standard MTU".to_owned();
    }

    fn accept_pending(&mut self) {
        if let (Some(peer), Some(session)) = (self.pending_peer, &self.session) {
            session.accept(peer);
        }
    }

    fn reject_pending(&mut self) {
        if let Some(peer) = self.pending_peer.take() {
            if let Some(session) = &self.session {
                session.reject(peer);
            }
        }
    }

    fn upload_frame(&mut self, frame: crate::session::DisplayFrame) {
        match frame.data {
            DisplayFrameData::Rgba(rgba) => {
                self.nv12_ready = false;
                self.upload_rgba(frame.width, frame.height, &rgba);
            }
            #[cfg(target_os = "macos")]
            DisplayFrameData::Nv12(pixel_buffer) => {
                let import_result = {
                    let mut renderer = self.render_state.renderer.write();
                    renderer
                        .callback_resources
                        .get_mut::<crate::video_renderer_macos::MacVideoRenderer>()
                        .ok_or_else(|| "Metal video renderer is unavailable".to_owned())
                        .and_then(|video| {
                            video.set_frame(
                                &self.render_state.device,
                                pixel_buffer,
                                frame.published_at,
                            )
                        })
                };
                match import_result {
                    Ok(()) => {
                        self.nv12_ready = true;
                        if let Some(texture) = self.texture.take() {
                            self.render_state.renderer.write().free_texture(&texture.id);
                        }
                    }
                    Err(error) => {
                        self.nv12_ready = false;
                        self.notice = Some(Notice::new(
                            Tone::Danger,
                            "Metal video import failed",
                            error,
                        ));
                    }
                }
            }
        }
    }

    fn upload_rgba(&mut self, width: usize, height: usize, rgba: &[u8]) {
        let needs_texture = self
            .texture
            .as_ref()
            .is_none_or(|texture| texture.width != width || texture.height != height);
        if needs_texture {
            self.clear_texture();
            let texture =
                self.render_state
                    .device
                    .create_texture(&eframe::wgpu::TextureDescriptor {
                        label: Some("echobind_remote_video"),
                        size: eframe::wgpu::Extent3d {
                            width: width as u32,
                            height: height as u32,
                            depth_or_array_layers: 1,
                        },
                        mip_level_count: 1,
                        sample_count: 1,
                        dimension: eframe::wgpu::TextureDimension::D2,
                        format: eframe::wgpu::TextureFormat::Rgba8Unorm,
                        usage: eframe::wgpu::TextureUsages::COPY_DST
                            | eframe::wgpu::TextureUsages::TEXTURE_BINDING,
                        view_formats: &[],
                    });
            let view = texture.create_view(&eframe::wgpu::TextureViewDescriptor::default());
            let id = self.render_state.renderer.write().register_native_texture(
                &self.render_state.device,
                &view,
                eframe::wgpu::FilterMode::Linear,
            );
            self.texture = Some(GpuVideoTexture {
                texture,
                id,
                width,
                height,
            });
        }

        let texture = self.texture.as_ref().expect("video texture was created");
        self.render_state.queue.write_texture(
            eframe::wgpu::TexelCopyTextureInfo {
                texture: &texture.texture,
                mip_level: 0,
                origin: eframe::wgpu::Origin3d::ZERO,
                aspect: eframe::wgpu::TextureAspect::All,
            },
            rgba,
            eframe::wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some((width * 4) as u32),
                rows_per_image: Some(height as u32),
            },
            eframe::wgpu::Extent3d {
                width: width as u32,
                height: height as u32,
                depth_or_array_layers: 1,
            },
        );
    }

    fn clear_texture(&mut self) {
        self.nv12_ready = false;
        if let Some(texture) = self.texture.take() {
            self.render_state.renderer.write().free_texture(&texture.id);
        }
        #[cfg(target_os = "macos")]
        if let Some(renderer) = self
            .render_state
            .renderer
            .write()
            .callback_resources
            .get_mut::<crate::video_renderer_macos::MacVideoRenderer>()
        {
            renderer.clear();
        }
    }

    fn refresh_audio_devices(&mut self) {
        match DesktopSession::audio_output_devices() {
            Ok(devices) => {
                self.audio_output_devices = devices;
            }
            Err(error) => {
                self.audio_backend = format!("Audio outputs unavailable: {error}");
            }
        }
    }

    fn set_audio_output(&mut self, device: Option<String>) {
        self.audio_output_device = device;
        if let Some(session) = &self.session {
            session.set_audio_output_device(self.audio_output_device.clone());
        }
    }

    fn set_fullscreen(&mut self, context: &egui::Context, fullscreen: bool) {
        self.fullscreen = fullscreen;
        context.send_viewport_cmd(egui::ViewportCommand::Fullscreen(fullscreen));
    }

    /// Paints the remote video letterboxed inside `area` and returns its rect.
    fn paint_video(&self, ui: &egui::Ui, area: egui::Rect) -> egui::Rect {
        if !self.has_video() || self.stream_width == 0 || self.stream_height == 0 {
            return egui::Rect::NOTHING;
        }
        let source = egui::vec2(self.stream_width as f32, self.stream_height as f32);
        let scale = (area.width() / source.x)
            .min(area.height() / source.y)
            .max(0.01);
        let video_rect = egui::Rect::from_center_size(area.center(), source * scale);
        if self.nv12_ready {
            #[cfg(target_os = "macos")]
            ui.painter().add(egui::Shape::Callback(
                crate::video_renderer_macos::paint_callback(video_rect),
            ));
        } else if let Some(texture) = &self.texture {
            egui::Image::from_texture((texture.id, source)).paint_at(ui, video_rect);
        }
        self.paint_remote_cursor(ui, video_rect);
        video_rect
    }

    fn paint_remote_cursor(&self, ui: &egui::Ui, video_rect: egui::Rect) {
        let Some(cursor) = self.remote_cursor.filter(|cursor| cursor.visible) else {
            return;
        };
        if self.mode != Mode::Connect
            || self.stream_width == 0
            || self.stream_height == 0
            || !video_rect.is_positive()
        {
            return;
        }

        let x = cursor.x as f32 / self.stream_width as f32;
        let y = cursor.y as f32 / self.stream_height as f32;
        let origin = egui::pos2(
            video_rect.left() + x * video_rect.width(),
            video_rect.top() + y * video_rect.height(),
        );
        if !video_rect.expand(24.0).contains(origin) {
            return;
        }

        // A small client-side arrow keeps cursor motion independent from the
        // video codec. DXGI reports the top-left of the hardware pointer, so
        // the arrow tip uses that exact streamed position.
        let points = vec![
            origin,
            origin + egui::vec2(1.5, 22.0),
            origin + egui::vec2(14.0, 14.0),
        ];
        ui.painter().add(egui::Shape::convex_polygon(
            points,
            egui::Color32::WHITE,
            egui::Stroke::new(2.0, egui::Color32::BLACK),
        ));
    }
}

impl eframe::App for EchobindApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let context = ui.ctx().clone();
        let toggle_fullscreen = context.input(|input| input.key_pressed(egui::Key::F11));
        let exit_fullscreen =
            self.fullscreen && context.input(|input| input.key_pressed(egui::Key::Escape));
        if toggle_fullscreen || exit_fullscreen {
            self.set_fullscreen(&context, !self.fullscreen);
        }
        self.process_session_updates(&context);

        if self.fullscreen {
            egui::CentralPanel::default()
                .frame(egui::Frame::new().fill(egui::Color32::BLACK))
                .show(ui, |ui| {
                    let area = ui.max_rect();
                    self.paint_video(ui, area);
                });
            self.show_fullscreen_stats(&context);
            return;
        }

        egui::Panel::top("top-bar")
            .resizable(false)
            .show_separator_line(true)
            .frame(
                egui::Frame::new()
                    .fill(theme::WINDOW)
                    .inner_margin(egui::Margin::symmetric(16, 12)),
            )
            .show(ui, |ui| self.show_top_bar(ui));
        egui::CentralPanel::default()
            .frame(
                egui::Frame::new()
                    .fill(theme::WINDOW)
                    .inner_margin(theme::GUTTER as i8),
            )
            .show(ui, |ui| self.show_body(ui));
    }
}

impl Drop for EchobindApp {
    fn drop(&mut self) {
        if let Some(mut session) = self.session.take() {
            session.stop();
        }
        if let Some(mut shutdown) = self.shutdown.take() {
            if let Some(handle) = shutdown.handle.take() {
                let _ = handle.join();
            }
        }
    }
}

fn still_stopping_notice() -> Notice {
    Notice::new(
        Tone::Caution,
        "Still stopping the last session",
        "Releasing the media devices and socket. Try again in a moment.",
    )
}

fn transport_label(datagram_size: usize) -> String {
    if datagram_size > 1400 {
        format!("Jumbo · {datagram_size} B")
    } else {
        "Standard MTU".to_owned()
    }
}

fn parse_address(ip: &str, port: u16) -> Result<SocketAddr, String> {
    let ip: IpAddr = ip
        .trim()
        .parse()
        .map_err(|_| "Enter a full IPv4 or IPv6 address.".to_owned())?;
    Ok(SocketAddr::new(ip, port))
}

/// Maps a 0–100 volume slider to a linear gain on a squared curve, which
/// follows perceived loudness more closely than a linear one.
fn volume_gain(percent: u32, muted: bool) -> f32 {
    if muted {
        return 0.0;
    }
    let level = percent.min(100) as f32 / 100.0;
    level * level
}

fn parse_port(port: &str) -> Result<u16, String> {
    match port.trim().parse::<u16>() {
        Ok(port) if port > 0 => Ok(port),
        _ => Err("Enter a port from 1 to 65535.".to_owned()),
    }
}

fn nvenc_busy_skips(total: u64, preprocess_full: u64, no_free_slot: u64) -> u64 {
    total.saturating_sub(preprocess_full.saturating_add(no_free_slot))
}

fn gpu_priority_label(class: u64) -> &'static str {
    match class {
        4 => "high",
        3 => "above-normal",
        2 => "normal",
        _ => "unknown",
    }
}

fn nvenc_slot_label(count: u64) -> String {
    if count == 0 {
        "unknown slots".to_owned()
    } else {
        format!("{count} slots")
    }
}

fn stale_server_stats_age(received_at: Option<Instant>, now: Instant) -> Option<Duration> {
    let age = now.saturating_duration_since(received_at?);
    (age >= SERVER_STATS_STALE_AFTER).then_some(age)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ipv4_and_ipv6_addresses() {
        assert_eq!(
            parse_address("127.0.0.1", 3013).unwrap(),
            "127.0.0.1:3013".parse().unwrap()
        );
        assert_eq!(
            parse_address("::1", 3013).unwrap(),
            "[::1]:3013".parse().unwrap()
        );
        assert!(parse_address("192.168.1", 3013).is_err());
    }

    #[test]
    fn parses_only_nonzero_ports() {
        assert_eq!(parse_port(" 3013 ").unwrap(), 3013);
        assert!(parse_port("0").is_err());
        assert!(parse_port("65536").is_err());
        assert!(parse_port("udp").is_err());
    }

    #[test]
    fn volume_slider_maps_to_a_squared_gain() {
        assert_eq!(volume_gain(100, false), 1.0);
        assert_eq!(volume_gain(50, false), 0.25);
        assert_eq!(volume_gain(0, false), 0.0);
        assert_eq!(volume_gain(150, false), 1.0);
        assert_eq!(volume_gain(100, true), 0.0);
    }

    #[test]
    fn labels_jumbo_transport_only_above_standard_datagrams() {
        assert_eq!(transport_label(1400), "Standard MTU");
        assert_eq!(transport_label(8192), "Jumbo · 8192 B");
    }

    #[test]
    fn isolates_nvenc_busy_skips_from_ring_saturation() {
        assert_eq!(nvenc_busy_skips(17, 5, 3), 9);
        assert_eq!(nvenc_busy_skips(2, 5, 3), 0);
    }

    #[test]
    fn server_stats_become_stale_only_after_the_snapshot_deadline() {
        let now = Instant::now();

        assert_eq!(stale_server_stats_age(None, now), None);
        assert_eq!(
            stale_server_stats_age(Some(now - SERVER_STATS_STALE_AFTER), now),
            Some(SERVER_STATS_STALE_AFTER),
        );
        assert_eq!(
            stale_server_stats_age(
                Some(now - SERVER_STATS_STALE_AFTER + Duration::from_millis(1)),
                now,
            ),
            None,
        );
    }
}
