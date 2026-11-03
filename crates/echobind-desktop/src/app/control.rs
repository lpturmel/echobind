//! Forwards the viewer's keyboard, mouse, and clipboard to a host that
//! allows remote control.
//!
//! The pointer is the viewer's own cursor: moving it over the video moves the
//! host's pointer to the same spot, and a click on the video sends that click
//! and gives the host the keyboard. A click anywhere else, or switching to
//! another app, takes the keyboard back. Fullscreen always forwards it.

use super::{EchobindApp, Mode};
use crate::session::ControlInput;
use echobind_core::input::{MouseButton, RemoteEvent, Shortcut};
use eframe::egui::{self, Event, Key, MouseWheelUnit, PointerButton, Pos2, Rect, Vec2};

/// egui's default scroll distance for one wheel notch, in points.
const POINTS_PER_NOTCH: f32 = 40.0;
const UNITS_PER_NOTCH: f32 = 120.0;

/// Where a frame drew the remote video, and the controls drawn over it.
pub(super) struct Stage {
    pub(super) video: Rect,
    pub(super) blockers: Vec<Rect>,
}

#[derive(Default)]
pub(super) struct RemoteControl {
    /// Set while painting; read by the next frame to route its input.
    pub(super) stage: Option<Stage>,
    /// The host has the keyboard.
    pub(super) keyboard: bool,
    buttons: Vec<MouseButton>,
    keys: Vec<u16>,
    scroll: Vec2,
}

impl RemoteControl {
    fn holds_anything(&self) -> bool {
        !self.buttons.is_empty() || !self.keys.is_empty()
    }

    /// Registers a control drawn over the video, which keeps its clicks.
    pub(super) fn block(&mut self, rect: Rect) {
        if let Some(stage) = &mut self.stage {
            stage.blockers.push(rect);
        }
    }
}

impl EchobindApp {
    /// Whether the viewer's input reaches the host right now.
    pub(super) fn controlling(&self) -> bool {
        self.mode == Mode::Connect
            && !self.reconnecting
            && self
                .session
                .as_ref()
                .is_some_and(|session| session.remote_control())
    }

    /// Runs before any widget, so keystrokes meant for the host can be taken
    /// out of this frame's input.
    pub(super) fn forward_input(&mut self, context: &egui::Context) {
        let stage = self.control.stage.take();
        let Some(stage) = stage.filter(|_| self.controlling()) else {
            self.release_control();
            return;
        };
        let fullscreen = self.fullscreen;
        if fullscreen {
            self.control.keyboard = true;
        }
        let over_stage = |pos: Pos2| {
            stage.video.contains(pos)
                && !stage.blockers.iter().any(|blocker| blocker.contains(pos))
                // Menus and popups float above the video.
                && context
                    .layer_id_at(pos)
                    .is_none_or(|layer| layer.order == egui::Order::Background)
        };

        let (events, mut pointer) =
            context.input(|input| (input.events.clone(), input.pointer.latest_pos()));
        let mut inputs = Vec::new();
        for event in &events {
            match event {
                Event::PointerMoved(pos) => {
                    pointer = Some(*pos);
                    // A drag that leaves the video keeps following the mouse.
                    if over_stage(*pos) || !self.control.buttons.is_empty() {
                        push_move(&mut inputs, stage.video, *pos);
                    }
                }
                Event::PointerButton {
                    pos,
                    button,
                    pressed,
                    ..
                } => {
                    let Some(button) = mouse_button(*button) else {
                        continue;
                    };
                    if *pressed {
                        if over_stage(*pos) {
                            self.control.keyboard = true;
                            push_move(&mut inputs, stage.video, *pos);
                            if !self.control.buttons.contains(&button) {
                                self.control.buttons.push(button);
                            }
                            inputs.push(ControlInput::Event(RemoteEvent::Button {
                                button,
                                pressed: true,
                            }));
                        } else if !fullscreen {
                            self.release_keyboard(&mut inputs);
                        }
                    } else if let Some(index) =
                        self.control.buttons.iter().position(|held| *held == button)
                    {
                        self.control.buttons.remove(index);
                        push_move(&mut inputs, stage.video, *pos);
                        inputs.push(ControlInput::Event(RemoteEvent::Button {
                            button,
                            pressed: false,
                        }));
                    }
                }
                Event::MouseWheel { unit, delta, .. } if pointer.is_some_and(over_stage) => {
                    let notches = match unit {
                        MouseWheelUnit::Point => *delta / POINTS_PER_NOTCH,
                        MouseWheelUnit::Line => *delta,
                        MouseWheelUnit::Page => *delta * 10.0,
                    };
                    self.control.scroll += notches * UNITS_PER_NOTCH;
                    let whole =
                        Vec2::new(self.control.scroll.x.trunc(), self.control.scroll.y.trunc());
                    self.control.scroll -= whole;
                    if whole != Vec2::ZERO {
                        let units =
                            |value: f32| value.clamp(i16::MIN.into(), i16::MAX.into()) as i16;
                        inputs.push(ControlInput::Event(RemoteEvent::Scroll {
                            x: units(whole.x),
                            y: units(whole.y),
                        }));
                    }
                }
                Event::Key {
                    key,
                    physical_key,
                    pressed,
                    ..
                } if self.control.keyboard && !reserved(*key, fullscreen) => {
                    // The physical key, so the host's layout decides what it types.
                    let Some(code) = hid_usage(physical_key.unwrap_or(*key)) else {
                        continue;
                    };
                    if *pressed {
                        if !self.control.keys.contains(&code) {
                            self.control.keys.push(code);
                        }
                        inputs.push(ControlInput::Event(RemoteEvent::Key {
                            code,
                            pressed: true,
                        }));
                    } else if let Some(index) =
                        self.control.keys.iter().position(|held| *held == code)
                    {
                        self.control.keys.remove(index);
                        inputs.push(ControlInput::Event(RemoteEvent::Key {
                            code,
                            pressed: false,
                        }));
                    }
                }
                // egui turns the clipboard shortcuts into these instead of
                // key presses, so the host performs its own.
                Event::Copy if self.control.keyboard => {
                    inputs.push(ControlInput::Event(RemoteEvent::Shortcut(Shortcut::Copy)));
                }
                Event::Cut if self.control.keyboard => {
                    inputs.push(ControlInput::Event(RemoteEvent::Shortcut(Shortcut::Cut)));
                }
                Event::Paste(text) if self.control.keyboard => {
                    inputs.push(ControlInput::Paste(text.clone()));
                }
                Event::WindowFocused(false) => {
                    self.release_keyboard(&mut inputs);
                    if !self.control.buttons.is_empty() {
                        self.control.buttons.clear();
                        inputs.push(ControlInput::Event(RemoteEvent::ReleaseAll));
                    }
                }
                _ => {}
            }
        }

        if self.control.keyboard {
            // Keep the host's keystrokes away from this app's own widgets.
            context.input_mut(|input| {
                input.events.retain(|event| match event {
                    Event::Key { key, .. } => reserved(*key, fullscreen),
                    Event::Text(_) | Event::Copy | Event::Cut | Event::Paste(_) => false,
                    _ => true,
                });
            });
            if let Some(focused) = context.memory(|memory| memory.focused()) {
                context.memory_mut(|memory| memory.surrender_focus(focused));
            }
        }
        if !inputs.is_empty() {
            if let Some(session) = &self.session {
                session.send_input(inputs);
            }
        }
    }

