use crate::session::{DesktopSession, DisplayFrameData, SessionEvent, VideoResolution};
use eframe::egui;
use std::{
    net::{IpAddr, SocketAddr},
    sync::{
        atomic::{AtomicU64, Ordering},
        mpsc, Arc,
    },
    thread,
    time::Instant,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    Host,
    Connect,
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
    port: u16,
    frames_per_second: u32,
    bitrate_mbps: u32,
    resolution: VideoResolution,
    jumbo_datagrams: bool,
    transport: String,
    status: String,
    pending_peer: Option<SocketAddr>,
    session: Option<DesktopSession>,
    shutdown: Option<SessionShutdown>,
    texture: Option<GpuVideoTexture>,
    nv12_ready: bool,
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
    capture_ms: f32,
    encode_ms: f32,
    send_ms: f32,
    encode_queue_ms: f32,
    reassembly_ms: f32,
    decode_ms: f32,
    decode_queue_ms: f32,
    jitter_ms: f32,
    rtt_ms: f32,
    lost_frames: u64,
}

impl EchobindApp {
    pub fn new(creation_context: &eframe::CreationContext<'_>) -> Self {
        creation_context.egui_ctx.set_theme(egui::Theme::Dark);
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
            port: 3013,
            frames_per_second: 60,
            // Native high-refresh game streaming needs substantially more than
            // 20 Mbps to avoid macroblocking during rapid full-screen motion.
            bitrate_mbps: 100,
            resolution: VideoResolution::Native,
            jumbo_datagrams: false,
            transport: "standard MTU".to_owned(),
            status: "Ready".to_owned(),
            pending_peer: None,
            session: None,
            shutdown: None,
            texture: None,
            nv12_ready: false,
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
            capture_ms: 0.0,
            encode_ms: 0.0,
            send_ms: 0.0,
            encode_queue_ms: 0.0,
            reassembly_ms: 0.0,
            decode_ms: 0.0,
            decode_queue_ms: 0.0,
            jitter_ms: 0.0,
            rtt_ms: 0.0,
            lost_frames: 0,
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
            self.status = "Ready".to_owned();
        }
        let Some(session) = &self.session else {
            if self.shutdown.is_some() {
                context.request_repaint_after(std::time::Duration::from_millis(20));
            }
            return;
        };

        let events: Vec<_> = session.drain_events().collect();
        let newest_frame = session.take_latest_frame();

