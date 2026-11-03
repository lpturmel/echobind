//! Remote control: the keyboard, mouse, and clipboard events a viewer sends
//! to drive the host, and the small reliable stream that carries them.
//!
//! UDP may drop or reorder any datagram, but a lost key or button release
//! would leave it stuck down on the host. Events are therefore numbered and
//! retransmitted until the peer acknowledges them, and the peer applies them
//! strictly in order. Pointer motion rides the same stream: the viewer sends
//! at most one position per UI frame, so it never builds a backlog.

use crate::protocol::{PacketParseError, MAX_EVENTS_PAYLOAD};
use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};

/// Clipboard text larger than this is not sent.
pub const MAX_CLIPBOARD_TEXT: usize = 256 * 1024;
const CLIPBOARD_CHUNK: usize = 1024;
/// How long the oldest unacknowledged event waits before it is sent again.
pub const RETRANSMIT_INTERVAL: Duration = Duration::from_millis(40);
/// Bytes that may be in flight before an acknowledgement; about eight
/// standard datagrams, so a large paste needs only a few round trips.
const WINDOW_BYTES: usize = 8 * MAX_EVENTS_PAYLOAD;
/// A peer that stops acknowledging cannot grow the queue without bound.
const MAX_QUEUED_BYTES: usize = MAX_CLIPBOARD_TEXT + 64 * 1024;

const POINTER_MOVE: u8 = 1;
const BUTTON: u8 = 2;
const SCROLL: u8 = 3;
const KEY: u8 = 4;
const SHORTCUT: u8 = 5;
const CLIPBOARD_TEXT: u8 = 6;
const RELEASE_ALL: u8 = 7;
const CLIPBOARD_FIRST: u8 = 1;
const CLIPBOARD_LAST: u8 = 1 << 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MouseButton {
    Left,
    Right,
    Middle,
    Back,
    Forward,
}

impl MouseButton {
    const ALL: [Self; 5] = [
        Self::Left,
        Self::Right,
        Self::Middle,
        Self::Back,
        Self::Forward,
    ];
}

/// A clipboard command, which each host performs with its own shortcut, so a
/// Mac viewer's Command-C copies on a Windows host.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Shortcut {
    Copy,
    Cut,
    Paste,
}

impl Shortcut {
    const ALL: [Self; 3] = [Self::Copy, Self::Cut, Self::Paste];
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RemoteEvent {
    /// An absolute position on the shared display, where 0 is the left or top
    /// edge and `u16::MAX` the right or bottom edge.
    PointerMove {
        x: u16,
        y: u16,
    },
    Button {
        button: MouseButton,
        pressed: bool,
    },
    /// Scrolling in 1/120ths of a line, in egui's direction: positive values
    /// move the content right and down, as the wheel turning towards you
    /// scrolls up.
    Scroll {
        x: i16,
        y: i16,
    },
    /// A physical key, as a USB HID keyboard usage, so the host's own layout
    /// decides what it types.
    Key {
        code: u16,
        pressed: bool,
    },
    Shortcut(Shortcut),
    /// One piece of UTF-8 clipboard text. The host sets its clipboard once the
    /// last piece arrives.
    ClipboardText {
        first: bool,
        last: bool,
        chunk: Vec<u8>,
    },
    /// Releases every key and button the peer holds down on this viewer's
    /// behalf, as when the viewer loses focus.
    ReleaseAll,
}

impl RemoteEvent {
    pub fn encode(&self, out: &mut Vec<u8>) {
        match self {
            Self::PointerMove { x, y } => {
                out.push(POINTER_MOVE);
                out.extend_from_slice(&x.to_be_bytes());
                out.extend_from_slice(&y.to_be_bytes());
            }
            Self::Button { button, pressed } => {
                let index = MouseButton::ALL
                    .iter()
                    .position(|known| known == button)
                    .expect("every button is listed");
                out.extend_from_slice(&[BUTTON, index as u8, u8::from(*pressed)]);
            }
            Self::Scroll { x, y } => {
                out.push(SCROLL);
                out.extend_from_slice(&x.to_be_bytes());
                out.extend_from_slice(&y.to_be_bytes());
            }
            Self::Key { code, pressed } => {
                out.push(KEY);
                out.extend_from_slice(&code.to_be_bytes());
                out.push(u8::from(*pressed));
            }
            Self::Shortcut(shortcut) => {
                let index = Shortcut::ALL
                    .iter()
                    .position(|known| known == shortcut)
                    .expect("every shortcut is listed");
                out.extend_from_slice(&[SHORTCUT, index as u8]);
            }
            Self::ClipboardText { first, last, chunk } => {
                let flags =
                    (u8::from(*first) * CLIPBOARD_FIRST) | (u8::from(*last) * CLIPBOARD_LAST);
                out.extend_from_slice(&[CLIPBOARD_TEXT, flags]);
                out.extend_from_slice(&(chunk.len() as u16).to_be_bytes());
                out.extend_from_slice(chunk);
            }
            Self::ReleaseAll => out.push(RELEASE_ALL),
        }
    }