    /// Gives the keyboard back to this app, releasing the keys the host holds.
    fn release_keyboard(&mut self, inputs: &mut Vec<ControlInput>) {
        self.control.keyboard = false;
        for code in self.control.keys.drain(..) {
            inputs.push(ControlInput::Event(RemoteEvent::Key {
                code,
                pressed: false,
            }));
        }
    }

    /// Ends forwarding, as when the video goes away or the host disconnects.
    pub(super) fn release_control(&mut self) {
        if self.control.holds_anything() {
            if let Some(session) = &self.session {
                session.send_input(vec![ControlInput::Event(RemoteEvent::ReleaseAll)]);
            }
        }
        self.control = RemoteControl::default();
    }
}

/// Keys the app keeps for itself: F11 toggles fullscreen, F10 the stream
/// stats, and Escape leaves fullscreen.
fn reserved(key: Key, fullscreen: bool) -> bool {
    matches!(key, Key::F10 | Key::F11) || (fullscreen && key == Key::Escape)
}

fn push_move(inputs: &mut Vec<ControlInput>, video: Rect, pos: Pos2) {
    let scale = f32::from(u16::MAX);
    let normalized = |value: f32, start: f32, length: f32| {
        (((value - start) / length.max(1.0)).clamp(0.0, 1.0) * scale).round() as u16
    };
    let event = RemoteEvent::PointerMove {
        x: normalized(pos.x, video.left(), video.width()),
        y: normalized(pos.y, video.top(), video.height()),
    };
    // Only the latest position since the last button or key matters.
    if let Some(ControlInput::Event(last @ RemoteEvent::PointerMove { .. })) = inputs.last_mut() {
        *last = event;
    } else {
        inputs.push(ControlInput::Event(event));
    }
}

fn mouse_button(button: PointerButton) -> Option<MouseButton> {
    Some(match button {
        PointerButton::Primary => MouseButton::Left,
        PointerButton::Secondary => MouseButton::Right,
        PointerButton::Middle => MouseButton::Middle,
        PointerButton::Extra1 => MouseButton::Back,
        PointerButton::Extra2 => MouseButton::Forward,
    })
}