        for event in events {
            match event {
                SessionEvent::Listening(address) => {
                    self.status = format!("Listening on {address}");
                }
                SessionEvent::AwaitingApproval => {
                    self.status = "Waiting for the host to accept…".to_owned();
                }
                SessionEvent::PendingConnection(peer) => {
                    self.status = format!("{peer} wants to connect");
                    self.pending_peer = Some(peer);
                }
                SessionEvent::Connected(peer) => {
                    self.status = format!("Connected to {peer}");
                    self.pending_peer = None;
                }
                SessionEvent::ConnectionRejected(reason) => {
                    self.status = format!("Connection rejected: {reason}");
                }
                SessionEvent::Disconnected(reason) => {
                    self.status = format!("Disconnected: {reason}");
                    self.pending_peer = None;
                }
                SessionEvent::CaptureReady => {
                    if self.mode == Mode::Host && self.pending_peer.is_none() {
                        self.status = format!("{} · capture ready", self.status);
                    }
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
                SessionEvent::Stats {
                    fps,
                    megabits_per_second,
                    capture_ms,
                    encode_ms,
                    send_ms,
                    encode_queue_ms,
                } => {
                    self.stream_fps = fps;
                    self.received_fps = fps;
                    self.stream_mbps = megabits_per_second;
                    self.capture_ms = capture_ms;
                    self.encode_ms = encode_ms;
                    self.send_ms = send_ms;
                    self.encode_queue_ms = encode_queue_ms;
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
                    self.transport = if datagram_size > 1400 {
                        format!("jumbo {datagram_size}-byte UDP")
                    } else {
                        "standard MTU".to_owned()
                    };
                }
                SessionEvent::Error(error) => {
                    self.status = format!("Error: {error}");
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

        context.request_repaint_after(std::time::Duration::from_millis(100));
    }

    fn start_host(&mut self) {
        if self.shutdown.is_some() {
            self.status = "Waiting for the previous session to finish stopping…".to_owned();
            return;
        }
        let address = match parse_address(&self.host_ip, self.port) {
            Ok(address) => address,
            Err(error) => {
                self.status = error;
                return;
            }
        };
        self.stop();

        match DesktopSession::start_host(
            address,
            self.frames_per_second,
            self.bitrate_mbps.saturating_mul(1_000_000),
            self.resolution,
            self.jumbo_datagrams,
        ) {
            Ok(session) => {
                self.status = format!("Starting server on {address}…");
                self.session = Some(session);
            }
            Err(error) => {
                self.status = format!("Unable to start server: {error}");
            }
        }
    }

    fn connect(&mut self, context: &egui::Context) {
        if self.shutdown.is_some() {
            self.status = "Waiting for the previous session to finish stopping…".to_owned();
            return;
        }
        let address = match parse_address(&self.server_ip, self.port) {
            Ok(address) => address,
            Err(error) => {
                self.status = error;
                return;
            }
        };
        self.stop();

        let repaint_context = context.clone();
        let frame_notifier = Arc::new(move || repaint_context.request_repaint());
        match DesktopSession::start_client(
            address,
            self.audio_output_device.clone(),
            frame_notifier,
        ) {
            Ok(session) => {
                self.status = format!("Requesting connection to {address}…");
                self.session = Some(session);
            }
            Err(error) => {
                self.status = format!("Unable to connect: {error}");
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
        self.pending_peer = None;
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
        self.capture_ms = 0.0;
        self.encode_ms = 0.0;
        self.send_ms = 0.0;
        self.reassembly_ms = 0.0;
        self.decode_ms = 0.0;
        self.encode_queue_ms = 0.0;
        self.decode_queue_ms = 0.0;
        self.jitter_ms = 0.0;
        self.rtt_ms = 0.0;
        self.lost_frames = 0;
        self.transport = "standard MTU".to_owned();
        self.status = if self.shutdown.is_some() {
            "Stopping session and releasing media devices and socket…".to_owned()
        } else {
            "Ready".to_owned()
        };
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
                        self.status = format!("Metal video import failed: {error}");
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

    fn show_controls(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.selectable_value(&mut self.mode, Mode::Host, "Create server");
            ui.selectable_value(&mut self.mode, Mode::Connect, "Connect");
            ui.separator();
            ui.label(&self.status);
        });
        ui.add_space(8.0);

        ui.horizontal(|ui| {
            match self.mode {
                Mode::Host => {
                    ui.label("Bind IP");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.host_ip)
                            .desired_width(150.0)
                            .hint_text("0.0.0.0"),
                    );
                }
                Mode::Connect => {
                    ui.label("Server IP");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.server_ip)
                            .desired_width(150.0)
                            .hint_text("192.168.1.10"),
                    );
                }
            }

            ui.label("Port");
            ui.add(egui::DragValue::new(&mut self.port).range(1..=u16::MAX));

            if self.mode == Mode::Host {
                ui.label("Resolution");
                egui::ComboBox::from_id_salt("stream-resolution")
                    .selected_text(self.resolution.label())
                    .show_ui(ui, |ui| {
                        ui.selectable_value(
                            &mut self.resolution,
                            VideoResolution::Native,
                            "Native",
                        );
                        ui.selectable_value(&mut self.resolution, VideoResolution::P720, "720p");
                        ui.selectable_value(&mut self.resolution, VideoResolution::P1080, "1080p");
                    });
                ui.label("FPS");
                ui.add(egui::DragValue::new(&mut self.frames_per_second).range(15..=120));
                ui.label("Mbps");
                ui.add(egui::DragValue::new(&mut self.bitrate_mbps).range(1..=200));
                ui.checkbox(&mut self.jumbo_datagrams, "Jumbo MTU 9000")
                    .on_hover_text(
                        "Only enable when every LAN hop supports a 9000-byte MTU; negotiation falls back to standard MTU for older clients.",
                    );
            }

            let active = self.session.is_some();
            let stopping = self.shutdown.is_some();
            if !active {
                let label = if self.mode == Mode::Host {
                    "Start server"
                } else {
                    "Connect"
                };
                if ui.add_enabled(!stopping, egui::Button::new(label)).clicked() {
                    match self.mode {
                        Mode::Host => self.start_host(),
                        Mode::Connect => self.connect(ui.ctx()),
                    }
                }
            } else if ui.button("Stop").clicked() {
                self.stop();
            }

            if (self.texture.is_some() || self.nv12_ready) && ui.button("Fullscreen").clicked() {
                self.set_fullscreen(ui.ctx(), true);
            }
        });

        if self.mode == Mode::Connect {
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.label("Audio output");
                let selected_text = self
                    .audio_output_device
                    .as_deref()
                    .unwrap_or("System default");
                let mut changed = false;
                egui::ComboBox::from_id_salt("audio-output-device")
                    .selected_text(selected_text)
                    .width(260.0)
                    .show_ui(ui, |ui| {
                        changed |= ui
                            .selectable_value(&mut self.audio_output_device, None, "System default")
                            .changed();
                        for device in &self.audio_output_devices {
                            changed |= ui
                                .selectable_value(
                                    &mut self.audio_output_device,
                                    Some(device.clone()),
                                    device,
                                )
                                .changed();
                        }
                    });
                if changed {
                    if let Some(session) = &self.session {
                        session.set_audio_output_device(self.audio_output_device.clone());
                    }
                }
                if ui.small_button("Refresh").clicked() {
                    self.refresh_audio_devices();
                }
                if self.session.is_some() {
                    ui.separator();
                    ui.label(&self.audio_backend);
                }
            });
        }

        if let Some(peer) = self.pending_peer {
            ui.add_space(8.0);
            egui::Frame::group(ui.style()).show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(format!("Allow {peer} to view this screen?"));
                    if ui.button("Accept").clicked() {
                        if let Some(session) = &self.session {
                            session.accept(peer);
                        }
                    }
                    if ui.button("Reject").clicked() {
                        if let Some(session) = &self.session {
                            session.reject(peer);
                        }
                        self.pending_peer = None;
                        self.status = format!("Rejected {peer}");
                    }
                });
            });
        }

        if self.stream_fps > 0.0 || self.stream_mbps > 0.0 {
            ui.add_space(6.0);
            let resolution = if self.stream_width > 0 && self.stream_height > 0 {
                format!("{}×{}", self.stream_width, self.stream_height)
            } else {
                self.resolution.label().to_owned()
            };
            if self.mode == Mode::Host {
                ui.label(format!(
                    "{:.1} FPS sent · {:.2} Mbps · {resolution} H.264 · {} · {} · {}",
                    self.stream_fps,
                    self.stream_mbps,
                    self.video_backend,
                    self.audio_backend,
                    self.transport,
                ));
                ui.label(format!(
                    "pipeline avg: capture {:.2} ms · encode {:.2} ms · encode→send {:.2} ms · send {:.2} ms",
                    self.capture_ms, self.encode_ms, self.encode_queue_ms, self.send_ms,
                ));
            } else {
                ui.label(format!(
                    "{:.1} decoded · {:.1} received · {:.1} presented · {} dropped / {} lost · {:.2} Mbps · {resolution} H.264 · {} · {} · {}",
                    self.stream_fps,
                    self.received_fps,
                    self.present_fps,
                    self.dropped_frames,
                    self.lost_frames,
                    self.stream_mbps,
                    self.video_backend,
                    self.audio_backend,
                    self.transport,
                ));
                ui.label(format!(
                    "pipeline avg: reassemble {:.2} ms · decode queue {:.2} ms · decode {:.2} ms · repaint→present {:.2} ms · jitter {:.2} ms · RTT {:.2} ms",
                    self.reassembly_ms, self.decode_queue_ms, self.decode_ms, self.present_latency_ms, self.jitter_ms, self.rtt_ms,
                ));
            }
        }
    }

    fn set_fullscreen(&mut self, context: &egui::Context, fullscreen: bool) {
        self.fullscreen = fullscreen;
        context.send_viewport_cmd(egui::ViewportCommand::Fullscreen(fullscreen));
    }

    fn show_video(&self, ui: &mut egui::Ui) {
        if self.texture.is_none() && !self.nv12_ready {
            ui.centered_and_justified(|ui| {
                ui.label(if self.mode == Mode::Host {
                    "Start a server and accept a viewer to begin sharing."
                } else {
                    "The remote screen will appear here after the host accepts."
                });
            });
            return;
        }

        let available = ui.available_size();
        let source = egui::vec2(self.stream_width as f32, self.stream_height as f32);
        let scale = (available.x / source.x)
            .min(available.y / source.y)
            .max(0.01);
        let display_size = source * scale;
        ui.centered_and_justified(|ui| {
            if self.nv12_ready {
                let (_rect, _) = ui.allocate_exact_size(display_size, egui::Sense::hover());
                #[cfg(target_os = "macos")]
                ui.painter().add(egui::Shape::Callback(
                    crate::video_renderer_macos::paint_callback(_rect),
                ));
            } else if let Some(texture) = &self.texture {
                ui.add(
                    egui::Image::from_texture((texture.id, source)).fit_to_exact_size(display_size),
                );
            }
        });
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

        if !self.fullscreen {
            egui::Panel::top("controls")
                .resizable(false)
                .show(ui, |ui| {
                    ui.add_space(10.0);
                    self.show_controls(ui);
                    ui.add_space(10.0);
                });
        }

        egui::CentralPanel::default().show(ui, |ui| {
            self.show_video(ui);
        });
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

fn parse_address(ip: &str, port: u16) -> Result<SocketAddr, String> {
    let ip: IpAddr = ip
        .trim()
        .parse()
        .map_err(|_| format!("Invalid IP address: {ip}"))?;
    Ok(SocketAddr::new(ip, port))
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
    }
}