    /// Decodes the first event in `bytes` and returns it with its length.
    pub fn decode(bytes: &[u8]) -> Result<(Self, usize), PacketParseError> {
        let field =
            |range: std::ops::Range<usize>| bytes.get(range).ok_or(PacketParseError::TooShort);
        let u16_at = |offset: usize| {
            field(offset..offset + 2).map(|pair| u16::from_be_bytes([pair[0], pair[1]]))
        };
        let flag_at = |offset: usize| match field(offset..offset + 1)?[0] {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(PacketParseError::Invalid),
        };
        let tag = *bytes.first().ok_or(PacketParseError::TooShort)?;
        match tag {
            POINTER_MOVE => Ok((
                Self::PointerMove {
                    x: u16_at(1)?,
                    y: u16_at(3)?,
                },
                5,
            )),
            BUTTON => {
                let button = *MouseButton::ALL
                    .get(usize::from(field(1..2)?[0]))
                    .ok_or(PacketParseError::Invalid)?;
                Ok((
                    Self::Button {
                        button,
                        pressed: flag_at(2)?,
                    },
                    3,
                ))
            }
            SCROLL => Ok((
                Self::Scroll {
                    x: u16_at(1)? as i16,
                    y: u16_at(3)? as i16,
                },
                5,
            )),
            KEY => Ok((
                Self::Key {
                    code: u16_at(1)?,
                    pressed: flag_at(3)?,
                },
                4,
            )),
            SHORTCUT => {
                let shortcut = *Shortcut::ALL
                    .get(usize::from(field(1..2)?[0]))
                    .ok_or(PacketParseError::Invalid)?;
                Ok((Self::Shortcut(shortcut), 2))
            }
            CLIPBOARD_TEXT => {
                let flags = field(1..2)?[0];
                if flags & !(CLIPBOARD_FIRST | CLIPBOARD_LAST) != 0 {
                    return Err(PacketParseError::Invalid);
                }
                let length = usize::from(u16_at(2)?);
                if length > CLIPBOARD_CHUNK {
                    return Err(PacketParseError::TooLarge);
                }
                Ok((
                    Self::ClipboardText {
                        first: flags & CLIPBOARD_FIRST != 0,
                        last: flags & CLIPBOARD_LAST != 0,
                        chunk: field(4..4 + length)?.to_vec(),
                    },
                    4 + length,
                ))
            }
            RELEASE_ALL => Ok((Self::ReleaseAll, 1)),
            _ => Err(PacketParseError::Invalid),
        }
    }
}

/// The viewer's (or host's) side of the stream: numbers events, keeps them
/// until the peer acknowledges them, and resends what goes missing.
pub struct EventSender {
    queue: VecDeque<(u64, Vec<u8>)>,
    queued_bytes: usize,
    next_sequence: u64,
    /// The highest sequence sent at least once.
    sent_through: u64,
    /// When the oldest queued event last made progress: it was queued, the
    /// peer acknowledged something, or it was resent.
    progress_at: Instant,
    payload: Vec<u8>,
}

impl Default for EventSender {
    fn default() -> Self {
        Self::new()
    }
}

impl EventSender {
    pub fn new() -> Self {
        Self {
            queue: VecDeque::new(),
            queued_bytes: 0,
            next_sequence: 1,
            sent_through: 0,
            progress_at: Instant::now(),
            payload: Vec::with_capacity(MAX_EVENTS_PAYLOAD),
        }
    }

    /// Queues an event. A queue the peer stopped draining is replaced by a
    /// single release-all, so nothing stays held down; this returns false.
    pub fn push(&mut self, event: &RemoteEvent) -> bool {
        let mut encoded = Vec::with_capacity(8);
        event.encode(&mut encoded);
        if self.queued_bytes + encoded.len() > MAX_QUEUED_BYTES {
            self.discard();
            return false;
        }
        if self.queue.is_empty() {
            self.progress_at = Instant::now();
        }
        self.queued_bytes += encoded.len();
        self.queue.push_back((self.next_sequence, encoded));
        self.next_sequence += 1;
        true
    }