/// The USB HID keyboard usage for an egui key.
fn hid_usage(key: Key) -> Option<u16> {
    Some(match key {
        Key::A => 0x04,
        Key::B => 0x05,
        Key::C => 0x06,
        Key::D => 0x07,
        Key::E => 0x08,
        Key::F => 0x09,
        Key::G => 0x0a,
        Key::H => 0x0b,
        Key::I => 0x0c,
        Key::J => 0x0d,
        Key::K => 0x0e,
        Key::L => 0x0f,
        Key::M => 0x10,
        Key::N => 0x11,
        Key::O => 0x12,
        Key::P => 0x13,
        Key::Q => 0x14,
        Key::R => 0x15,
        Key::S => 0x16,
        Key::T => 0x17,
        Key::U => 0x18,
        Key::V => 0x19,
        Key::W => 0x1a,
        Key::X => 0x1b,
        Key::Y => 0x1c,
        Key::Z => 0x1d,
        Key::Num1 | Key::Exclamationmark => 0x1e,
        Key::Num2 => 0x1f,
        Key::Num3 => 0x20,
        Key::Num4 => 0x21,
        Key::Num5 => 0x22,
        Key::Num6 => 0x23,
        Key::Num7 => 0x24,
        Key::Num8 => 0x25,
        Key::Num9 => 0x26,
        Key::Num0 => 0x27,
        Key::Enter => 0x28,
        Key::Escape => 0x29,
        Key::Backspace => 0x2a,
        Key::Tab => 0x2b,
        Key::Space => 0x2c,
        Key::Minus => 0x2d,
        Key::Equals => 0x2e,
        Key::OpenBracket | Key::OpenCurlyBracket => 0x2f,
        Key::CloseBracket | Key::CloseCurlyBracket => 0x30,
        Key::Backslash | Key::Pipe => 0x31,
        Key::Semicolon | Key::Colon => 0x33,
        Key::Quote => 0x34,
        Key::Backtick => 0x35,
        Key::Comma => 0x36,
        Key::Period => 0x37,
        Key::Slash | Key::Questionmark => 0x38,
        Key::F1 => 0x3a,
        Key::F2 => 0x3b,
        Key::F3 => 0x3c,
        Key::F4 => 0x3d,
        Key::F5 => 0x3e,
        Key::F6 => 0x3f,
        Key::F7 => 0x40,
        Key::F8 => 0x41,
        Key::F9 => 0x42,
        Key::F10 => 0x43,
        Key::F11 => 0x44,
        Key::F12 => 0x45,
        Key::Insert => 0x49,
        Key::Home => 0x4a,
        Key::PageUp => 0x4b,
        Key::Delete => 0x4c,
        Key::End => 0x4d,
        Key::PageDown => 0x4e,
        Key::ArrowRight => 0x4f,
        Key::ArrowLeft => 0x50,
        Key::ArrowDown => 0x51,
        Key::ArrowUp => 0x52,
        // winit reports the keypad + as Plus; every other keypad key shares
        // its egui key with the main keyboard.
        Key::Plus => 0x57,
        Key::IntlBackslash => 0x64,
        Key::F13 => 0x68,
        Key::F14 => 0x69,
        Key::F15 => 0x6a,
        Key::F16 => 0x6b,
        Key::F17 => 0x6c,
        Key::F18 => 0x6d,
        Key::F19 => 0x6e,
        Key::F20 => 0x6f,
        Key::F21 => 0x70,
        Key::F22 => 0x71,
        Key::F23 => 0x72,
        Key::F24 => 0x73,
        Key::ControlLeft => 0xe0,
        Key::ShiftLeft => 0xe1,
        Key::AltLeft => 0xe2,
        Key::SuperLeft => 0xe3,
        Key::ControlRight => 0xe4,
        Key::ShiftRight => 0xe5,
        Key::AltRight => 0xe6,
        Key::SuperRight => 0xe7,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_keys_to_hid_usages() {
        assert_eq!(hid_usage(Key::A), Some(0x04));
        assert_eq!(hid_usage(Key::Z), Some(0x1d));
        assert_eq!(hid_usage(Key::Num0), Some(0x27));
        assert_eq!(hid_usage(Key::F12), Some(0x45));
        assert_eq!(hid_usage(Key::SuperLeft), Some(0xe3));
        assert_eq!(hid_usage(Key::Copy), None);
    }

    #[test]
    fn only_app_shortcuts_stay_local() {
        assert!(reserved(Key::F11, false));
        assert!(reserved(Key::F10, false));
        assert!(!reserved(Key::Escape, false));
        assert!(reserved(Key::Escape, true));
        assert!(!reserved(Key::A, true));
    }

    #[test]
    fn pointer_positions_are_normalized_and_coalesced() {
        let video = Rect::from_min_size(Pos2::new(100.0, 50.0), Vec2::new(200.0, 100.0));
        let mut inputs = Vec::new();
        push_move(&mut inputs, video, Pos2::new(100.0, 50.0));
        push_move(&mut inputs, video, Pos2::new(400.0, 100.0));
        assert_eq!(inputs.len(), 1);
        let ControlInput::Event(RemoteEvent::PointerMove { x, y }) = &inputs[0] else {
            panic!("expected a pointer move");
        };
        assert_eq!((*x, *y), (u16::MAX, u16::MAX / 2 + 1));
    }
}
