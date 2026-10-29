use super::{
    gpu_priority_label, nvenc_busy_skips, nvenc_slot_label, stale_server_stats_age, EchobindApp,
    Mode, SharePhase, WatchPhase,
};
use crate::{
    icons::{self, Icon},
    session::VideoResolution,
    theme::{self, Tone, Weight},
    widgets::{self, ButtonKind, Dot, IconButtonKind, Segment, SegmentTrack, StepState},
};
use eframe::egui::{
    self, pos2, vec2, Align, Align2, Color32, CornerRadius, Id, Layout, Margin, Rect, RectAlign,
    Response, Sense, Stroke, StrokeKind, Ui, UiBuilder, Vec2,
};
use std::time::Instant;

const FRAME_RATES: [u32; 4] = [30, 60, 90, 120];
const TOOLBAR_FILL: Color32 = Color32::from_rgba_premultiplied(12, 13, 16, 240);
const HUD_FILL: Color32 = Color32::from_rgba_premultiplied(8, 8, 10, 240);
const DIM_FILL: Color32 = Color32::from_rgba_premultiplied(7, 7, 9, 209);

fn child<R>(ui: &mut Ui, rect: Rect, add: impl FnOnce(&mut Ui) -> R) -> R {
    ui.scope_builder(
        UiBuilder::new()
            .max_rect(rect)
            .layout(Layout::top_down(Align::Min)),
        add,
    )
    .inner
}

fn scroll_child(ui: &mut Ui, rect: Rect, id_salt: &str, add: impl FnOnce(&mut Ui)) {
    child(ui, rect, |ui| {
        egui::ScrollArea::vertical()
            .id_salt(id_salt)
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                add(ui);
            });
    });
}

fn paint_surface(ui: &Ui, rect: Rect, fill: Color32) {
    ui.painter().rect(
        rect,
        CornerRadius::same(theme::RADIUS_SURFACE),
        fill,
        Stroke::new(1.0, theme::DIVIDER),
        StrokeKind::Inside,
    );
}

/// Paints a card and splits its padded interior into a scrolling body and a
/// footer pinned to the bottom edge.
fn card_rects(ui: &Ui, rect: Rect, footer_height: f32) -> (Rect, Rect) {
    paint_surface(ui, rect, theme::SURFACE);
    let inner = rect.shrink(theme::CARD_PADDING);
    let footer = Rect::from_min_max(
        pos2(inner.left(), inner.bottom() - footer_height),
        inner.max,
    );
    let body = Rect::from_min_max(
        inner.min,
        pos2(inner.right(), footer.top() - theme::SECTION_GAP),
    );
    (body, footer)
}

fn split_left(area: Rect, width: f32) -> (Rect, Rect) {
    let left = Rect::from_min_size(area.min, vec2(width, area.height()));
    let right = Rect::from_min_max(pos2(left.right() + theme::GUTTER, area.top()), area.max);
    (left, right)
}

fn split_right(area: Rect, width: f32) -> (Rect, Rect) {
    let right = Rect::from_min_max(pos2(area.right() - width, area.top()), area.max);
    let left = Rect::from_min_max(area.min, pos2(right.left() - theme::GUTTER, area.bottom()));
    (left, right)
}

/// Vertically centers a block whose height is only known after layout, using
/// the height measured on the previous pass.
fn centered_block(ui: &mut Ui, rect: Rect, id_salt: &str, width: f32, add: impl FnOnce(&mut Ui)) {
    let id = Id::new(("echobind-centered", id_salt));
    let previous = ui.data(|data| data.get_temp::<f32>(id));
    let height = previous.unwrap_or(0.0);
    let top = (rect.center().y - height / 2.0).max(rect.top());
    let width = width.min(rect.width());
    let block = Rect::from_min_max(
        pos2(rect.center().x - width / 2.0, top),
        pos2(rect.center().x + width / 2.0, rect.bottom()),
    );
    let response = ui.scope_builder(
        UiBuilder::new()
            .max_rect(block)
            .layout(Layout::top_down(Align::Center)),
        |ui| {
            ui.set_width(width);
            add(ui);
        },
    );
    let measured = response.response.rect.height();
    if previous.is_none_or(|previous| (previous - measured).abs() > 0.5) {
        ui.data_mut(|data| data.insert_temp(id, measured));
        ui.ctx().request_discard("centered block height changed");
    }
}

/// Lays out a column of fixed width centered in `area`, scrolling when tall.
fn compact_column(ui: &mut Ui, area: Rect, id_salt: &str, width: f32, add: impl FnOnce(&mut Ui)) {
    child(ui, area, |ui| {
        egui::ScrollArea::vertical()
            .id_salt(id_salt)
            .auto_shrink([false, false])
            .show(ui, |ui| {
                let width = width.min(ui.available_width());
                let margin = ((ui.available_width() - width) / 2.0).max(0.0);
                ui.horizontal_top(|ui| {
                    ui.add_space(margin);
                    ui.vertical(|ui| {
                        ui.set_width(width);
                        add(ui);
                    });
                });
            });
    });
}

fn card(ui: &mut Ui, add: impl FnOnce(&mut Ui)) {
    theme::card_frame().show(ui, |ui| {
        ui.set_width(ui.available_width());
        add(ui);
    });
}

fn paint_stage(ui: &Ui, rect: Rect) {
    paint_surface(ui, rect, theme::STAGE);
}

/// Rounds off video that reaches the stage edges by painting the window
/// color over its corners, then redraws the stage outline.
fn mask_stage_corners(ui: &Ui, stage: Rect) {
    let radius = f32::from(theme::RADIUS_SURFACE);
    ui.painter().rect_stroke(
        stage.expand(radius / 2.0),
        CornerRadius::same(theme::RADIUS_SURFACE + theme::RADIUS_SURFACE / 2),
        Stroke::new(radius, theme::WINDOW),
        StrokeKind::Middle,
    );
    ui.painter().rect_stroke(
        stage,
        CornerRadius::same(theme::RADIUS_SURFACE),
        Stroke::new(1.0, theme::DIVIDER),
        StrokeKind::Inside,
    );
}

fn panel_header(ui: &mut Ui, title: &str, body: &str) {
    widgets::stack(ui, 4.0, |ui| {
        widgets::title(ui, title);
        widgets::muted(ui, body, 14.0);
    });
}

fn section(ui: &mut Ui, label: &str, add: impl FnOnce(&mut Ui)) {
    widgets::stack(ui, 12.0, |ui| {
        widgets::overline(ui, label);
        widgets::stack(ui, theme::FIELD_GAP, add);
    });
}

fn stage_message(ui: &mut Ui, title: &str, body: &str) {
    widgets::stack(ui, 8.0, |ui| {
        ui.label(widgets::text(title, 18.0, Weight::SemiBold, theme::TEXT));
        widgets::muted(ui, body, 14.0);
    });
}

fn resolution_label(resolution: VideoResolution, width: u32, height: u32) -> String {
    if width > 0 && height > 0 {
        format!("{} · {width}×{height}", resolution.label())
    } else {
        resolution.label().to_owned()
    }
}

impl EchobindApp {
    fn compact(context: &egui::Context) -> bool {
        context.content_rect().width() < theme::COMPACT_BREAKPOINT
    }

    fn status(&self) -> (Tone, &'static str, Option<String>, Dot) {
        if self.shutdown.is_some() {
            return (Tone::Neutral, "Stopping…", None, Dot::Hollow);
        }
        if self.session.is_none() {
            return (Tone::Neutral, "Ready", None, Dot::Solid);
        }
        match self.mode {
            Mode::Connect if self.reconnecting => (Tone::Caution, "Reconnecting", None, Dot::Pulse),
            Mode::Connect if self.has_video() => (
                Tone::Signal,
                "Connected",
                self.target.map(|target| target.to_string()),
                Dot::Solid,
            ),
            Mode::Connect if self.peer.is_some() && self.audio_only_session() => (
                Tone::Signal,
                "Listening",
                self.target.map(|target| target.to_string()),
                Dot::Solid,
            ),
            Mode::Connect => (Tone::Caution, "Waiting for host", None, Dot::Pulse),
            Mode::Host if self.pending_peer.is_some() => {
                (Tone::Caution, "1 request", None, Dot::Pulse)
            }
            Mode::Host if self.peer.is_some() && self.audio_only_session() => {
                (Tone::Signal, "Live · 1 listener", None, Dot::Solid)
            }
            Mode::Host if self.peer.is_some() => {
                (Tone::Signal, "Live · 1 viewer", None, Dot::Solid)
            }
            Mode::Host if self.listen_address.is_some() => {
                (Tone::Signal, "Listening", None, Dot::Solid)
            }
            Mode::Host => (Tone::Neutral, "Starting…", None, Dot::Hollow),
        }
    }