    /// Queues clipboard text in pieces. Returns false, queuing nothing, when
    /// it exceeds [`MAX_CLIPBOARD_TEXT`].
    pub fn push_clipboard(&mut self, text: &str) -> bool {
        let bytes = text.as_bytes();
        if bytes.len() > MAX_CLIPBOARD_TEXT {
            return false;
        }
        let chunks = bytes.len().div_ceil(CLIPBOARD_CHUNK).max(1);
        for index in 0..chunks {
            let start = (index * CLIPBOARD_CHUNK).min(bytes.len());
            let end = (start + CLIPBOARD_CHUNK).min(bytes.len());
            if !self.push(&RemoteEvent::ClipboardText {
                first: index == 0,
                last: index + 1 == chunks,
                chunk: bytes[start..end].to_vec(),
            }) {
                return false;
            }
        }
        true
    }

    /// Drops everything still queued and asks the peer to let go of whatever
    /// it holds down, as after a reconnect.
    pub fn discard(&mut self) {
        self.queue.clear();
        self.queued_bytes = 0;
        self.sent_through = self.next_sequence - 1;
        self.push(&RemoteEvent::ReleaseAll);
    }

    pub fn is_idle(&self) -> bool {
        self.queue.is_empty()
    }

    /// Forgets every event up to and including `sequence`.
    pub fn acknowledge(&mut self, sequence: u64) {
        let mut advanced = false;
        while self
            .queue
            .front()
            .is_some_and(|(queued, _)| *queued <= sequence)
        {
            let (_, encoded) = self.queue.pop_front().expect("the front was just checked");
            self.queued_bytes -= encoded.len();
            advanced = true;
        }
        if advanced {
            self.progress_at = Instant::now();
        }
    }

    /// Sends the queued events not sent before, as far as the window allows.
    pub fn send_new(&mut self, out: &mut Vec<u8>, send: impl FnMut(&[u8])) {
        let from = self.sent_through + 1;
        self.send_window(from, out, send);
    }

    /// Whether the oldest event has waited too long for an acknowledgement.
    pub fn retransmit_due(&self, now: Instant) -> bool {
        !self.queue.is_empty()
            && now.saturating_duration_since(self.progress_at) >= RETRANSMIT_INTERVAL
    }

    /// Sends the whole window again, starting with the oldest event.
    pub fn retransmit(&mut self, out: &mut Vec<u8>, send: impl FnMut(&[u8])) {
        let Some(&(oldest, _)) = self.queue.front() else {
            return;
        };
        self.progress_at = Instant::now();
        self.send_window(oldest, out, send);
    }

    fn send_window(&mut self, from: u64, out: &mut Vec<u8>, mut send: impl FnMut(&[u8])) {
        let Some(&(base, _)) = self.queue.front() else {
            return;
        };
        let mut window = 0_usize;
        let mut first = None::<u64>;
        self.payload.clear();
        for (sequence, encoded) in &self.queue {
            window += encoded.len();
            if window > WINDOW_BYTES && *sequence != base {
                break;
            }
            if *sequence < from {
                continue;
            }
            if self.payload.len() + encoded.len() > MAX_EVENTS_PAYLOAD {
                if let Some(first) = first.take() {
                    crate::protocol::Packet::Events {
                        base,
                        first,
                        payload: &self.payload,
                    }
                    .encode(out);
                    send(out);
                }
                self.payload.clear();
            }
            first.get_or_insert(*sequence);
            self.payload.extend_from_slice(encoded);
            self.sent_through = self.sent_through.max(*sequence);
        }
        if let Some(first) = first {
            crate::protocol::Packet::Events {
                base,
                first,
                payload: &self.payload,
            }
            .encode(out);
            send(out);
        }
    }
}

/// The receiving side: applies each event once, in order, and reports the
/// sequence to acknowledge.
#[derive(Default)]
pub struct EventReceiver {
    applied: Option<u64>,
}

impl EventReceiver {
    pub fn new() -> Self {
        Self::default()
    }

    /// Forgets the stream, for a new peer.
    pub fn reset(&mut self) {
        self.applied = None;
    }

