use crate::session::{DesktopSession, SessionEvent, VideoResolution};
use eframe::egui;
use std::net::{IpAddr, SocketAddr};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    Host,
    Connect,
}

pub struct EchobindApp {
    mode: Mode,
    host_ip: String,
    server_ip: String,
    port: u16,
    frames_per_second: u32,
    bitrate_mbps: u32,
    resolution: VideoResolution,
    status: String,
    pending_peer: Option<SocketAddr>,
    session: Option<DesktopSession>,
    texture: Option<egui::TextureHandle>,
    stream_fps: f32,
    stream_mbps: f32,
    stream_width: u32,
    stream_height: u32,
    video_backend: String,
    fullscreen: bool,
}

impl EchobindApp {
    pub fn new(creation_context: &eframe::CreationContext<'_>) -> Self {
        creation_context.egui_ctx.set_theme(egui::Theme::Dark);

        Self {
            mode: Mode::Connect,
            host_ip: "0.0.0.0".to_owned(),
            server_ip: "127.0.0.1".to_owned(),
            port: 3013,
            frames_per_second: 60,
            bitrate_mbps: 6,
            resolution: VideoResolution::P720,
            status: "Ready".to_owned(),
            pending_peer: None,
            session: None,
            texture: None,
            stream_fps: 0.0,
            stream_mbps: 0.0,
            stream_width: 0,
            stream_height: 0,
            video_backend: "initializing".to_owned(),
            fullscreen: false,
        }
    }

    fn process_session_updates(&mut self, context: &egui::Context) {
        let Some(session) = &self.session else {
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
                SessionEvent::Stats {
                    fps,
                    megabits_per_second,
                } => {
                    self.stream_fps = fps;
                    self.stream_mbps = megabits_per_second;
                }
                SessionEvent::Error(error) => {
                    self.status = format!("Error: {error}");
                }
            }
        }

        if let Some(frame) = newest_frame {
            self.stream_width = frame.width as u32;
            self.stream_height = frame.height as u32;
            let image =
                egui::ColorImage::from_rgba_unmultiplied([frame.width, frame.height], &frame.rgba);
            if let Some(texture) = &mut self.texture {
                texture.set(image, egui::TextureOptions::LINEAR);
            } else {
                self.texture =
                    Some(context.load_texture("remote-video", image, egui::TextureOptions::LINEAR));
            }
        }

        context.request_repaint_after(std::time::Duration::from_millis(8));
    }

    fn start_host(&mut self) {
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

    fn connect(&mut self) {
        let address = match parse_address(&self.server_ip, self.port) {
            Ok(address) => address,
            Err(error) => {
                self.status = error;
                return;
            }
        };
        self.stop();

        match DesktopSession::start_client(address) {
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
            session.stop();
        }
        self.pending_peer = None;
        self.texture = None;
        self.stream_fps = 0.0;
        self.stream_mbps = 0.0;
        self.stream_width = 0;
        self.stream_height = 0;
        self.video_backend = "initializing".to_owned();
        self.status = "Ready".to_owned();
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
                        ui.selectable_value(&mut self.resolution, VideoResolution::P720, "720p");
                        ui.selectable_value(&mut self.resolution, VideoResolution::P1080, "1080p");
                    });
                ui.label("FPS");
                ui.add(egui::DragValue::new(&mut self.frames_per_second).range(15..=60));
                ui.label("Mbps");
                ui.add(egui::DragValue::new(&mut self.bitrate_mbps).range(1..=20));
            }

            let active = self.session.is_some();
            if !active {
                let label = if self.mode == Mode::Host {
                    "Start server"
                } else {
                    "Connect"
                };
                if ui.button(label).clicked() {
                    match self.mode {
                        Mode::Host => self.start_host(),
                        Mode::Connect => self.connect(),
                    }
                }
            } else if ui.button("Stop").clicked() {
                self.stop();
            }

            if self.texture.is_some() && ui.button("Fullscreen").clicked() {
                self.set_fullscreen(ui.ctx(), true);
            }
        });

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
            ui.label(format!(
                "{:.1} FPS · {:.2} Mbps · {resolution} H.264 · {}",
                self.stream_fps, self.stream_mbps, self.video_backend,
            ));
        }
    }

    fn set_fullscreen(&mut self, context: &egui::Context, fullscreen: bool) {
        self.fullscreen = fullscreen;
        context.send_viewport_cmd(egui::ViewportCommand::Fullscreen(fullscreen));
    }

    fn show_video(&self, ui: &mut egui::Ui) {
        let Some(texture) = &self.texture else {
            ui.centered_and_justified(|ui| {
                ui.label(if self.mode == Mode::Host {
                    "Start a server and accept a viewer to begin sharing."
                } else {
                    "The remote screen will appear here after the host accepts."
                });
            });
            return;
        };

        let available = ui.available_size();
        let source = texture.size_vec2();
        let scale = (available.x / source.x)
            .min(available.y / source.y)
            .max(0.01);
        let display_size = source * scale;
        ui.centered_and_justified(|ui| {
            ui.add(egui::Image::from_texture(texture).fit_to_exact_size(display_size));
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