    pub(super) fn show_top_bar(&mut self, ui: &mut Ui) {
        let compact = Self::compact(ui.ctx());
        let row = Rect::from_min_size(
            ui.max_rect().min,
            vec2(ui.available_width(), theme::CONTROL_HEIGHT),
        );
        ui.allocate_rect(row, Sense::hover());

        let logo = Rect::from_min_size(pos2(row.left(), row.center().y - 16.0), Vec2::splat(32.0));
        ui.painter().rect_filled(
            logo,
            CornerRadius::same(theme::RADIUS_CONTROL),
            theme::tint(theme::SIGNAL, 0.12),
        );
        icons::paint(
            ui.painter(),
            Rect::from_center_size(logo.center(), Vec2::splat(18.0)),
            Icon::Logo,
            theme::SIGNAL,
        );
        ui.painter().text(
            pos2(logo.right() + 10.0, row.center().y),
            Align2::LEFT_CENTER,
            "Echobind",
            theme::inter(15.0, Weight::SemiBold),
            theme::TEXT,
        );

        let locked = self.session.is_some();
        let mut mode = self.mode;
        child(ui, row, |ui| {
            ui.with_layout(Layout::top_down(Align::Center), |ui| {
                let segments = [
                    Segment::new(Mode::Connect, "Watch")
                        .icon(Icon::Eye)
                        .enabled(!locked || self.mode == Mode::Connect),
                    Segment::new(Mode::Host, "Share")
                        .icon(Icon::Share)
                        .enabled(!locked || self.mode == Mode::Host),
                ];
                widgets::segmented(ui, "mode", &mut mode, &segments, SegmentTrack::Bar);
            });
        });
        if mode != self.mode {
            self.mode = mode;
            self.notice = None;
            self.ip_error = None;
            self.port_error = None;
        }

        let (tone, label, detail, dot) = self.status();
        child(ui, row, |ui| {
            ui.with_layout(Layout::top_down(Align::Max), |ui| {
                ui.add_space((theme::CONTROL_HEIGHT - theme::PILL_HEIGHT) / 2.0);
                let label = if compact && detail.is_some() {
                    ""
                } else {
                    label
                };
                widgets::status_pill(ui, tone, label, detail.as_deref(), dot);
            });
        });
    }

    pub(super) fn show_body(&mut self, ui: &mut Ui) {
        let area = ui.available_rect_before_wrap();
        let compact = Self::compact(ui.ctx());
        match self.mode {
            Mode::Connect => self.show_watch(ui, area, compact),
            Mode::Host => self.show_share(ui, area, compact),
        }
    }

    fn show_notice(&self, ui: &mut Ui) {
        if let Some(notice) = &self.notice {
            widgets::banner(ui, notice.tone, &notice.title, &notice.body);
        }
    }

    fn address_fields(&mut self, ui: &mut Ui, ip_label: &str, ip_hint: &str) {
        widgets::stack(ui, theme::LABEL_GAP, |ui| {
            ui.horizontal_top(|ui| {
                ui.spacing_mut().item_spacing.x = 12.0;
                let port_width = 96.0;
                let ip_width = ui.available_width() - port_width - 12.0;
                let ip = if self.mode == Mode::Host {
                    &mut self.host_ip
                } else {
                    &mut self.server_ip
                };
                let ip_invalid = self.ip_error.is_some();
                let port_invalid = self.port_error.is_some();
                let mut ip_changed = false;
                let mut port_changed = false;
                ui.vertical(|ui| {
                    ui.set_width(ip_width);
                    widgets::stack(ui, theme::LABEL_GAP, |ui| {
                        widgets::field_label(ui, ip_label);
                        ip_changed =
                            widgets::text_field(ui, ip, ip_hint, ip_width, ip_invalid).changed();
                    });
                });
                ui.vertical(|ui| {
                    ui.set_width(port_width);
                    widgets::stack(ui, theme::LABEL_GAP, |ui| {
                        widgets::field_label(ui, "Port");
                        port_changed = widgets::text_field(
                            ui,
                            &mut self.port_text,
                            "3013",
                            port_width,
                            port_invalid,
                        )
                        .changed();
                    });
                });
                if ip_changed {
                    self.ip_error = None;
                }
                if port_changed {
                    self.port_error = None;
                }
            });
            if let Some(error) = self.ip_error.as_ref().or(self.port_error.as_ref()) {
                ui.label(widgets::text(error, 12.0, Weight::Regular, theme::DANGER));
            }
        });
    }

    fn audio_output_menu(&mut self, anchor: &Response, align: RectAlign) {
        let mut chosen = None;
        egui::Popup::menu(anchor)
            .align(align)
            .width(anchor.rect.width().max(240.0))
            .show(|ui| {
                ui.spacing_mut().item_spacing.y = 2.0;
                if widgets::menu_row(ui, "System default", self.audio_output_device.is_none())
                    .clicked()
                {
                    chosen = Some(None);
                }
                for device in &self.audio_output_devices {
                    let selected = self.audio_output_device.as_deref() == Some(device.as_str());
                    if widgets::menu_row(ui, device, selected).clicked() {
                        chosen = Some(Some(device.clone()));
                    }
                }
            });
        if let Some(device) = chosen {
            self.set_audio_output(device);
        }
    }

