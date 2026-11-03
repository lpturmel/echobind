//! Applies a viewer's keyboard, mouse, and clipboard events on the host.
//!
//! Injection runs on its own thread, fed by the host network thread, so a
//! slow system input call can never delay a keyframe request, a pong, or any
//! stage of the video pipeline.

use super::SessionEvent;
use echobind_core::input::{ClipboardAssembler, MouseButton, RemoteEvent, Shortcut};
use std::{
    collections::HashSet,
    sync::mpsc,
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
use tracing::debug;

/// Whether this platform can inject input at all.
pub(super) const SUPPORTED: bool = cfg!(any(target_os = "macos", target_os = "windows"));

/// How long after a copy or cut the host watches its clipboard for the
/// result to send back to the viewer.
const COPY_WATCH: Duration = Duration::from_secs(2);
const COPY_POLL_INTERVAL: Duration = Duration::from_millis(30);

// USB HID keyboard usages for the keys the injector itself presses.
const HID_C: u16 = 0x06;
const HID_V: u16 = 0x19;
const HID_X: u16 = 0x1b;
const HID_LEFT_CONTROL: u16 = 0xe0;
const HID_LEFT_SHIFT: u16 = 0xe1;
const HID_LEFT_ALT: u16 = 0xe2;
const HID_LEFT_GUI: u16 = 0xe3;
const HID_RIGHT_CONTROL: u16 = 0xe4;
const HID_RIGHT_SHIFT: u16 = 0xe5;
const HID_RIGHT_ALT: u16 = 0xe6;
const HID_RIGHT_GUI: u16 = 0xe7;

fn is_modifier(code: u16) -> bool {
    (HID_LEFT_CONTROL..=HID_RIGHT_GUI).contains(&code)
}

pub(super) enum InjectorCommand {
    Events(Vec<RemoteEvent>),
    /// The viewer changed or left: let go of everything it held.
    ReleaseAll,
}

/// Starts the injector. Clipboard text copied on the host at the viewer's
/// request goes out through `clipboard_out`.
pub(super) fn spawn_injector(
    commands: mpsc::Receiver<InjectorCommand>,
    clipboard_out: mpsc::Sender<String>,
    events: mpsc::Sender<SessionEvent>,
) -> JoinHandle<()> {
    thread::Builder::new()
        .name("echobind-input".to_owned())
        .spawn(move || {
            if let Err(reason) = platform::ensure_permission() {
                let _ = events.send(SessionEvent::ControlUnavailable(reason));
            }
            let mut injector = Injector::new(platform::Platform::new());
            loop {
                let timeout = if injector.copy_watch.is_some() {
                    COPY_POLL_INTERVAL
                } else {
                    Duration::from_secs(1)
                };
                match commands.recv_timeout(timeout) {
                    Ok(InjectorCommand::Events(batch)) => {
                        for event in batch {
                            injector.apply(event);
                        }
                    }
                    Ok(InjectorCommand::ReleaseAll) => injector.release_all(),
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                    // The host network thread exited: sharing stopped.
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                }
                if let Some(text) = injector.poll_copy() {
                    let _ = clipboard_out.send(text);
                }
            }
            injector.release_all();
        })
        .expect("unable to start the input injector")
}

/// What one platform must provide to replay a viewer's input.
trait InputBackend {
    fn move_pointer(&mut self, x: u16, y: u16, dragging: Option<MouseButton>);
    fn button(&mut self, button: MouseButton, pressed: bool, click_count: i64);
    fn scroll(&mut self, x: i16, y: i16);
    /// Presses or releases a HID key. `modifiers` lists every modifier held
    /// once the event applies; `repeat` marks a held key's repeated press.
    fn key(&mut self, code: u16, pressed: bool, repeat: bool, modifiers: &HashSet<u16>) -> bool;
    /// The modifier that drives this platform's clipboard shortcuts.
    fn command_key(&self) -> u16;
    fn clipboard_change_count(&mut self) -> u64;
    fn clipboard_text(&mut self) -> Option<String>;
    fn set_clipboard_text(&mut self, text: &str);
}

struct Injector<B: InputBackend> {
    backend: B,
    keys: HashSet<u16>,
    buttons: Vec<MouseButton>,
    pointer: (u16, u16),
    last_click: Option<(MouseButton, Instant, (u16, u16), i64)>,
    clipboard: ClipboardAssembler,
    copy_watch: Option<(u64, Instant)>,
}

impl<B: InputBackend> Injector<B> {
    fn new(backend: B) -> Self {
        Self {
            backend,
            keys: HashSet::new(),
            buttons: Vec::new(),
            pointer: (0, 0),
            last_click: None,
            clipboard: ClipboardAssembler::default(),
            copy_watch: None,
        }
    }

    fn apply(&mut self, event: RemoteEvent) {
        match event {
            RemoteEvent::PointerMove { x, y } => {
                self.pointer = (x, y);
                self.backend
                    .move_pointer(x, y, self.buttons.first().copied());
            }
            RemoteEvent::Button { button, pressed } => {
                if pressed {
                    let click_count = self.click_count(button);
                    if !self.buttons.contains(&button) {
                        self.buttons.push(button);
                    }
                    self.backend.button(button, true, click_count);
                } else if let Some(index) = self.buttons.iter().position(|held| *held == button) {
                    self.buttons.remove(index);
                    let click_count = self.last_click.map_or(1, |(.., count)| count);
                    self.backend.button(button, false, click_count);
                }
            }
            RemoteEvent::Scroll { x, y } => self.backend.scroll(x, y),
            RemoteEvent::Key { code, pressed } => self.key(code, pressed),
            RemoteEvent::Shortcut(shortcut) => self.shortcut(shortcut),
            RemoteEvent::ClipboardText { first, last, chunk } => {
                if let Some(text) = self.clipboard.push(first, last, &chunk) {
                    self.backend.set_clipboard_text(&text);
                }
            }
            RemoteEvent::ReleaseAll => self.release_all(),
        }
    }

    /// macOS needs each click numbered for double and triple clicks, which a
    /// real mouse gets from the window server.
    fn click_count(&mut self, button: MouseButton) -> i64 {
        const DOUBLE_CLICK: Duration = Duration::from_millis(500);
        const SLOP: u16 = 400; // about 4 px on a 4K display
        let now = Instant::now();
        let count = match self.last_click {
            Some((previous, at, (x, y), count))
                if previous == button
                    && now.duration_since(at) <= DOUBLE_CLICK
                    && x.abs_diff(self.pointer.0) <= SLOP
                    && y.abs_diff(self.pointer.1) <= SLOP =>
            {
                count + 1
            }
            _ => 1,
        };
        self.last_click = Some((button, now, self.pointer, count));
        count
    }

    fn key(&mut self, code: u16, pressed: bool) {
        if pressed {
            let repeat = !self.keys.insert(code);
            let modifiers = self.held_modifiers();
            if !self.backend.key(code, true, repeat, &modifiers) && !repeat {
                self.keys.remove(&code);
            }
        } else if self.keys.remove(&code) {
            // A release of a key the host never pressed, such as the C of a
            // viewer's copy shortcut, is dropped.
            let modifiers = self.held_modifiers();
            self.backend.key(code, false, false, &modifiers);
        }
    }

    fn held_modifiers(&self) -> HashSet<u16> {
        self.keys
            .iter()
            .copied()
            .filter(|code| is_modifier(*code))
            .collect()
    }

    /// Performs a clipboard command with this host's own shortcut, whatever
    /// modifiers the viewer holds. The command key goes down before any other
    /// modifier is released, so releasing a lone Windows key can't open the
    /// Start menu. Modifiers the viewer still holds stay released until it
    /// presses them again.
    fn shortcut(&mut self, shortcut: Shortcut) {
        let command = self.backend.command_key();
        let letter = match shortcut {
            Shortcut::Copy => HID_C,
            Shortcut::Cut => HID_X,
            Shortcut::Paste => HID_V,
        };
        if matches!(shortcut, Shortcut::Copy | Shortcut::Cut) {
            let baseline = self.backend.clipboard_change_count();
            self.copy_watch = Some((baseline, Instant::now() + COPY_WATCH));
        }
        let command_held = self.keys.contains(&command);
        if !command_held {
            self.key(command, true);
        }
        let others: Vec<u16> = self
            .held_modifiers()
            .into_iter()
            .filter(|code| *code != command)
            .collect();
        for code in others {
            self.key(code, false);
        }
        self.key(letter, true);
        self.key(letter, false);
        if !command_held {
            self.key(command, false);
        }
    }

    /// Returns the host's clipboard text once a copy the viewer asked for
    /// changes it.
    fn poll_copy(&mut self) -> Option<String> {
        let (baseline, deadline) = self.copy_watch?;
        if self.backend.clipboard_change_count() != baseline {
            self.copy_watch = None;
            return self.backend.clipboard_text();
        }
        if Instant::now() >= deadline {
            debug!("The host clipboard didn't change after the viewer's copy");
            self.copy_watch = None;
        }
        None
    }

    fn release_all(&mut self) {
        // Ordinary keys first, so no shortcut fires as modifiers come up.
        let mut keys: Vec<u16> = self.keys.iter().copied().collect();
        keys.sort_by_key(|code| is_modifier(*code));
        for code in keys {
            self.key(code, false);
        }
        for button in std::mem::take(&mut self.buttons) {
            self.backend.button(button, false, 1);
        }
        self.clipboard = ClipboardAssembler::default();
    }
}

#[cfg(target_os = "windows")]
mod platform {
    use super::{InputBackend, MouseButton, HID_LEFT_CONTROL};
    use std::collections::HashSet;
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYBD_EVENT_FLAGS,
        KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE, MOUSEEVENTF_ABSOLUTE,
        MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN,
        MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP,
        MOUSEEVENTF_WHEEL, MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, MOUSEINPUT, MOUSE_EVENT_FLAGS,
        VIRTUAL_KEY,
    };

    const XBUTTON1: u32 = 1;
    const XBUTTON2: u32 = 2;

    pub(super) fn ensure_permission() -> Result<(), String> {
        Ok(())
    }

    pub(super) struct Platform;

    impl Platform {
        pub(super) fn new() -> Self {
            Self
        }
    }

    fn send(input: INPUT) {
        // Windows refuses input aimed at an elevated window or the secure
        // desktop (UAC, the lock screen); nothing more can be done then.
        let sent = unsafe { SendInput(&[input], size_of::<INPUT>() as i32) };
        if sent == 0 {
            tracing::debug!("Windows dropped an injected input event");
        }
    }

    fn mouse(dx: i32, dy: i32, data: u32, flags: MOUSE_EVENT_FLAGS) {
        send(INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 {
                mi: MOUSEINPUT {
                    dx,
                    dy,
                    mouseData: data,
                    dwFlags: flags,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        });
    }

    impl InputBackend for Platform {
        fn move_pointer(&mut self, x: u16, y: u16, _dragging: Option<MouseButton>) {
            // Absolute coordinates span the primary display, which is the
            // display the host captures.
            mouse(
                i32::from(x),
                i32::from(y),
                0,
                MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE,
            );
        }

        fn button(&mut self, button: MouseButton, pressed: bool, _click_count: i64) {
            let (flags, data) = match (button, pressed) {
                (MouseButton::Left, true) => (MOUSEEVENTF_LEFTDOWN, 0),
                (MouseButton::Left, false) => (MOUSEEVENTF_LEFTUP, 0),
                (MouseButton::Right, true) => (MOUSEEVENTF_RIGHTDOWN, 0),
                (MouseButton::Right, false) => (MOUSEEVENTF_RIGHTUP, 0),
                (MouseButton::Middle, true) => (MOUSEEVENTF_MIDDLEDOWN, 0),
                (MouseButton::Middle, false) => (MOUSEEVENTF_MIDDLEUP, 0),
                (MouseButton::Back, true) => (MOUSEEVENTF_XDOWN, XBUTTON1),
                (MouseButton::Back, false) => (MOUSEEVENTF_XUP, XBUTTON1),
                (MouseButton::Forward, true) => (MOUSEEVENTF_XDOWN, XBUTTON2),
                (MouseButton::Forward, false) => (MOUSEEVENTF_XUP, XBUTTON2),
            };
            mouse(0, 0, data, flags);
        }

        fn scroll(&mut self, x: i16, y: i16) {
            // A positive wheel delta scrolls up, which moves content down;
            // a positive horizontal delta scrolls right, moving content left.
            if y != 0 {
                mouse(0, 0, i32::from(y) as u32, MOUSEEVENTF_WHEEL);
            }
            if x != 0 {
                mouse(0, 0, (-i32::from(x)) as u32, MOUSEEVENTF_HWHEEL);
            }
        }

        fn key(&mut self, code: u16, pressed: bool, _repeat: bool, _: &HashSet<u16>) -> bool {
            let Some((scan, extended)) = scancode(code) else {
                return false;
            };
            let mut flags = KEYEVENTF_SCANCODE;
            if extended {
                flags |= KEYEVENTF_EXTENDEDKEY;
            }
            if !pressed {
                flags |= KEYEVENTF_KEYUP;
            }
            send(INPUT {
                r#type: INPUT_KEYBOARD,
                Anonymous: INPUT_0 {
                    ki: KEYBDINPUT {
                        wVk: VIRTUAL_KEY(0),
                        wScan: scan,
                        dwFlags: KEYBD_EVENT_FLAGS(flags.0),
                        time: 0,
                        dwExtraInfo: 0,
                    },
                },
            });
            true
        }

        fn command_key(&self) -> u16 {
            HID_LEFT_CONTROL
        }

        fn clipboard_change_count(&mut self) -> u64 {
            clipboard_win::raw::seq_num().map_or(0, |count| u64::from(count.get()))
        }

        fn clipboard_text(&mut self) -> Option<String> {
            clipboard_win::get_clipboard_string().ok()
        }

        fn set_clipboard_text(&mut self, text: &str) {
            // Viewers send bare newlines; Windows text uses CRLF.
            let text = text.replace("\r\n", "\n").replace('\n', "\r\n");
            if let Err(error) = clipboard_win::set_clipboard_string(&text) {
                tracing::warn!("Unable to set the clipboard for the viewer: {error}");
            }
        }
    }

    /// The set-1 scancode for a HID usage, and whether it needs the E0
    /// extended prefix.
    fn scancode(code: u16) -> Option<(u16, bool)> {
        const LETTERS: [u16; 26] = [
            0x1e, 0x30, 0x2e, 0x20, 0x12, 0x21, 0x22, 0x23, 0x17, 0x24, 0x25, 0x26, 0x32, 0x31,
            0x18, 0x19, 0x10, 0x13, 0x1f, 0x14, 0x16, 0x2f, 0x11, 0x2d, 0x15, 0x2c,
        ];
        let plain = |scan| Some((scan, false));
        let extended = |scan| Some((scan, true));
        match code {
            0x04..=0x1d => plain(LETTERS[usize::from(code - 0x04)]),
            // 1 through 9, then 0.
            0x1e..=0x27 => plain(code - 0x1e + 0x02),
            0x28 => plain(0x1c),                      // Enter
            0x29 => plain(0x01),                      // Escape
            0x2a => plain(0x0e),                      // Backspace
            0x2b => plain(0x0f),                      // Tab
            0x2c => plain(0x39),                      // Space
            0x2d => plain(0x0c),                      // -
            0x2e => plain(0x0d),                      // =
            0x2f => plain(0x1a),                      // [
            0x30 => plain(0x1b),                      // ]
            0x31 => plain(0x2b),                      // backslash
            0x33 => plain(0x27),                      // ;
            0x34 => plain(0x28),                      // '
            0x35 => plain(0x29),                      // `
            0x36 => plain(0x33),                      // ,
            0x37 => plain(0x34),                      // .
            0x38 => plain(0x35),                      // /
            0x39 => plain(0x3a),                      // Caps Lock
            0x3a..=0x43 => plain(code - 0x3a + 0x3b), // F1–F10
            0x44 => plain(0x57),                      // F11
            0x45 => plain(0x58),                      // F12
            0x49 => extended(0x52),                   // Insert
            0x4a => extended(0x47),                   // Home
            0x4b => extended(0x49),                   // Page Up
            0x4c => extended(0x53),                   // Delete
            0x4d => extended(0x4f),                   // End
            0x4e => extended(0x51),                   // Page Down
            0x4f => extended(0x4d),                   // Right
            0x50 => extended(0x4b),                   // Left
            0x51 => extended(0x50),                   // Down
            0x52 => extended(0x48),                   // Up
            0x57 => plain(0x4e),                      // Keypad +
            0x64 => plain(0x56),                      // ISO backslash
            0x68..=0x72 => plain(code - 0x68 + 0x64), // F13–F23
            0x73 => plain(0x76),                      // F24
            0xe0 => plain(0x1d),                      // Left Control
            0xe1 => plain(0x2a),                      // Left Shift
            0xe2 => plain(0x38),                      // Left Alt
            0xe3 => extended(0x5b),                   // Left Windows
            0xe4 => extended(0x1d),                   // Right Control
            0xe5 => plain(0x36),                      // Right Shift
            0xe6 => extended(0x38),                   // Right Alt
            0xe7 => extended(0x5c),                   // Right Windows
            _ => None,
        }
    }

    #[cfg(test)]
    mod tests {
        use super::scancode;

        #[test]
        fn maps_hid_usages_to_set_one_scancodes() {
            assert_eq!(scancode(0x04), Some((0x1e, false))); // A
            assert_eq!(scancode(0x1d), Some((0x2c, false))); // Z
            assert_eq!(scancode(0x27), Some((0x0b, false))); // 0
            assert_eq!(scancode(0x43), Some((0x44, false))); // F10
            assert_eq!(scancode(0x50), Some((0x4b, true))); // Left arrow
            assert_eq!(scancode(0x01), None);
        }
    }
}

#[cfg(target_os = "macos")]
mod platform {
    use super::{
        InputBackend, MouseButton, HID_LEFT_ALT, HID_LEFT_CONTROL, HID_LEFT_GUI, HID_LEFT_SHIFT,
        HID_RIGHT_ALT, HID_RIGHT_CONTROL, HID_RIGHT_GUI, HID_RIGHT_SHIFT,
    };
    use objc2_app_kit::{NSPasteboard, NSPasteboardTypeString};
    use objc2_foundation::NSString;
    use std::{collections::HashSet, ffi::c_void};

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct CGPoint {
        x: f64,
        y: f64,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct CGSize {
        width: f64,
        height: f64,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct CGRect {
        origin: CGPoint,
        size: CGSize,
    }

    type CGEventRef = *mut c_void;

    const HID_EVENT_TAP: u32 = 0;
    const LEFT_MOUSE_DOWN: u32 = 1;
    const LEFT_MOUSE_UP: u32 = 2;
    const RIGHT_MOUSE_DOWN: u32 = 3;
    const RIGHT_MOUSE_UP: u32 = 4;
    const MOUSE_MOVED: u32 = 5;
    const LEFT_MOUSE_DRAGGED: u32 = 6;
    const RIGHT_MOUSE_DRAGGED: u32 = 7;
    const OTHER_MOUSE_DOWN: u32 = 25;
    const OTHER_MOUSE_UP: u32 = 26;
    const OTHER_MOUSE_DRAGGED: u32 = 27;
    const MOUSE_EVENT_CLICK_STATE: u32 = 1;
    const KEYBOARD_EVENT_AUTOREPEAT: u32 = 8;
    const SCROLL_UNIT_PIXEL: u32 = 0;
    const FLAG_SHIFT: u64 = 0x0002_0000;
    const FLAG_CONTROL: u64 = 0x0004_0000;
    const FLAG_ALTERNATE: u64 = 0x0008_0000;
    const FLAG_COMMAND: u64 = 0x0010_0000;
    /// Pixels per wheel notch; the viewer sends 120 units a notch.
    const PIXELS_PER_NOTCH: i32 = 40;

    #[link(name = "CoreGraphics", kind = "framework")]
    extern "C" {
        fn CGMainDisplayID() -> u32;
        fn CGDisplayBounds(display: u32) -> CGRect;
        fn CGEventCreateMouseEvent(
            source: *const c_void,
            kind: u32,
            position: CGPoint,
            button: u32,
        ) -> CGEventRef;
        fn CGEventCreateKeyboardEvent(source: *const c_void, key: u16, down: bool) -> CGEventRef;
        fn CGEventCreateScrollWheelEvent2(
            source: *const c_void,
            units: u32,
            wheel_count: u32,
            wheel1: i32,
            wheel2: i32,
            wheel3: i32,
        ) -> CGEventRef;
        fn CGEventSetFlags(event: CGEventRef, flags: u64);
        fn CGEventSetIntegerValueField(event: CGEventRef, field: u32, value: i64);
        fn CGEventPost(tap: u32, event: CGEventRef);
    }

    #[link(name = "CoreFoundation", kind = "framework")]
    extern "C" {
        fn CFRelease(object: *const c_void);
        fn CFDictionaryCreate(
            allocator: *const c_void,
            keys: *const *const c_void,
            values: *const *const c_void,
            count: isize,
            key_callbacks: *const c_void,
            value_callbacks: *const c_void,
        ) -> *const c_void;
        static kCFBooleanTrue: *const c_void;
        static kCFTypeDictionaryKeyCallBacks: c_void;
        static kCFTypeDictionaryValueCallBacks: c_void;
    }

    #[link(name = "ApplicationServices", kind = "framework")]
    extern "C" {
        fn AXIsProcessTrustedWithOptions(options: *const c_void) -> bool;
        static kAXTrustedCheckOptionPrompt: *const c_void;
    }

    /// macOS drops synthetic input from apps without Accessibility access.
    /// Asking also shows the system prompt the first time.
    pub(super) fn ensure_permission() -> Result<(), String> {
        let trusted = unsafe {
            let keys = [kAXTrustedCheckOptionPrompt];
            let values = [kCFBooleanTrue];
            let options = CFDictionaryCreate(
                std::ptr::null(),
                keys.as_ptr(),
                values.as_ptr(),
                1,
                std::ptr::addr_of!(kCFTypeDictionaryKeyCallBacks).cast(),
                std::ptr::addr_of!(kCFTypeDictionaryValueCallBacks).cast(),
            );
            let trusted = AXIsProcessTrustedWithOptions(options);
            if !options.is_null() {
                CFRelease(options);
            }
            trusted
        };
        if trusted {
            Ok(())
        } else {
            Err("Turn on Echobind in System Settings › Privacy & Security › Accessibility, then start sharing again.".to_owned())
        }
    }

    pub(super) struct Platform {
        position: CGPoint,
        scroll_remainder: (i32, i32),
    }

    impl Platform {
        pub(super) fn new() -> Self {
            Self {
                position: CGPoint { x: 0.0, y: 0.0 },
                scroll_remainder: (0, 0),
            }
        }
    }

    fn post(event: CGEventRef, configure: impl FnOnce(CGEventRef)) {
        if event.is_null() {
            return;
        }
        unsafe {
            configure(event);
            CGEventPost(HID_EVENT_TAP, event);
            CFRelease(event);
        }
    }

    fn button_number(button: MouseButton) -> u32 {
        match button {
            MouseButton::Left => 0,
            MouseButton::Right => 1,
            MouseButton::Middle => 2,
            MouseButton::Back => 3,
            MouseButton::Forward => 4,
        }
    }

    fn flags(modifiers: &HashSet<u16>) -> u64 {
        modifiers.iter().fold(0, |flags, code| {
            flags
                | match *code {
                    HID_LEFT_SHIFT | HID_RIGHT_SHIFT => FLAG_SHIFT,
                    HID_LEFT_CONTROL | HID_RIGHT_CONTROL => FLAG_CONTROL,
                    HID_LEFT_ALT | HID_RIGHT_ALT => FLAG_ALTERNATE,
                    HID_LEFT_GUI | HID_RIGHT_GUI => FLAG_COMMAND,
                    _ => 0,
                }
        })
    }

    impl InputBackend for Platform {
        fn move_pointer(&mut self, x: u16, y: u16, dragging: Option<MouseButton>) {
            let bounds = unsafe { CGDisplayBounds(CGMainDisplayID()) };
            let scale = f64::from(u16::MAX);
            self.position = CGPoint {
                x: bounds.origin.x + f64::from(x) / scale * (bounds.size.width - 1.0),
                y: bounds.origin.y + f64::from(y) / scale * (bounds.size.height - 1.0),
            };
            // Apps only see a drag from dragged events, not plain moves.
            let (kind, button) = match dragging {
                None => (MOUSE_MOVED, 0),
                Some(MouseButton::Left) => (LEFT_MOUSE_DRAGGED, 0),
                Some(MouseButton::Right) => (RIGHT_MOUSE_DRAGGED, 1),
                Some(other) => (OTHER_MOUSE_DRAGGED, button_number(other)),
            };
            post(
                unsafe { CGEventCreateMouseEvent(std::ptr::null(), kind, self.position, button) },
                |_| {},
            );
        }

        fn button(&mut self, button: MouseButton, pressed: bool, click_count: i64) {
            let kind = match (button, pressed) {
                (MouseButton::Left, true) => LEFT_MOUSE_DOWN,
                (MouseButton::Left, false) => LEFT_MOUSE_UP,
                (MouseButton::Right, true) => RIGHT_MOUSE_DOWN,
                (MouseButton::Right, false) => RIGHT_MOUSE_UP,
                (_, true) => OTHER_MOUSE_DOWN,
                (_, false) => OTHER_MOUSE_UP,
            };
            post(
                unsafe {
                    CGEventCreateMouseEvent(
                        std::ptr::null(),
                        kind,
                        self.position,
                        button_number(button),
                    )
                },
                |event| unsafe {
                    CGEventSetIntegerValueField(event, MOUSE_EVENT_CLICK_STATE, click_count);
                },
            );
        }

        fn scroll(&mut self, x: i16, y: i16) {
            // Accumulate units so slow trackpad motion isn't rounded away.
            let x = self.scroll_remainder.0 + i32::from(x) * PIXELS_PER_NOTCH;
            let y = self.scroll_remainder.1 + i32::from(y) * PIXELS_PER_NOTCH;
            self.scroll_remainder = (x % 120, y % 120);
            let (x, y) = (x / 120, y / 120);
            if x == 0 && y == 0 {
                return;
            }
            // Positive wheel 1 scrolls up and positive wheel 2 left, both of
            // which move the content the way the viewer's delta does.
            post(
                unsafe {
                    CGEventCreateScrollWheelEvent2(std::ptr::null(), SCROLL_UNIT_PIXEL, 2, y, x, 0)
                },
                |_| {},
            );
        }

        fn key(
            &mut self,
            code: u16,
            pressed: bool,
            repeat: bool,
            modifiers: &HashSet<u16>,
        ) -> bool {
            let Some(keycode) = virtual_keycode(code) else {
                return false;
            };
            post(
                unsafe { CGEventCreateKeyboardEvent(std::ptr::null(), keycode, pressed) },
                |event| unsafe {
                    CGEventSetFlags(event, flags(modifiers));
                    if repeat {
                        CGEventSetIntegerValueField(event, KEYBOARD_EVENT_AUTOREPEAT, 1);
                    }
                },
            );
            true
        }

        fn command_key(&self) -> u16 {
            HID_LEFT_GUI
        }

        fn clipboard_change_count(&mut self) -> u64 {
            NSPasteboard::generalPasteboard().changeCount() as u64
        }

        fn clipboard_text(&mut self) -> Option<String> {
            NSPasteboard::generalPasteboard()
                .stringForType(unsafe { NSPasteboardTypeString })
                .map(|text| text.to_string())
        }

        fn set_clipboard_text(&mut self, text: &str) {
            let pasteboard = NSPasteboard::generalPasteboard();
            pasteboard.clearContents();
            pasteboard
                .setString_forType(&NSString::from_str(text), unsafe { NSPasteboardTypeString });
        }
    }

    /// The macOS virtual keycode (`kVK_*`) for a HID usage.
    fn virtual_keycode(code: u16) -> Option<u16> {
        const LETTERS: [u16; 26] = [
            0x00, 0x0b, 0x08, 0x02, 0x0e, 0x03, 0x05, 0x04, 0x22, 0x26, 0x28, 0x25, 0x2e, 0x2d,
            0x1f, 0x23, 0x0c, 0x0f, 0x01, 0x11, 0x20, 0x09, 0x0d, 0x07, 0x10, 0x06,
        ];
        // 1 through 9, then 0.
        const DIGITS: [u16; 10] = [0x12, 0x13, 0x14, 0x15, 0x17, 0x16, 0x1a, 0x1c, 0x19, 0x1d];
        const F1_TO_F12: [u16; 12] = [
            0x7a, 0x78, 0x63, 0x76, 0x60, 0x61, 0x62, 0x64, 0x65, 0x6d, 0x67, 0x6f,
        ];
        const F13_TO_F20: [u16; 8] = [0x69, 0x6b, 0x71, 0x6a, 0x40, 0x4f, 0x50, 0x5a];
        Some(match code {
            0x04..=0x1d => LETTERS[usize::from(code - 0x04)],
            0x1e..=0x27 => DIGITS[usize::from(code - 0x1e)],
            0x28 => 0x24, // Return
            0x29 => 0x35, // Escape
            0x2a => 0x33, // Delete (backspace)
            0x2b => 0x30, // Tab
            0x2c => 0x31, // Space
            0x2d => 0x1b, // -
            0x2e => 0x18, // =
            0x2f => 0x21, // [
            0x30 => 0x1e, // ]
            0x31 => 0x2a, // backslash
            0x33 => 0x29, // ;
            0x34 => 0x27, // '
            0x35 => 0x32, // `
            0x36 => 0x2b, // ,
            0x37 => 0x2f, // .
            0x38 => 0x2c, // /
            0x39 => 0x39, // Caps Lock
            0x3a..=0x45 => F1_TO_F12[usize::from(code - 0x3a)],
            0x49 => 0x72, // Insert, as Help
            0x4a => 0x73, // Home
            0x4b => 0x74, // Page Up
            0x4c => 0x75, // Forward Delete
            0x4d => 0x77, // End
            0x4e => 0x79, // Page Down
            0x4f => 0x7c, // Right
            0x50 => 0x7b, // Left
            0x51 => 0x7d, // Down
            0x52 => 0x7e, // Up
            0x57 => 0x45, // Keypad +
            0x64 => 0x0a, // ISO section
            0x68..=0x6f => F13_TO_F20[usize::from(code - 0x68)],
            0xe0 => 0x3b, // Control
            0xe1 => 0x38, // Shift
            0xe2 => 0x3a, // Option
            0xe3 => 0x37, // Command
            0xe4 => 0x3e, // Right Control
            0xe5 => 0x3c, // Right Shift
            0xe6 => 0x3d, // Right Option
            0xe7 => 0x36, // Right Command
            _ => return None,
        })
    }

    #[cfg(test)]
    mod tests {
        use super::virtual_keycode;

        #[test]
        fn maps_hid_usages_to_mac_keycodes() {
            assert_eq!(virtual_keycode(0x04), Some(0x00)); // A
            assert_eq!(virtual_keycode(0x06), Some(0x08)); // C
            assert_eq!(virtual_keycode(0x19), Some(0x09)); // V
            assert_eq!(virtual_keycode(0x27), Some(0x1d)); // 0
            assert_eq!(virtual_keycode(0x45), Some(0x6f)); // F12
            assert_eq!(virtual_keycode(0x01), None);
        }
    }
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
mod platform {
    use super::{InputBackend, MouseButton, HID_LEFT_CONTROL};
    use std::collections::HashSet;

    pub(super) fn ensure_permission() -> Result<(), String> {
        Err("Remote control isn't supported on this platform.".to_owned())
    }

    pub(super) struct Platform;

    impl Platform {
        pub(super) fn new() -> Self {
            Self
        }
    }

    impl InputBackend for Platform {
        fn move_pointer(&mut self, _: u16, _: u16, _: Option<MouseButton>) {}
        fn button(&mut self, _: MouseButton, _: bool, _: i64) {}
        fn scroll(&mut self, _: i16, _: i16) {}
        fn key(&mut self, _: u16, _: bool, _: bool, _: &HashSet<u16>) -> bool {
            false
        }
        fn command_key(&self) -> u16 {
            HID_LEFT_CONTROL
        }
        fn clipboard_change_count(&mut self) -> u64 {
            0
        }
        fn clipboard_text(&mut self) -> Option<String> {
            None
        }
        fn set_clipboard_text(&mut self, _: &str) {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Records what the injector asks of the platform.
    #[derive(Default)]
    struct Recorder {
        log: Vec<String>,
        clipboard: String,
        change_count: u64,
    }

    impl InputBackend for Recorder {
        fn move_pointer(&mut self, x: u16, y: u16, dragging: Option<MouseButton>) {
            self.log.push(format!("move {x} {y} {dragging:?}"));
        }
        fn button(&mut self, button: MouseButton, pressed: bool, click_count: i64) {
            self.log
                .push(format!("{button:?} {pressed} x{click_count}"));
        }
        fn scroll(&mut self, x: i16, y: i16) {
            self.log.push(format!("scroll {x} {y}"));
        }
        fn key(&mut self, code: u16, pressed: bool, repeat: bool, _: &HashSet<u16>) -> bool {
            let state = if pressed { "down" } else { "up" };
            let repeat = if repeat { " repeat" } else { "" };
            self.log.push(format!("key {code:#x} {state}{repeat}"));
            true
        }
        fn command_key(&self) -> u16 {
            HID_LEFT_CONTROL
        }
        fn clipboard_change_count(&mut self) -> u64 {
            self.change_count
        }
        fn clipboard_text(&mut self) -> Option<String> {
            Some(self.clipboard.clone())
        }
        fn set_clipboard_text(&mut self, text: &str) {
            self.clipboard = text.to_owned();
            self.change_count += 1;
        }
    }

    fn key(code: u16, pressed: bool) -> RemoteEvent {
        RemoteEvent::Key { code, pressed }
    }

    #[test]
    fn releases_only_keys_and_buttons_it_pressed() {
        let mut injector = Injector::new(Recorder::default());
        injector.apply(key(HID_C, false));
        injector.apply(key(0x04, true));
        injector.apply(key(0x04, true));
        injector.apply(RemoteEvent::Button {
            button: MouseButton::Left,
            pressed: true,
        });
        injector.apply(RemoteEvent::PointerMove { x: 5, y: 6 });
        injector.release_all();
        injector.release_all();
        assert_eq!(
            injector.backend.log,
            [
                "key 0x4 down",
                "key 0x4 down repeat",
                "Left true x1",
                "move 5 6 Some(Left)",
                "key 0x4 up",
                "Left false x1",
            ]
        );
    }

    #[test]
    fn a_viewer_shortcut_uses_the_hosts_command_key_alone() {
        let mut injector = Injector::new(Recorder::default());
        // A Mac viewer holds Command (the Windows key here) and Shift.
        injector.apply(key(HID_LEFT_GUI, true));
        injector.apply(key(HID_LEFT_SHIFT, true));
        injector.backend.log.clear();
        injector.apply(RemoteEvent::Shortcut(Shortcut::Copy));
        let log = &injector.backend.log;
        assert_eq!(log[0], "key 0xe0 down", "Control goes down first");
        assert!(log[1..3].contains(&"key 0xe3 up".to_owned()));
        assert!(log[1..3].contains(&"key 0xe1 up".to_owned()));
        assert_eq!(log[3..], ["key 0x6 down", "key 0x6 up", "key 0xe0 up"]);

        // The viewer's later releases of those modifiers are dropped.
        injector.backend.log.clear();
        injector.apply(key(HID_LEFT_GUI, false));
        assert!(injector.backend.log.is_empty());
    }

    #[test]
    fn a_held_command_key_stays_down_through_a_shortcut() {
        let mut injector = Injector::new(Recorder::default());
        injector.apply(key(HID_LEFT_CONTROL, true));
        injector.backend.log.clear();
        injector.apply(RemoteEvent::Shortcut(Shortcut::Paste));
        assert_eq!(injector.backend.log, ["key 0x19 down", "key 0x19 up"]);
    }

    #[test]
    fn pasted_text_is_set_before_the_paste_shortcut() {
        let mut injector = Injector::new(Recorder::default());
        injector.apply(RemoteEvent::ClipboardText {
            first: true,
            last: true,
            chunk: b"from the viewer".to_vec(),
        });
        assert_eq!(injector.backend.clipboard, "from the viewer");
    }

    #[test]
    fn a_copy_sends_the_new_clipboard_back_once() {
        let mut injector = Injector::new(Recorder::default());
        injector.apply(RemoteEvent::Shortcut(Shortcut::Copy));
        assert_eq!(injector.poll_copy(), None);
        injector.backend.set_clipboard_text("copied on the host");
        assert_eq!(injector.poll_copy().as_deref(), Some("copied on the host"));
        assert_eq!(injector.poll_copy(), None);
    }

    #[test]
    fn quick_clicks_in_place_count_up() {
        let mut injector = Injector::new(Recorder::default());
        for _ in 0..3 {
            for pressed in [true, false] {
                injector.apply(RemoteEvent::Button {
                    button: MouseButton::Left,
                    pressed,
                });
            }
        }
        assert_eq!(
            injector.backend.log,
            [
                "Left true x1",
                "Left false x1",
                "Left true x2",
                "Left false x2",
                "Left true x3",
                "Left false x3",
            ]
        );
    }
}