    /// Applies the events in one packet that follow the last applied one and
    /// returns the sequence to acknowledge. A packet after a gap applies
    /// nothing; the sender resends the missing events first.
    pub fn receive(
        &mut self,
        base: u64,
        first: u64,
        payload: &[u8],
        mut apply: impl FnMut(RemoteEvent),
    ) -> u64 {
        // The sender only moves its base past events this side acknowledged,
        // or past events it discarded on purpose; either way they are done.
        let mut applied = self.applied.unwrap_or(0).max(base.saturating_sub(1));
        let mut sequence = first;
        let mut rest = payload;
        while !rest.is_empty() && sequence <= applied + 1 {
            let Ok((event, length)) = RemoteEvent::decode(rest) else {
                break;
            };
            rest = &rest[length..];
            if sequence == applied + 1 {
                apply(event);
                applied = sequence;
            }
            sequence += 1;
        }
        self.applied = Some(applied);
        applied
    }
}

/// Joins [`RemoteEvent::ClipboardText`] pieces back into text.
#[derive(Default)]
pub struct ClipboardAssembler {
    text: Option<Vec<u8>>,
}

impl ClipboardAssembler {
    pub fn push(&mut self, first: bool, last: bool, chunk: &[u8]) -> Option<String> {
        if first {
            self.text = Some(Vec::new());
        }
        let text = self.text.as_mut()?;
        text.extend_from_slice(chunk);
        if text.len() > MAX_CLIPBOARD_TEXT {
            self.text = None;
            return None;
        }
        if !last {
            return None;
        }
        String::from_utf8(self.text.take()?).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::Packet;

    fn sample_events() -> Vec<RemoteEvent> {
        vec![
            RemoteEvent::PointerMove { x: 0, y: u16::MAX },
            RemoteEvent::Button {
                button: MouseButton::Forward,
                pressed: true,
            },
            RemoteEvent::Scroll { x: -120, y: 360 },
            RemoteEvent::Key {
                code: 0xe3,
                pressed: false,
            },
            RemoteEvent::Shortcut(Shortcut::Paste),
            RemoteEvent::ClipboardText {
                first: true,
                last: false,
                chunk: "héllo".as_bytes().to_vec(),
            },
            RemoteEvent::ReleaseAll,
        ]
    }

    /// Collects every datagram the sender emits, decoded.
    fn sent(sender: &mut EventSender, retransmit: bool) -> Vec<(u64, u64, Vec<u8>)> {
        let mut out = Vec::new();
        let mut packets = Vec::new();
        let mut collect = |datagram: &[u8]| {
            let Ok(Packet::Events {
                base,
                first,
                payload,
            }) = Packet::try_from(datagram)
            else {
                panic!("the sender emitted something other than events");
            };
            packets.push((base, first, payload.to_vec()));
        };
        if retransmit {
            sender.retransmit(&mut out, &mut collect);
        } else {
            sender.send_new(&mut out, &mut collect);
        }
        packets
    }

    #[test]
    fn every_event_round_trips() {
        for event in sample_events() {
            let mut encoded = Vec::new();
            event.encode(&mut encoded);
            assert_eq!(
                RemoteEvent::decode(&encoded),
                Ok((event.clone(), encoded.len()))
            );
            assert!(RemoteEvent::decode(&encoded[..encoded.len() - 1]).is_err());
        }
        assert_eq!(
            RemoteEvent::decode(&[BUTTON, 9, 1]),
            Err(PacketParseError::Invalid)
        );
        assert_eq!(
            RemoteEvent::decode(&[KEY, 0, 4, 2]),
            Err(PacketParseError::Invalid)
        );
        assert_eq!(RemoteEvent::decode(&[0xff]), Err(PacketParseError::Invalid));
    }

    #[test]
    fn events_are_applied_once_and_in_order() {
        let mut sender = EventSender::new();
        let mut receiver = EventReceiver::new();
        let events = sample_events();
        for event in &events {
            assert!(sender.push(event));
        }
        let packets = sent(&mut sender, false);
        assert_eq!(packets.len(), 1);

        let mut applied = Vec::new();
        let (base, first, payload) = &packets[0];
        let ack = receiver.receive(*base, *first, payload, |event| applied.push(event));
        assert_eq!(applied, events);
        assert_eq!(ack, events.len() as u64);

        // A duplicate changes nothing.
        let again = receiver.receive(*base, *first, payload, |event| applied.push(event));
        assert_eq!(again, ack);
        assert_eq!(applied.len(), events.len());

        sender.acknowledge(ack);
        assert!(sender.is_idle());
    }

    #[test]
    fn a_lost_packet_is_resent_before_later_events_apply() {
        let mut sender = EventSender::new();
        let mut receiver = EventReceiver::new();
        sender.push(&RemoteEvent::Key {
            code: 4,
            pressed: true,
        });
        let lost = sent(&mut sender, false);
        assert_eq!(lost.len(), 1);
        sender.push(&RemoteEvent::Key {
            code: 4,
            pressed: false,
        });
        let later = sent(&mut sender, false);
        let (base, first, payload) = &later[0];
        assert_eq!((*base, *first), (1, 2));

        let mut applied = Vec::new();
        let ack = receiver.receive(*base, *first, payload, |event| applied.push(event));
        assert_eq!(ack, 0, "the key release must wait for its press");
        assert!(applied.is_empty());

        for (base, first, payload) in sent(&mut sender, true) {
            receiver.receive(base, first, &payload, |event| applied.push(event));
        }
        assert_eq!(
            applied,
            vec![
                RemoteEvent::Key {
                    code: 4,
                    pressed: true
                },
                RemoteEvent::Key {
                    code: 4,
                    pressed: false
                },
            ]
        );
    }

    #[test]
    fn retransmits_only_after_the_interval_without_progress() {
        let mut sender = EventSender::new();
        assert!(!sender.retransmit_due(Instant::now() + RETRANSMIT_INTERVAL));
        sender.push(&RemoteEvent::ReleaseAll);
        assert!(!sender.retransmit_due(Instant::now()));
        assert!(sender.retransmit_due(Instant::now() + RETRANSMIT_INTERVAL));
    }

    #[test]
    fn large_clipboard_text_spans_packets_and_reassembles() {
        let text: String = "0123456789€".repeat(2_000);
        let mut sender = EventSender::new();
        let mut receiver = EventReceiver::new();
        let mut assembler = ClipboardAssembler::default();
        assert!(sender.push_clipboard(&text));

        let mut received = None;
        for _ in 0..100 {
            let mut packets = sent(&mut sender, false);
            if packets.is_empty() {
                packets = sent(&mut sender, true);
            }
            if packets.is_empty() {
                break;
            }
            for (base, first, payload) in packets {
                assert!(payload.len() <= MAX_EVENTS_PAYLOAD);
                let ack = receiver.receive(base, first, &payload, |event| {
                    if let RemoteEvent::ClipboardText { first, last, chunk } = event {
                        if let Some(text) = assembler.push(first, last, &chunk) {
                            received = Some(text);
                        }
                    }
                });
                sender.acknowledge(ack);
            }
        }
        assert_eq!(received.as_deref(), Some(text.as_str()));
        assert!(sender.is_idle());

        assert!(!sender.push_clipboard(&"x".repeat(MAX_CLIPBOARD_TEXT + 1)));
        assert!(sender.is_idle());
    }

    #[test]
    fn a_receiver_joins_a_stream_already_in_progress() {
        let mut sender = EventSender::new();
        for _ in 0..3 {
            sender.push(&RemoteEvent::ReleaseAll);
        }
        sender.acknowledge(3);
        sender.push(&RemoteEvent::Shortcut(Shortcut::Copy));
        let packets = sent(&mut sender, false);
        let (base, first, payload) = &packets[0];

        let mut receiver = EventReceiver::new();
        let mut applied = Vec::new();
        let ack = receiver.receive(*base, *first, payload, |event| applied.push(event));
        assert_eq!(ack, 4);
        assert_eq!(applied, vec![RemoteEvent::Shortcut(Shortcut::Copy)]);
    }

    #[test]
    fn discarding_skips_the_receiver_past_the_dropped_events() {
        let mut sender = EventSender::new();
        let mut receiver = EventReceiver::new();
        sender.push(&RemoteEvent::Key {
            code: 4,
            pressed: true,
        });
        let first_packets = sent(&mut sender, false);
        for (base, first, payload) in &first_packets {
            receiver.receive(*base, *first, payload, |_| {});
        }
        sender.push(&RemoteEvent::Key {
            code: 5,
            pressed: true,
        });
        sender.discard();

        let mut applied = Vec::new();
        for (base, first, payload) in sent(&mut sender, false) {
            receiver.receive(base, first, &payload, |event| applied.push(event));
        }
        assert_eq!(applied, vec![RemoteEvent::ReleaseAll]);
    }

    #[test]
    fn assembler_ignores_pieces_without_a_start() {
        let mut assembler = ClipboardAssembler::default();
        assert_eq!(assembler.push(false, true, b"orphan"), None);
        assert_eq!(assembler.push(true, false, b"hel"), None);
        assert_eq!(assembler.push(false, true, b"lo"), Some("hello".to_owned()));
    }
}