    fn audio_output_field(&mut self, ui: &mut Ui) {
        widgets::stack(ui, theme::LABEL_GAP, |ui| {
            widgets::field_label(ui, "Audio output");
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 8.0;
                let width = ui.available_width() - theme::CONTROL_HEIGHT - 8.0;
                let selected = self
                    .audio_output_device
                    .clone()
                    .unwrap_or_else(|| "System default".to_owned());
                let response = widgets::select_button(ui, &selected, width);
                self.audio_output_menu(&response, RectAlign::BOTTOM_START);
                if widgets::icon_button(
                    ui,
                    Icon::Refresh,
                    "Refresh audio outputs",
                    IconButtonKind::Secondary,
                )
                .clicked()
                {
                    self.refresh_audio_devices();
                }
            });
            widgets::muted(
                ui,
                "You can switch outputs while the stream is playing.",
                13.0,
            );
        });
    }

    // ---------------------------------------------------------------------
    // Watch

    fn show_watch(&mut self, ui: &mut Ui, area: Rect, compact: bool) {
        let phase = self.watch_phase();
        match phase {
            WatchPhase::Live => return self.show_watch_live(ui, area, compact),
            WatchPhase::Listening => return self.show_watch_listening(ui, area, compact),
            WatchPhase::Idle | WatchPhase::Waiting => {}
        }
        let waiting = phase == WatchPhase::Waiting;
        if compact {
            compact_column(
                ui,
                area,
                "watch-column",
                theme::COMPACT_COLUMN_WIDTH,
                |ui| {
                    card(ui, |ui| {
                        widgets::stack(ui, theme::SECTION_GAP, |ui| {
                            self.watch_body(ui, waiting);
                            if waiting {
                                self.waiting_summary(ui);
                            }
                            self.watch_footer(ui, waiting);
                        });
                    });
                },
            );
            return;
        }

        let (panel, stage) = split_left(area, theme::PANEL_WIDTH);
        let footer_height = if waiting {
            theme::CONTROL_HEIGHT
        } else {
            theme::CONTROL_HEIGHT + 12.0 + 22.0
        };
        let (body, footer) = card_rects(ui, panel, footer_height);
        scroll_child(ui, body, "watch-form", |ui| self.watch_body(ui, waiting));
        child(ui, footer, |ui| self.watch_footer(ui, waiting));

        paint_stage(ui, stage);
        if waiting {
            self.waiting_stage(ui, stage);
        } else {
            watch_empty_stage(ui, stage, self.watch_audio_only);
        }
    }

    fn watch_body(&mut self, ui: &mut Ui, waiting: bool) {
        widgets::stack(ui, theme::SECTION_GAP, |ui| {
            panel_header(
                ui,
                "Watch a screen",
                "Connect to a host on your network. Streaming starts once they accept.",
            );
            widgets::stack(ui, theme::FIELD_GAP, |ui| {
                ui.add_enabled_ui(!waiting, |ui| {
                    widgets::stack(ui, theme::FIELD_GAP, |ui| {
                        self.watch_session_field(ui);
                        self.address_fields(ui, "Host IP", "192.168.1.10");
                        self.audio_output_field(ui);
                    });
                });
                // Volume stays adjustable while the request is pending.
                self.watch_volume_control(ui);
            });
            self.show_notice(ui);
        });
    }

    fn watch_session_field(&mut self, ui: &mut Ui) {
        widgets::stack(ui, theme::LABEL_GAP, |ui| {
            widgets::field_label(ui, "Session");
            widgets::segmented(
                ui,
                "watch-session",
                &mut self.watch_audio_only,
                &[
                    Segment::new(false, "Video + audio").icon(Icon::Monitor),
                    Segment::new(true, "Audio only").icon(Icon::Volume),
                ],
                SegmentTrack::Field,
            );
            if self.watch_audio_only {
                widgets::muted(
                    ui,
                    "The host sends system audio only. No video is sent or decoded.",
                    13.0,
                );
            }
        });
    }

    fn watch_volume_control(&mut self, ui: &mut Ui) {
        if widgets::volume_control(ui, "Volume", &mut self.watch_volume, &mut self.watch_muted) {
            self.apply_volume();
        }
    }

    fn share_volume_control(&mut self, ui: &mut Ui, label: &str) {
        if widgets::volume_control(ui, label, &mut self.share_volume, &mut self.share_muted) {
            self.apply_volume();
        }
    }

    fn show_watch_listening(&mut self, ui: &mut Ui, area: Rect, compact: bool) {
        let host = self
            .target
            .map(|target| target.ip().to_string())
            .unwrap_or_else(|| "the host".to_owned());
        if compact {
            compact_column(
                ui,
                area,
                "listening-column",
                theme::COMPACT_COLUMN_WIDTH,
                |ui| {
                    widgets::stack(ui, theme::GUTTER, |ui| {
                        card(ui, |ui| self.listening_controls(ui, &host, false));
                        card(ui, |ui| self.listener_health(ui));
                    });
                },
            );
            return;
        }

        let (stage, dock) = split_right(area, theme::HEALTH_WIDTH);
        paint_stage(ui, stage);
        centered_block(ui, stage.shrink(32.0), "watch-listening", 480.0, |ui| {
            self.listening_controls(ui, &host, true);
        });
        paint_surface(ui, dock, theme::SURFACE);
        scroll_child(
            ui,
            dock.shrink(theme::CARD_PADDING),
            "listener-health",
            |ui| self.listener_health(ui),
        );
    }

    /// The audio-only session's hero, volume, output and disconnect controls.
    /// On the stage they sit in their own card; in a compact column the
    /// surrounding card already frames them.
    fn listening_controls(&mut self, ui: &mut Ui, host: &str, on_stage: bool) {
        widgets::stack(ui, theme::SECTION_GAP, |ui| {
            ui.vertical_centered(|ui| {
                widgets::stack(ui, 16.0, |ui| {
                    widgets::pulse(ui, Icon::Volume, Tone::Signal);
                    stage_message(
                        ui,
                        &format!("Listening to {host}"),
                        "Audio only. The host's system audio plays through your output.",
                    );
                });
            });
            let controls = |app: &mut Self, ui: &mut Ui| {
                widgets::stack(ui, theme::FIELD_GAP, |ui| {
                    app.watch_volume_control(ui);
                    app.audio_output_field(ui);
                });
            };
            if on_stage {
                card(ui, |ui| {
                    ui.with_layout(Layout::top_down(Align::Min), |ui| controls(self, ui));
                });
            } else {
                controls(self, ui);
            }
            let disconnect = if on_stage {
                ui.vertical_centered(|ui| {
                    widgets::button(
                        ui,
                        ButtonKind::Danger,
                        Some(Icon::Close),
                        "Disconnect",
                        None,
                    )
                })
                .inner
            } else {
                let width = ui.available_width();
                widgets::button(
                    ui,
                    ButtonKind::Danger,
                    Some(Icon::Close),
                    "Disconnect",
                    Some(width),
                )
            };
            if disconnect.clicked() {
                self.stop();
            }
        });
    }

    fn listener_health(&self, ui: &mut Ui) {
        widgets::stack(ui, 20.0, |ui| {
            ui.horizontal(|ui| {
                widgets::heading(ui, "Stream health");
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    widgets::tag(ui, "Live", Tone::Signal);
                });
            });
            widgets::tile_grid(
                ui,
                2,
                &[
                    (
                        "Round trip",
                        format!("{:.1}", self.rtt_ms),
                        "ms",
                        "Live ping".to_owned(),
                    ),
                    (
                        "Bitrate",
                        format!("{:.2}", self.stream_mbps),
                        "Mbps",
                        "Audio only".to_owned(),
                    ),
                ],
            );
            section(ui, "Session", |ui| {
                widgets::stack(ui, 12.0, |ui| {
                    let output = self
                        .audio_output_device
                        .clone()
                        .unwrap_or_else(|| "System default".to_owned());
                    widgets::key_value(ui, "Mode", "Audio only", false, 13.0);
                    widgets::key_value(ui, "Output", &output, false, 13.0);
                    widgets::key_value(ui, "Transport", &self.transport, false, 13.0);
                    widgets::muted(ui, self.audio_backend.clone(), 12.0);
                });
            });
            let note = if self.watch_audio_only {
                "To see the host's screen, disconnect and choose Video + audio."
            } else {
                "The host is sharing audio only."
            };
            widgets::muted(ui, note, 13.0);
        });
    }

    fn watch_footer(&mut self, ui: &mut Ui, waiting: bool) {
        let width = ui.available_width();
        if waiting {
            if widgets::button(
                ui,
                ButtonKind::Secondary,
                None,
                "Cancel request",
                Some(width),
            )
            .clicked()
            {
                self.stop();
            }
            return;
        }
        widgets::stack(ui, 12.0, |ui| {
            let stopping = self.shutdown.is_some();
            let label = if stopping { "Stopping…" } else { "Connect" };
            let clicked = ui
                .add_enabled_ui(!stopping, |ui| {
                    widgets::button(ui, ButtonKind::Primary, None, label, Some(width))
                })
                .inner
                .clicked();
            if clicked {
                let context = ui.ctx().clone();
                self.connect(&context);
            }
            widgets::key_hint(ui, &["F11"], "", "toggles fullscreen while watching");
        });
    }

    fn waiting_copy(&self) -> (String, &'static str, [StepState; 3]) {
        let host = self
            .target
            .map(|target| target.ip().to_string())
            .unwrap_or_else(|| "the host".to_owned());
        if self.reconnecting {
            (
                format!("Reconnecting to {host}"),
                "The host stopped responding. Echobind keeps trying while this stays open.",
                [StepState::Done, StepState::Done, StepState::Current],
            )
        } else if self.peer.is_some() {
            (
                format!("{host} accepted"),
                "Waiting for the first video frame.",
                [StepState::Done, StepState::Done, StepState::Current],
            )
        } else {
            (
                format!("Waiting for {host} to accept"),
                "Your request is showing on the host's screen. The stream starts the moment they accept.",
                [StepState::Done, StepState::Current, StepState::Pending],
            )
        }
    }

    fn waiting_stage(&self, ui: &mut Ui, stage: Rect) {
        let (title, body, states) = self.waiting_copy();
        centered_block(ui, stage.shrink(32.0), "watch-waiting", 560.0, |ui| {
            widgets::stack(ui, 32.0, |ui| {
                widgets::pulse(ui, Icon::Monitor, Tone::Caution);
                stage_message(ui, &title, body);
                widgets::progress_steps(
                    ui,
                    &[
                        ("Request sent", states[0]),
                        ("Host approval", states[1]),
                        ("Stream", states[2]),
                    ],
                );
            });
        });
    }

    fn waiting_summary(&self, ui: &mut Ui) {
        let (title, body, states) = self.waiting_copy();
        widgets::stack(ui, 16.0, |ui| {
            ui.vertical_centered(|ui| {
                widgets::pulse(ui, Icon::Monitor, Tone::Caution);
                stage_message(ui, &title, body);
            });
            widgets::progress_steps(
                ui,
                &[
                    ("Sent", states[0]),
                    ("Approval", states[1]),
                    ("Stream", states[2]),
                ],
            );
        });
    }

    fn show_watch_live(&mut self, ui: &mut Ui, area: Rect, compact: bool) {
        let docked = !compact && self.health_open;
        let (stage, dock) = if docked {
            let (stage, dock) = split_right(area, theme::HEALTH_WIDTH);
            (stage, Some(dock))
        } else {
            (area, None)
        };

        paint_stage(ui, stage);
        self.paint_video(ui, stage.shrink(1.0));
        mask_stage_corners(ui, stage);

        let mut toolbar_bounds = stage;
        if compact && self.drawer_open {
            let drawer = Rect::from_min_max(
                pos2(
                    stage.right() - 12.0 - theme::DRAWER_WIDTH,
                    stage.top() + 12.0,
                ),
                pos2(stage.right() - 12.0, stage.bottom() - 12.0),
            );
            toolbar_bounds.max.x = drawer.left();
            ui.painter().add(
                theme::overlay_shadow().as_shape(drawer, CornerRadius::same(theme::RADIUS_GROUP)),
            );
            ui.painter().rect(
                drawer,
                CornerRadius::same(theme::RADIUS_GROUP),
                theme::SURFACE,
                Stroke::new(1.0, theme::CONTROL_BORDER),
                StrokeKind::Inside,
            );
            // Swallow clicks on the drawer so they never reach the video.
            ui.interact(drawer, Id::new("health-drawer"), Sense::click());
            let mut close = false;
            scroll_child(ui, drawer.shrink(16.0), "health-drawer", |ui| {
                close = self.viewer_health(ui, true);
            });
            if close {
                self.drawer_open = false;
            }
        }
        self.stream_toolbar(ui, toolbar_bounds, compact);

        if let Some(dock) = dock {
            paint_surface(ui, dock, theme::SURFACE);
            scroll_child(ui, dock.shrink(theme::CARD_PADDING), "health-dock", |ui| {
                self.viewer_health(ui, false);
            });
        }
    }

    fn stream_toolbar(&mut self, ui: &mut Ui, bounds: Rect, compact: bool) {
        let icons_only = compact && self.drawer_open;
        let fps = if self.present_fps > 0.0 {
            self.present_fps
        } else {
            self.stream_fps
        };
        let readout = [
            (format!("{fps:.1}"), "fps"),
            (format!("{:.1}", self.stream_mbps), "Mbps"),
            (format!("{:.1}", self.rtt_ms), "ms RTT"),
        ];
        let readout_galleys: Vec<_> = if icons_only {
            Vec::new()
        } else {
            readout
                .iter()
                .map(|(value, unit)| {
                    (
                        ui.painter().layout_no_wrap(
                            value.clone(),
                            theme::mono(13.0, Weight::Medium),
                            theme::TEXT,
                        ),
                        ui.painter().layout_no_wrap(
                            (*unit).to_owned(),
                            theme::inter(12.0, Weight::Regular),
                            theme::MUTED,
                        ),
                    )
                })
                .collect()
        };
        let readout_width = if readout_galleys.is_empty() {
            0.0
        } else {
            readout_galleys
                .iter()
                .map(|(value, unit)| value.size().x + 4.0 + unit.size().x)
                .sum::<f32>()
                + 16.0 * (readout_galleys.len() - 1) as f32
                + 24.0
        };
        let disconnect_labelled = !compact;
        let disconnect_width = if disconnect_labelled { 128.0 } else { 40.0 };
        // Mute, then (unless collapsed) an 88 px slider and the output menu.
        let volume_slider_width = if icons_only { 0.0 } else { 88.0 };
        let volume_width = if icons_only {
            40.0
        } else {
            40.0 + 4.0 + volume_slider_width + 4.0 + 28.0
        };
        let buttons_width = volume_width + 40.0 * 2.0 + disconnect_width + 8.0 * 3.0;
        let width =
            8.0 + if readout_width > 0.0 {
                readout_width + 8.0 + 1.0 + 8.0
            } else {
                0.0
            } + buttons_width
                + 8.0;
        let rect = Rect::from_center_size(
            pos2(bounds.center().x, bounds.bottom() - 16.0 - 28.0),
            vec2(width.min(bounds.width() - 32.0), 56.0),
        );
        ui.painter()
            .add(theme::overlay_shadow().as_shape(rect, CornerRadius::same(theme::RADIUS_SURFACE)));
        ui.painter().rect(
            rect,
            CornerRadius::same(theme::RADIUS_SURFACE),
            TOOLBAR_FILL,
            Stroke::new(1.0, theme::CONTROL_BORDER),
            StrokeKind::Inside,
        );
        ui.interact(rect, Id::new("stream-toolbar"), Sense::click());

        let inner = rect.shrink(8.0);
        ui.scope_builder(
            UiBuilder::new()
                .max_rect(inner)
                .layout(Layout::left_to_right(Align::Center)),
            |ui| {
                ui.spacing_mut().item_spacing.x = 8.0;
                if readout_width > 0.0 {
                    let (readout_rect, _) = ui.allocate_exact_size(
                        vec2(readout_width, theme::CONTROL_HEIGHT),
                        Sense::hover(),
                    );
                    let mut left = readout_rect.left() + 12.0;
                    for (value, unit) in readout_galleys {
                        let value_width = value.size().x;
                        let unit_width = unit.size().x;
                        let value_top = readout_rect.center().y - value.size().y / 2.0;
                        let unit_top = readout_rect.center().y - unit.size().y / 2.0 + 1.0;
                        ui.painter()
                            .galley(pos2(left, value_top), value, theme::TEXT);
                        ui.painter().galley(
                            pos2(left + value_width + 4.0, unit_top),
                            unit,
                            theme::MUTED,
                        );
                        left += value_width + 4.0 + unit_width + 16.0;
                    }
                    let (divider, _) = ui.allocate_exact_size(vec2(1.0, 24.0), Sense::hover());
                    ui.painter().rect_filled(divider, 0, theme::CONTROL_BORDER);
                }

                let mut volume = self.watch_volume;
                let mut muted = self.watch_muted;
                let volume_changed = ui
                    .scope(|ui| {
                        ui.spacing_mut().item_spacing.x = 4.0;
                        let changed = widgets::volume_inline(
                            ui,
                            &mut volume,
                            &mut muted,
                            volume_slider_width,
                        );
                        if !icons_only {
                            let output =
                                widgets::narrow_icon_button(ui, Icon::ChevronDown, "Audio output");
                            self.audio_output_menu(&output, RectAlign::TOP_START);
                        }
                        changed
                    })
                    .inner;
                self.watch_volume = volume;
                self.watch_muted = muted;
                if volume_changed {
                    self.apply_volume();
                }

                let health_open = if compact {
                    self.drawer_open
                } else {
                    self.health_open
                };
                let kind = if health_open {
                    IconButtonKind::Pressed
                } else {
                    IconButtonKind::Ghost
                };
                if widgets::icon_button(ui, Icon::Activity, "Stream health", kind).clicked() {
                    if compact {
                        self.drawer_open = !self.drawer_open;
                    } else {
                        self.health_open = !self.health_open;
                    }
                }
                if widgets::icon_button(
                    ui,
                    Icon::Maximize,
                    "Fullscreen (F11)",
                    IconButtonKind::Ghost,
                )
                .clicked()
                {
                    let context = ui.ctx().clone();
                    self.set_fullscreen(&context, true);
                }
                let disconnect = if disconnect_labelled {
                    widgets::button(
                        ui,
                        ButtonKind::Danger,
                        Some(Icon::Close),
                        "Disconnect",
                        Some(disconnect_width),
                    )
                } else {
                    widgets::icon_button(ui, Icon::Close, "Disconnect", IconButtonKind::Danger)
                };
                if disconnect.clicked() {
                    self.stop();
                }
            },
        );
    }

    fn server_stats_age(&self) -> Option<f32> {
        stale_server_stats_age(self.server_stats_received_at, Instant::now())
            .map(|age| age.as_secs_f32())
    }

    /// The viewer's stream-health panel; returns true when its close button
    /// was pressed.
    fn viewer_health(&mut self, ui: &mut Ui, closable: bool) -> bool {
        let mut close = false;
        let stale_age = self.server_stats_age();
        widgets::stack(ui, 20.0, |ui| {
            let header_height = if closable {
                theme::CONTROL_HEIGHT
            } else {
                theme::TAG_HEIGHT
            };
            ui.allocate_ui_with_layout(
                vec2(ui.available_width(), header_height),
                Layout::left_to_right(Align::Center),
                |ui| {
                    widgets::heading(ui, "Stream health");
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if closable
                            && widgets::icon_button(
                                ui,
                                Icon::Close,
                                "Close stream health",
                                IconButtonKind::Ghost,
                            )
                            .clicked()
                        {
                            close = true;
                        }
                        if stale_age.is_some() {
                            widgets::tag(ui, "Stale", Tone::Caution);
                        } else {
                            widgets::tag(ui, "Live", Tone::Signal);
                        }
                    });
                },
            );

            if let Some(age) = stale_age {
                widgets::banner(
                    ui,
                    Tone::Caution,
                    &format!("Host telemetry stale · {age:.1} s"),
                    "Host values aren't live. Its video pipeline may have stopped.",
                );
            }

            let (presented_label, presented, presented_context) = if self.present_fps > 0.0 {
                (
                    "Presented",
                    self.present_fps,
                    format!("of {:.1} received", self.received_fps),
                )
            } else {
                (
                    "Decoded",
                    self.stream_fps,
                    format!("{:.1} received", self.received_fps),
                )
            };
            widgets::tile_grid(
                ui,
                2,
                &[
                    (
                        presented_label,
                        format!("{presented:.1}"),
                        "fps",
                        presented_context,
                    ),
                    (
                        "Bitrate",
                        format!("{:.1}", self.stream_mbps),
                        "Mbps",
                        self.transport.clone(),
                    ),
                    (
                        "Round trip",
                        format!("{:.1}", self.rtt_ms),
                        "ms",
                        format!("Jitter {:.1} ms", self.jitter_ms),
                    ),
                    (
                        "Frames lost",
                        self.lost_frames.to_string(),
                        "lost",
                        format!("{} dropped", self.dropped_frames),
                    ),
                ],
            );

            section(ui, "Latency by stage", |ui| {
                let host_stages = self.server_stats.map(|server| {
                    [
                        ("Capture", server.capture_ms),
                        ("Encode", server.encode_ms),
                        ("Encode → send", server.encode_queue_ms),
                        ("Send", server.send_ms),
                    ]
                });
                let viewer_stages = [
                    ("Reassemble", self.reassembly_ms),
                    ("Decode queue", self.decode_queue_ms),
                    ("Decode", self.decode_ms),
                    ("Present", self.present_latency_ms),
                ];
                let scale = host_stages
                    .iter()
                    .flatten()
                    .chain(viewer_stages.iter())
                    .map(|(_, value)| *value)
                    .fold(4.0_f32, f32::max);

                widgets::stack(ui, 8.0, |ui| match host_stages {
                    Some(stages) => {
                        let total: f32 = stages.iter().map(|(_, value)| value).sum();
                        widgets::key_value(ui, "Host", &format!("{total:.2} ms"), true, 12.0);
                        ui.scope(|ui| {
                            if stale_age.is_some() {
                                ui.multiply_opacity(0.6);
                            }
                            for (label, value) in stages {
                                widgets::meter_row(ui, label, value, scale, theme::DIVIDER);
                            }
                        });
                    }
                    None => widgets::muted(ui, "Waiting for host stats…", 13.0),
                });
                widgets::stack(ui, 8.0, |ui| {
                    let total: f32 = viewer_stages.iter().map(|(_, value)| value).sum();
                    widgets::key_value(
                        ui,
                        &format!("This device · {}", self.video_backend),
                        &format!("{total:.2} ms"),
                        true,
                        12.0,
                    );
                    for (label, value) in viewer_stages {
                        widgets::meter_row(ui, label, value, scale, theme::DIVIDER);
                    }
                });
            });

            widgets::stack(ui, 8.0, |ui| {
                widgets::disclosure(ui, "viewer-session", "Session details", |ui| {
                    widgets::key_value(
                        ui,
                        "Resolution",
                        &format!("{}×{} · H.264", self.stream_width, self.stream_height),
                        false,
                        13.0,
                    );
                    widgets::key_value(ui, "Decoder", &self.video_backend, false, 13.0);
                    widgets::key_value(ui, "Audio", &self.audio_backend, false, 13.0);
                    widgets::key_value(ui, "Transport", &self.transport, false, 13.0);
                    if let Some(server) = self.server_stats {
                        widgets::key_value(
                            ui,
                            "Host GPU",
                            &format!(
                                "{} priority · {}",
                                gpu_priority_label(server.gpu_priority_class),
                                nvenc_slot_label(server.nvenc_buffer_count)
                            ),
                            false,
                            13.0,
                        );
                    }
                });
                widgets::disclosure(ui, "viewer-host-stages", "Host stages & counters", |ui| {
                    let Some(server) = self.server_stats else {
                        widgets::muted(ui, "Waiting for host stats…", 13.0);
                        return;
                    };
                    for (key, value) in [
                        (
                            "Source / sent",
                            format!("{:.1} / {:.1} fps", server.source_fps, server.fps),
                        ),
                        (
                            "GPU wait · lock",
                            format!("{:.2} · {:.2} ms", server.gpu_wait_ms, server.gpu_lock_ms),
                        ),
                        (
                            "Copy · convert issue",
                            format!("{:.2} · {:.2}", server.copy_wait_ms, server.convert_wait_ms),
                        ),
                        ("Sync / map", format!("{:.2}", server.map_ms)),
                        (
                            "Submit · API · busy",
                            format!(
                                "{:.2} · {:.2} · {:.2}",
                                server.submit_ms, server.submit_api_ms, server.submit_busy_wait_ms
                            ),
                        ),
                        ("Busy retries", server.submit_busy_retries.to_string()),
                        (
                            "Completion wait",
                            format!("{:.2}", server.completion_wait_ms),
                        ),
                        (
                            "Bitstream · lock · copy",
                            format!(
                                "{:.2} · {:.2} · {:.2}",
                                server.bitstream_ms,
                                server.bitstream_lock_ms,
                                server.bitstream_copy_ms
                            ),
                        ),
                        (
                            "Unlock · unmap",
                            format!("{:.2} · {:.2}", server.bitstream_unlock_ms, server.unmap_ms),
                        ),
                        (
                            "NVENC submit / complete",
                            format!(
                                "{} / {}",
                                server.nvenc_submissions, server.nvenc_completions
                            ),
                        ),
                        (
                            "DXGI timeouts · backlog",
                            format!(
                                "{} · {} (max {})",
                                server.dxgi_timeouts, server.dxgi_backlog, server.dxgi_backlog_max
                            ),
                        ),
                        (
                            "Skips pace / pre / slot",
                            format!(
                                "{} / {} / {}",
                                server.pacing_skips,
                                server.preprocess_busy_skips,
                                server.no_free_slot_skips
                            ),
                        ),
                        (
                            "NVENC busy",
                            nvenc_busy_skips(
                                server.slot_busy_skips,
                                server.preprocess_busy_skips,
                                server.no_free_slot_skips,
                            )
                            .to_string(),
                        ),
                        (
                            "Cursor-only / stale",
                            format!("{} / {}", server.cursor_only_frames, server.stale_frames),
                        ),
                        ("Send failures", server.video_send_failures.to_string()),
                    ] {
                        widgets::key_value(ui, key, &value, true, 13.0);
                    }
                });
            });
        });
        close
    }

    pub(super) fn show_fullscreen_stats(&self, context: &egui::Context) {
        if self.mode != Mode::Connect {
            return;
        }

        let stale_age = self.server_stats_age();
        egui::Area::new("fullscreen-stats".into())
            .anchor(Align2::RIGHT_TOP, vec2(-16.0, 16.0))
            .interactable(false)
            .show(context, |ui| {
                egui::Frame::new()
                    .fill(HUD_FILL)
                    .stroke(Stroke::new(1.0, theme::CONTROL_BORDER))
                    .corner_radius(theme::RADIUS_SURFACE)
                    .inner_margin(16)
                    .show(ui, |ui| {
                        ui.set_width(300.0);
                        widgets::stack(ui, 12.0, |ui| {
                            ui.horizontal(|ui| {
                                let (logo, _) =
                                    ui.allocate_exact_size(Vec2::splat(16.0), Sense::hover());
                                icons::paint(ui.painter(), logo, Icon::Logo, theme::SIGNAL);
                                ui.label(
                                    widgets::text("ECHOBIND", 11.0, Weight::SemiBold, theme::TEXT)
                                        .extra_letter_spacing(0.88),
                                );
                                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                    if stale_age.is_some() {
                                        widgets::tag(ui, "Stale", Tone::Caution);
                                    } else {
                                        widgets::tag(ui, "Live", Tone::Signal);
                                    }
                                });
                            });

                            if let Some(age) = stale_age {
                                widgets::banner(
                                    ui,
                                    Tone::Caution,
                                    &format!("Host telemetry stale · {age:.1} s"),
                                    "Host values aren't live. Its video pipeline may have stopped.",
                                );
                            }

                            ui.scope(|ui| {
                                if stale_age.is_some() {
                                    ui.multiply_opacity(0.6);
                                }
                                self.hud_host_rows(ui, stale_age.is_some());
                            });
                            widgets::divider(ui);
                            widgets::stack(ui, 4.0, |ui| {
                                widgets::overline(ui, "This device");
                                for (key, value) in [
                                    (
                                        "Recv / decode / present",
                                        format!(
                                            "{:.1} / {:.1} / {:.1}",
                                            self.received_fps, self.stream_fps, self.present_fps
                                        ),
                                    ),
                                    ("Bitrate", format!("{:.1} Mbps", self.stream_mbps)),
                                    ("Decode", format!("{:.2} ms", self.decode_ms)),
                                    (
                                        "RTT · jitter",
                                        format!("{:.2} · {:.2} ms", self.rtt_ms, self.jitter_ms),
                                    ),
                                    (
                                        "Drop / loss",
                                        format!("{} / {}", self.dropped_frames, self.lost_frames),
                                    ),
                                ] {
                                    widgets::key_value(ui, key, &value, true, 12.0);
                                }
                            });
                            widgets::key_hint(ui, &["F11", "Esc"], "or", "to exit fullscreen");
                        });
                    });
            });
    }

    fn hud_host_rows(&self, ui: &mut Ui, stale: bool) {
        widgets::stack(ui, 4.0, |ui| {
            widgets::overline(
                ui,
                if stale {
                    "Host · last snapshot"
                } else {
                    "Host"
                },
            );
            let Some(server) = self.server_stats else {
                widgets::muted(ui, "Waiting for host stats…", 12.0);
                return;
            };
            let mut rows = vec![
                (
                    "Source / sent",
                    format!("{:.1} / {:.1} fps", server.source_fps, server.fps),
                ),
                ("Bitrate", format!("{:.1} Mbps", server.megabits_per_second)),
                (
                    "Capture · encode",
                    format!("{:.2} · {:.2} ms", server.capture_ms, server.encode_ms),
                ),
                (
                    "GPU wait · lock",
                    format!("{:.2} · {:.2} ms", server.gpu_wait_ms, server.gpu_lock_ms),
                ),
                (
                    "Queue · send",
                    format!("{:.2} · {:.2} ms", server.encode_queue_ms, server.send_ms),
                ),
                (
                    "GPU · NVENC",
                    format!(
                        "{} · {}",
                        gpu_priority_label(server.gpu_priority_class),
                        nvenc_slot_label(server.nvenc_buffer_count)
                    ),
                ),
                (
                    "NVENC submit / done",
                    format!(
                        "{} / {}",
                        server.nvenc_submissions, server.nvenc_completions
                    ),
                ),
                (
                    "Copy · convert · map",
                    format!(
                        "{:.2} · {:.2} · {:.2}",
                        server.copy_wait_ms, server.convert_wait_ms, server.map_ms
                    ),
                ),
                (
                    "Submit · API · busy",
                    format!(
                        "{:.2} · {:.2} · {:.2} ({}×)",
                        server.submit_ms,
                        server.submit_api_ms,
                        server.submit_busy_wait_ms,
                        server.submit_busy_retries
                    ),
                ),
                (
                    "Complete · bits",
                    format!(
                        "{:.2} · {:.2} ms",
                        server.completion_wait_ms, server.bitstream_ms
                    ),
                ),
                (
                    "Bits lock · copy",
                    format!(
                        "{:.2} · {:.2} ms",
                        server.bitstream_lock_ms, server.bitstream_copy_ms
                    ),
                ),
                (
                    "Unlock · unmap",
                    format!(
                        "{:.2} · {:.2} ms",
                        server.bitstream_unlock_ms, server.unmap_ms
                    ),
                ),
            ];
            let busy = nvenc_busy_skips(
                server.slot_busy_skips,
                server.preprocess_busy_skips,
                server.no_free_slot_skips,
            );
            if [
                server.dxgi_timeouts,
                server.dxgi_backlog,
                server.pacing_skips,
                server.slot_busy_skips,
                server.preprocess_busy_skips,
                server.no_free_slot_skips,
                server.cursor_only_frames,
                server.stale_frames,
                server.video_send_failures,
            ]
            .into_iter()
            .any(|value| value > 0)
            {
                rows.push((
                    "DXGI timeout · backlog",
                    format!(
                        "{} · {} (max {})",
                        server.dxgi_timeouts, server.dxgi_backlog, server.dxgi_backlog_max
                    ),
                ));
                rows.push((
                    "Skips pace/pre/slot/busy",
                    format!(
                        "{}/{}/{}/{}",
                        server.pacing_skips,
                        server.preprocess_busy_skips,
                        server.no_free_slot_skips,
                        busy
                    ),
                ));
                rows.push((
                    "Cursor-only / stale",
                    format!("{} / {}", server.cursor_only_frames, server.stale_frames),
                ));
                rows.push(("Send failures", server.video_send_failures.to_string()));
            }
            for (key, value) in rows {
                widgets::key_value(ui, key, &value, true, 12.0);
            }
        });
    }

    // ---------------------------------------------------------------------
    // Share

    fn show_share(&mut self, ui: &mut Ui, area: Rect, compact: bool) {
        let phase = self.share_phase();
        if compact {
            let width = if phase == SharePhase::Live {
                area.width()
            } else {
                theme::COMPACT_COLUMN_WIDTH
            };
            let audio = self.audio_only_session();
            compact_column(ui, area, "share-column", width, |ui| match phase {
                SharePhase::Live => {
                    widgets::stack(ui, theme::GUTTER, |ui| {
                        self.share_session_bar(ui, audio);
                        card(ui, |ui| {
                            widgets::stack(ui, theme::SECTION_GAP, |ui| {
                                section(ui, "Shared audio", |ui| {
                                    self.share_volume_control(ui, "Volume");
                                });
                                if audio {
                                    section(ui, "Session", |ui| {
                                        self.stream_settings_summary(ui, true);
                                    });
                                }
                            });
                        });
                        if !audio {
                            card(ui, |ui| self.host_health(ui, 4));
                        }
                    });
                }
                _ => card(ui, |ui| {
                    widgets::stack(ui, theme::SECTION_GAP, |ui| {
                        self.share_panel_body(ui, phase);
                        self.share_footer(ui, phase);
                    });
                }),
            });
            if phase == SharePhase::Listening {
                if let Some(peer) = self.pending_peer {
                    self.request_dialog(ui, area.expand(theme::GUTTER), peer, false);
                }
            }
            return;
        }

        let (panel, stage) = split_left(area, theme::PANEL_WIDTH);
        let (body, footer) = card_rects(ui, panel, theme::CONTROL_HEIGHT);
        scroll_child(ui, body, "share-panel", |ui| {
            self.share_panel_body(ui, phase)
        });
        child(ui, footer, |ui| self.share_footer(ui, phase));

        match phase {
            SharePhase::Idle => {
                paint_stage(ui, stage);
                self.share_idle_stage(ui, stage);
            }
            SharePhase::Listening => {
                paint_stage(ui, stage);
                self.listening_stage(ui, stage);
                if let Some(peer) = self.pending_peer {
                    self.request_dialog(ui, stage, peer, true);
                }
            }
            SharePhase::Live if self.audio_only_session() => {
                paint_stage(ui, stage);
                self.share_audio_stage(ui, stage);
            }
            SharePhase::Live => {
                paint_surface(ui, stage, theme::SURFACE);
                scroll_child(ui, stage.shrink(theme::CARD_PADDING), "host-health", |ui| {
                    let columns = if ui.available_width() >= 640.0 { 4 } else { 2 };
                    self.host_health(ui, columns);
                });
            }
        }
    }

    fn share_audio_stage(&self, ui: &mut Ui, stage: Rect) {
        let listener = self
            .peer
            .map(|peer| peer.ip().to_string())
            .unwrap_or_else(|| "A listener".to_owned());
        let summary = format!("Audio only · Opus · {}", self.transport);
        centered_block(ui, stage.shrink(32.0), "share-audio", 520.0, |ui| {
            widgets::stack(ui, 24.0, |ui| {
                widgets::pulse(ui, Icon::Volume, Tone::Signal);
                stage_message(
                    ui,
                    &format!("{listener} is listening"),
                    "Everything your computer plays is shared until you stop.",
                );
                widgets::summary_pill(ui, Icon::Volume, &summary);
            });
        });
    }

    fn share_panel_body(&mut self, ui: &mut Ui, phase: SharePhase) {
        widgets::stack(ui, theme::SECTION_GAP, |ui| match phase {
            SharePhase::Idle => {
                panel_header(
                    ui,
                    "Share your screen",
                    "Viewers on your network can ask to watch. You approve each one.",
                );
                section(ui, "Session", |ui| {
                    widgets::segmented(
                        ui,
                        "share-session",
                        &mut self.share_audio_only,
                        &[
                            Segment::new(false, "Screen + audio").icon(Icon::Monitor),
                            Segment::new(true, "Audio only").icon(Icon::Volume),
                        ],
                        SegmentTrack::Field,
                    );
                    if self.share_audio_only {
                        widgets::muted(
                            ui,
                            "Shares system audio without capturing your screen. Video settings don't apply.",
                            13.0,
                        );
                    }
                });
                section(ui, "Network", |ui| {
                    self.address_fields(ui, "Bind IP", "0.0.0.0");
                    widgets::muted(ui, "0.0.0.0 listens on every network interface.", 13.0);
                });
                if !self.share_audio_only {
                    section(ui, "Video", |ui| self.video_settings(ui));
                }
                section(ui, "Audio", |ui| {
                    self.share_volume_control(ui, "Shared audio volume");
                    widgets::muted(
                        ui,
                        "System audio, encoded with Opus. Volume can change while sharing.",
                        13.0,
                    );
                });
                section(ui, "Transport", |ui| self.transport_settings(ui));
                self.show_notice(ui);
            }
            SharePhase::Listening => {
                panel_header(
                    ui,
                    "Sharing is on",
                    "Waiting for a viewer. Settings are locked while sharing.",
                );
                theme::inset_frame().show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.horizontal(|ui| {
                        ui.vertical(|ui| {
                            ui.spacing_mut().item_spacing.y = 2.0;
                            ui.label(widgets::text(
                                "Listening on",
                                12.0,
                                Weight::Regular,
                                theme::MUTED,
                            ));
                            let address = self
                                .listen_address
                                .map(|address| address.to_string())
                                .unwrap_or_else(|| "starting…".to_owned());
                            ui.label(
                                egui::RichText::new(address)
                                    .font(theme::mono(15.0, Weight::Medium))
                                    .color(theme::TEXT),
                            );
                        });
                        if self.capture_ready {
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                widgets::tag(ui, "Capture ready", Tone::Signal);
                            });
                        }
                    });
                });
                section(ui, "Shared audio", |ui| {
                    self.share_volume_control(ui, "Volume");
                });
                section(ui, "Stream settings", |ui| {
                    self.stream_settings_summary(ui, false)
                });
                self.show_notice(ui);
            }
            SharePhase::Live => {
                let audio = self.audio_only_session();
                let (title, body) = if audio {
                    (
                        "Sharing audio",
                        "System audio is streaming. Your screen isn't captured.",
                    )
                } else {
                    (
                        "Sharing your screen",
                        "Primary display and system audio are streaming.",
                    )
                };
                widgets::stack(ui, 4.0, |ui| {
                    ui.horizontal(|ui| {
                        widgets::title(ui, title);
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            widgets::tag(ui, "Live", Tone::Signal);
                        });
                    });
                    widgets::muted(ui, body, 14.0);
                });
                self.viewer_card(ui, audio);
                section(ui, "Shared audio", |ui| {
                    self.share_volume_control(ui, "Volume");
                    widgets::muted(
                        ui,
                        if audio {
                            "Changes apply to the listener right away."
                        } else {
                            "Changes apply to the viewer right away."
                        },
                        13.0,
                    );
                });
                section(
                    ui,
                    if audio { "Session" } else { "Stream settings" },
                    |ui| {
                        self.stream_settings_summary(ui, true);
                        if !audio {
                            widgets::muted(ui, "Stop sharing to change settings.", 13.0);
                        }
                    },
                );
                self.show_notice(ui);
            }
        });
    }

    fn share_footer(&mut self, ui: &mut Ui, phase: SharePhase) {
        let width = ui.available_width();
        if phase == SharePhase::Idle {
            let stopping = self.shutdown.is_some();
            let label = if stopping {
                "Stopping…"
            } else {
                "Start sharing"
            };
            let clicked = ui
                .add_enabled_ui(!stopping, |ui| {
                    widgets::button(
                        ui,
                        ButtonKind::Primary,
                        Some(Icon::Share),
                        label,
                        Some(width),
                    )
                })
                .inner
                .clicked();
            if clicked {
                self.start_host();
            }
        } else if widgets::button(
            ui,
            ButtonKind::Danger,
            Some(Icon::Stop),
            "Stop sharing",
            Some(width),
        )
        .clicked()
        {
            self.stop();
        }
    }

    fn video_settings(&mut self, ui: &mut Ui) {
        widgets::stack(ui, theme::LABEL_GAP, |ui| {
            widgets::field_label(ui, "Resolution");
            widgets::segmented(
                ui,
                "resolution",
                &mut self.resolution,
                &[
                    Segment::new(VideoResolution::Native, "Native"),
                    Segment::new(VideoResolution::P1080, "1080p"),
                    Segment::new(VideoResolution::P720, "720p"),
                ],
                SegmentTrack::Field,
            );
        });
        widgets::stack(ui, theme::LABEL_GAP, |ui| {
            widgets::field_label(ui, "Frame rate");
            let labels = FRAME_RATES.map(|rate| rate.to_string());
            let segments: Vec<_> = FRAME_RATES
                .iter()
                .zip(labels.iter())
                .map(|(rate, label)| Segment::new(*rate, label.as_str()))
                .collect();
            widgets::segmented(
                ui,
                "frame-rate",
                &mut self.frames_per_second,
                &segments,
                SegmentTrack::Field,
            );
        });
        widgets::stack(ui, theme::LABEL_GAP, |ui| {
            ui.horizontal(|ui| {
                widgets::field_label(ui, "Bitrate");
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.label(widgets::mono_text(
                        format!("{} Mbps", self.bitrate_mbps),
                        13.0,
                        theme::TEXT,
                    ));
                });
            });
            let width = ui.available_width();
            widgets::slider(ui, &mut self.bitrate_mbps, 1..=200, "Bitrate in Mbps", width).on_hover_text(
                "This is encoded-video payload before recovery, UDP/IP, and Ethernet overhead. Use 80 Mbps or less on a physical 100-Mbps path.",
            );
            let (rect, _) =
                ui.allocate_exact_size(vec2(ui.available_width(), 16.0), Sense::hover());
            let font = theme::inter(12.0, Weight::Regular);
            ui.painter().text(
                rect.left_center(),
                Align2::LEFT_CENTER,
                "1",
                font.clone(),
                theme::MUTED,
            );
            ui.painter().text(
                rect.center(),
                Align2::CENTER_CENTER,
                "Encoded video, before network overhead",
                font.clone(),
                theme::MUTED,
            );
            ui.painter().text(
                rect.right_center(),
                Align2::RIGHT_CENTER,
                "200",
                font,
                theme::MUTED,
            );
            if self.bitrate_mbps > 85 {
                widgets::banner(
                    ui,
                    Tone::Caution,
                    "Needs a gigabit path",
                    "At 90–120 FPS, 100 Mbps of video is about 110 Mbps on the wire. Use 80 Mbps or less on a 100-Mbps link.",
                );
            }
        });
    }

    fn transport_settings(&mut self, ui: &mut Ui) {
        theme::inset_frame()
            .inner_margin(12)
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal_top(|ui| {
                    let text_width = ui.available_width() - 40.0 - 16.0;
                    ui.vertical(|ui| {
                        ui.set_width(text_width);
                        ui.spacing_mut().item_spacing.y = 2.0;
                        ui.label(widgets::text(
                            "Jumbo MTU 9000",
                            14.0,
                            Weight::Medium,
                            theme::TEXT,
                        ));
                        widgets::muted(
                            ui,
                            "Only if every LAN hop uses a 9000-byte MTU. Older viewers fall back to standard.",
                            13.0,
                        );
                    });
                    ui.add_space(16.0 - ui.spacing().item_spacing.x);
                    widgets::switch(ui, &mut self.jumbo_datagrams, "Jumbo MTU 9000");
                });
            });
    }

    fn settings_summary(&self) -> String {
        let transport = if self.jumbo_datagrams {
            "Jumbo MTU"
        } else {
            "Standard MTU"
        };
        if self.share_audio_only {
            return "Audio only · Opus · Standard MTU".to_owned();
        }
        format!(
            "{} · {} FPS · {} Mbps · {transport}",
            self.resolution.label(),
            self.frames_per_second,
            self.bitrate_mbps
        )
    }

    fn stream_settings_summary(&self, ui: &mut Ui, live: bool) {
        if self.audio_only_session() {
            widgets::stack(ui, 12.0, |ui| {
                widgets::key_value(ui, "Mode", "Audio only", false, 14.0);
                widgets::key_value(ui, "Transport", "Standard MTU", false, 14.0);
                widgets::muted(ui, self.audio_backend.clone(), 13.0);
            });
            return;
        }
        widgets::stack(ui, 12.0, |ui| {
            widgets::key_value(ui, "Session", "Screen + audio", false, 14.0);
            let resolution = if live {
                resolution_label(self.resolution, self.stream_width, self.stream_height)
            } else {
                self.resolution.label().to_owned()
            };
            let transport = if live {
                self.transport.clone()
            } else if self.jumbo_datagrams {
                "Jumbo MTU (when supported)".to_owned()
            } else {
                "Standard MTU".to_owned()
            };
            widgets::key_value(ui, "Resolution", &resolution, false, 14.0);
            widgets::key_value(
                ui,
                "Frame rate",
                &format!("{} FPS", self.frames_per_second),
                false,
                14.0,
            );
            widgets::key_value(
                ui,
                "Bitrate",
                &format!("{} Mbps", self.bitrate_mbps),
                false,
                14.0,
            );
            widgets::key_value(ui, "Transport", &transport, false, 14.0);
            if live {
                widgets::key_value(ui, "Encoder", &self.video_backend, false, 14.0);
            }
            widgets::key_value(ui, "Audio", &self.audio_backend, false, 14.0);
        });
    }

    fn viewer_card(&self, ui: &mut Ui, listening: bool) {
        theme::inset_frame().show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 12.0;
                self.viewer_identity(ui, listening);
            });
        });
    }

    /// Avatar, address and "Watching now" or "Listening now" for the viewer.
    fn viewer_identity(&self, ui: &mut Ui, listening: bool) {
        let (avatar, _) = ui.allocate_exact_size(Vec2::splat(40.0), Sense::hover());
        ui.painter()
            .circle_filled(avatar.center(), 20.0, theme::tint(theme::SIGNAL, 0.12));
        icons::paint(
            ui.painter(),
            Rect::from_center_size(avatar.center(), Vec2::splat(18.0)),
            if listening { Icon::Volume } else { Icon::Eye },
            theme::SIGNAL,
        );
        ui.vertical(|ui| {
            ui.spacing_mut().item_spacing.y = 2.0;
            let peer = self.peer.map(|peer| peer.to_string()).unwrap_or_default();
            ui.label(
                egui::RichText::new(peer)
                    .font(theme::mono(14.0, Weight::Medium))
                    .color(theme::TEXT),
            );
            ui.label(widgets::text(
                if listening {
                    "Listening now"
                } else {
                    "Watching now"
                },
                13.0,
                Weight::Regular,
                theme::MUTED,
            ));
        });
    }

    fn share_session_bar(&mut self, ui: &mut Ui, listening: bool) {
        theme::card_frame()
            .inner_margin(Margin::symmetric(16, 12))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 12.0;
                    self.viewer_identity(ui, listening);
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if widgets::button(
                            ui,
                            ButtonKind::Danger,
                            Some(Icon::Stop),
                            "Stop sharing",
                            None,
                        )
                        .clicked()
                        {
                            self.stop();
                        }
                        let summary = if listening {
                            "Audio only · Opus".to_owned()
                        } else {
                            format!(
                                "{} · {} FPS · {} Mbps",
                                self.resolution.label(),
                                self.frames_per_second,
                                self.bitrate_mbps
                            )
                        };
                        widgets::tag(ui, &summary, Tone::Neutral);
                    });
                });
            });
    }

    fn share_idle_stage(&self, ui: &mut Ui, stage: Rect) {
        let summary = self.settings_summary();
        let (icon, summary_icon, body) = if self.share_audio_only {
            (
                Icon::Volume,
                Icon::Volume,
                "Your system audio streams once you start sharing and accept a viewer.",
            )
        } else {
            (
                Icon::Share,
                Icon::Monitor,
                "Your primary display streams once you start sharing and accept a viewer.",
            )
        };
        centered_block(ui, stage.shrink(32.0), "share-idle", 600.0, |ui| {
            widgets::stack(ui, 24.0, |ui| {
                widgets::icon_tile(ui, icon, Tone::Neutral);
                stage_message(ui, "Not sharing", body);
                widgets::summary_pill(ui, summary_icon, &summary);
                widgets::steps(
                    ui,
                    &[
                        ("Start sharing", "Echobind listens for viewers on UDP."),
                        ("Accept a viewer", "Nothing is captured until you do."),
                        ("Stop anytime", "The viewer is disconnected right away."),
                    ],
                );
            });
        });
    }

    fn listening_stage(&self, ui: &mut Ui, stage: Rect) {
        let port = self
            .listen_address
            .map(|address| address.port())
            .unwrap_or(self.port);
        centered_block(ui, stage.shrink(32.0), "share-listening", 520.0, |ui| {
            widgets::stack(ui, 24.0, |ui| {
                widgets::pulse(ui, Icon::Logo, Tone::Signal);
                stage_message(
                    ui,
                    "Listening for viewers",
                    &format!("Viewers connect with this computer's IP and port {port}."),
                );
            });
        });
    }

    fn request_dialog(
        &mut self,
        ui: &mut Ui,
        bounds: Rect,
        peer: std::net::SocketAddr,
        rounded: bool,
    ) {
        let radius = if rounded { theme::RADIUS_SURFACE } else { 0 };
        ui.painter()
            .rect_filled(bounds, CornerRadius::same(radius), DIM_FILL);
        ui.interact(bounds, Id::new("request-dim"), Sense::click());

        let width = 420.0_f32.min(bounds.width() - 32.0);
        let mut accept = false;
        let mut decline = false;
        let (icon, title, body) = if self.pending_video && !self.share_audio_only {
            (
                Icon::Eye,
                "A viewer wants to watch your screen",
                "They'll see your primary display and hear system audio until you stop sharing.",
            )
        } else {
            (
                Icon::Volume,
                "A listener wants to hear your audio",
                "They'll hear your system audio until you stop sharing. Your screen isn't captured.",
            )
        };
        centered_block(ui, bounds.shrink(16.0), "request-dialog", width, |ui| {
            egui::Frame::new()
                .fill(theme::SURFACE)
                .stroke(Stroke::new(1.0, theme::CONTROL_BORDER))
                .corner_radius(theme::RADIUS_SURFACE)
                .inner_margin(theme::DIALOG_PADDING as i8)
                .shadow(theme::overlay_shadow())
                .show(ui, |ui| {
                    ui.set_width(width - theme::DIALOG_PADDING * 2.0);
                    ui.with_layout(Layout::top_down(Align::Min), |ui| {
                        widgets::stack(ui, 20.0, |ui| {
                            widgets::icon_tile(ui, icon, Tone::Signal);
                            widgets::stack(ui, 8.0, |ui| {
                                ui.label(widgets::text(title, 18.0, Weight::SemiBold, theme::TEXT));
                                widgets::muted(ui, body, 14.0);
                            });
                            theme::inset_frame().show(ui, |ui| {
                                ui.set_width(ui.available_width());
                                widgets::key_value(
                                    ui,
                                    "Request from",
                                    &peer.to_string(),
                                    true,
                                    14.0,
                                );
                            });
                            ui.horizontal(|ui| {
                                ui.spacing_mut().item_spacing.x = 12.0;
                                let half = (ui.available_width() - 12.0) / 2.0;
                                decline = widgets::button(
                                    ui,
                                    ButtonKind::Secondary,
                                    None,
                                    "Decline",
                                    Some(half),
                                )
                                .clicked();
                                accept = widgets::button(
                                    ui,
                                    ButtonKind::Primary,
                                    Some(Icon::Check),
                                    "Accept",
                                    Some(half),
                                )
                                .clicked();
                            });
                        });
                    });
                });
        });
        if accept {
            self.accept_pending();
        } else if decline {
            self.reject_pending();
        }
    }

    fn host_health(&self, ui: &mut Ui, columns: usize) {
        widgets::stack(ui, 20.0, |ui| {
            widgets::stack(ui, 12.0, |ui| {
                widgets::heading(ui, "Stream health");
                ui.horizontal_wrapped(|ui| {
                    widgets::tag(ui, &self.video_backend, Tone::Neutral);
                    widgets::tag(ui, &self.transport, Tone::Neutral);
                    widgets::tag(ui, &self.audio_backend, Tone::Neutral);
                });
            });

            let windows = cfg!(target_os = "windows");
            let sent_context = if self.source_fps > 0.0 {
                let source = if windows {
                    "DXGI updates"
                } else {
                    "source updates"
                };
                format!("{:.1} {source}/s", self.source_fps)
            } else {
                "Waiting for frames".to_owned()
            };
            let failures_context = if self.video_send_failures == 0 {
                "No failed datagrams".to_owned()
            } else {
                "Since sharing started".to_owned()
            };
            widgets::tile_grid(
                ui,
                columns,
                &[
                    (
                        "Sent",
                        format!("{:.1}", self.stream_fps),
                        "fps",
                        sent_context,
                    ),
                    (
                        "Bitrate",
                        format!("{:.1}", self.stream_mbps),
                        "Mbps",
                        format!("Target {} Mbps", self.bitrate_mbps),
                    ),
                    (
                        "Encode",
                        format!("{:.2}", self.encode_ms),
                        "ms",
                        format!("Capture {:.2} ms", self.capture_ms),
                    ),
                    (
                        "Send failures",
                        self.video_send_failures.to_string(),
                        "",
                        failures_context,
                    ),
                ],
            );

            let side_by_side = windows && ui.available_width() >= 600.0;
            if side_by_side {
                ui.columns(2, |columns| {
                    self.pipeline_section(&mut columns[0]);
                    self.encoder_stages_section(&mut columns[1]);
                });
            } else {
                self.pipeline_section(ui);
                if windows {
                    self.encoder_stages_section(ui);
                }
            }
            if windows {
                self.capture_counters_section(ui);
            }
        });
    }

    fn pipeline_section(&self, ui: &mut Ui) {
        theme::inset_frame().inner_margin(16).show(ui, |ui| {
            ui.set_width(ui.available_width());
            widgets::stack(ui, 12.0, |ui| {
                let stages = [
                    ("Capture", self.capture_ms),
                    ("Encode", self.encode_ms),
                    ("Encode → send", self.encode_queue_ms),
                    ("Send", self.send_ms),
                ];
                let total: f32 = stages.iter().map(|(_, value)| value).sum();
                let scale = stages
                    .iter()
                    .map(|(_, value)| *value)
                    .fold(4.0_f32, f32::max);
                ui.horizontal(|ui| {
                    widgets::overline(ui, "Pipeline average");
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        ui.label(widgets::mono_text(
                            format!("{total:.2} ms"),
                            12.0,
                            theme::MUTED,
                        ));
                    });
                });
                for (index, (label, value)) in stages.into_iter().enumerate() {
                    widgets::meter_row(ui, label, value, scale, theme::SELECTED);
                    if index == 0 {
                        ui.add_space(-8.0);
                        ui.horizontal(|ui| {
                            ui.add_space(116.0 - ui.spacing().item_spacing.x);
                            ui.label(widgets::text(
                                format!(
                                    "GPU wait {:.2} · lock {:.2}",
                                    self.gpu_wait_ms, self.gpu_lock_ms
                                ),
                                12.0,
                                Weight::Regular,
                                theme::MUTED,
                            ));
                        });
                    }
                }
            });
        });
    }

    fn encoder_stages_section(&self, ui: &mut Ui) {
        theme::inset_frame().inner_margin(16).show(ui, |ui| {
            ui.set_width(ui.available_width());
            widgets::stack(ui, 12.0, |ui| {
                ui.horizontal(|ui| {
                    widgets::overline(ui, "Encoder stages");
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        widgets::tag(ui, "Windows", Tone::Neutral);
                    });
                });
                widgets::stack(ui, 6.0, |ui| {
                    for (key, value) in [
                        (
                            "Copy · convert issue",
                            format!("{:.2} · {:.2}", self.copy_wait_ms, self.convert_wait_ms),
                        ),
                        ("Sync / map", format!("{:.2}", self.map_ms)),
                        (
                            "Submit · API · busy",
                            format!(
                                "{:.2} · {:.2} · {:.2}",
                                self.submit_ms, self.submit_api_ms, self.submit_busy_wait_ms
                            ),
                        ),
                        ("Busy retries", self.submit_busy_retries.to_string()),
                        ("Completion wait", format!("{:.2}", self.completion_wait_ms)),
                        (
                            "Bitstream · lock · copy",
                            format!(
                                "{:.2} · {:.2} · {:.2}",
                                self.bitstream_ms, self.bitstream_lock_ms, self.bitstream_copy_ms
                            ),
                        ),
                        (
                            "Unlock · unmap",
                            format!("{:.2} · {:.2}", self.bitstream_unlock_ms, self.unmap_ms),
                        ),
                    ] {
                        widgets::key_value(ui, key, &value, true, 13.0);
                    }
                });
            });
        });
    }

    fn capture_counters_section(&self, ui: &mut Ui) {
        theme::inset_frame().inner_margin(16).show(ui, |ui| {
            ui.set_width(ui.available_width());
            widgets::stack(ui, 12.0, |ui| {
                widgets::overline(ui, "Capture counters");
                widgets::counter_grid(
                    ui,
                    &[
                        ("NVENC submitted", self.nvenc_submissions.to_string()),
                        ("NVENC completed", self.nvenc_completions.to_string()),
                        ("DXGI timeouts", self.dxgi_timeouts.to_string()),
                        (
                            "Backlog (max)",
                            format!("{} ({})", self.dxgi_backlog, self.dxgi_backlog_max),
                        ),
                        ("Pacing skips", self.pacing_skips.to_string()),
                        ("Preprocess skips", self.preprocess_busy_skips.to_string()),
                        ("No-slot skips", self.no_free_slot_skips.to_string()),
                        (
                            "NVENC busy",
                            nvenc_busy_skips(
                                self.slot_busy_skips,
                                self.preprocess_busy_skips,
                                self.no_free_slot_skips,
                            )
                            .to_string(),
                        ),
                        ("Cursor-only", self.cursor_only_frames.to_string()),
                        ("Stale discarded", self.stale_frames.to_string()),
                    ],
                );
            });
        });
    }
}

fn watch_empty_stage(ui: &mut Ui, stage: Rect, audio_only: bool) {
    let (icon, title, body, last_step) = if audio_only {
        (
            Icon::Volume,
            "Audio only",
            "You'll hear the host's system audio as soon as they accept your request.",
            ("Listen", "System audio streams over UDP."),
        )
    } else {
        (
            Icon::Monitor,
            "No stream yet",
            "The host's screen appears here as soon as they accept your request.",
            (
                "Watch and listen",
                "Video and system audio stream over UDP.",
            ),
        )
    };
    centered_block(ui, stage.shrink(32.0), "watch-empty", 600.0, |ui| {
        widgets::stack(ui, 24.0, |ui| {
            widgets::icon_tile(ui, icon, Tone::Neutral);
            stage_message(ui, title, body);
            widgets::steps(
                ui,
                &[
                    ("Enter the host IP", "Port 3013 unless the host changed it."),
                    ("Host accepts", "They approve each viewer by IP."),
                    last_step,
                ],
            );
        });
    });
}
