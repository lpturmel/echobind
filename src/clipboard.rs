use crate::{
    error::{Error, Result},
    protocol::{ClipboardChunk, Packet, MAX_CLIPBOARD_CHUNK_PAYLOAD},
};
use std::{
    collections::HashMap,
    net::{SocketAddr, UdpSocket},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex,
    },
    thread,
    time::{Duration, Instant},
};
use tracing::{error, warn};

const CLIPBOARD_POLL_INTERVAL: Duration = Duration::from_millis(250);
const CLIPBOARD_TRANSFER_TTL: Duration = Duration::from_secs(10);

pub trait ClipboardBackend: Send + 'static {
    fn get_text(&mut self) -> Result<Option<String>>;
    fn set_text(&mut self, text: &str) -> Result<()>;
}

pub trait ClipboardBehavior: Clone + Send + 'static {
    fn spawn_polling(&self, running: Arc<AtomicBool>) -> thread::JoinHandle<()>;
    fn receive_chunk(&self, chunk: ClipboardChunk<'_>) -> Result<()>;
}

pub struct Clipboard<B: ClipboardBackend> {
    backend: Arc<Mutex<B>>,
    sender: ClipboardSender,
    reassembler: Arc<Mutex<ClipboardReassembler>>,
    last_text: Arc<Mutex<Option<String>>>,
}

impl<B: ClipboardBackend> Clone for Clipboard<B> {
    fn clone(&self) -> Self {
        Self {
            backend: self.backend.clone(),
            sender: self.sender.clone(),
            reassembler: self.reassembler.clone(),
            last_text: self.last_text.clone(),
        }
    }
}

impl<B: ClipboardBackend> Clipboard<B> {
    pub fn new(backend: B, sender: ClipboardSender) -> Self {
        Self {
            backend: Arc::new(Mutex::new(backend)),
            sender,
            reassembler: Arc::new(Mutex::new(ClipboardReassembler::new(
                CLIPBOARD_TRANSFER_TTL,
            ))),
            last_text: Arc::new(Mutex::new(None)),
        }
    }
}

impl<B: ClipboardBackend> ClipboardBehavior for Clipboard<B> {
    fn spawn_polling(&self, running: Arc<AtomicBool>) -> thread::JoinHandle<()> {
        let backend = self.backend.clone();
        let sender = self.sender.clone();
        let last_text = self.last_text.clone();

        thread::spawn(move || {
            while running.load(Ordering::Relaxed) {
                thread::sleep(CLIPBOARD_POLL_INTERVAL);

                let text = match backend.lock().unwrap().get_text() {
                    Ok(Some(text)) => text,
                    Ok(None) => continue,
                    Err(err) => {
                        warn!("Failed to read clipboard: {err}");
                        continue;
                    }
                };

                let mut last = last_text.lock().unwrap();
                if last.as_deref() == Some(text.as_str()) {
                    continue;
                }

                if let Err(err) = sender.send_text(&text) {
                    error!("Failed to send clipboard change: {err}");
                    continue;
                }
                *last = Some(text);
            }
        })
    }

    fn receive_chunk(&self, chunk: ClipboardChunk<'_>) -> Result<()> {
        let Some(text) = self.reassembler.lock().unwrap().push(chunk)? else {
            return Ok(());
        };

        self.backend.lock().unwrap().set_text(&text)?;
        *self.last_text.lock().unwrap() = Some(text);
        Ok(())
    }
}

#[derive(Clone)]
pub struct ClipboardSender {
    socket: Arc<UdpSocket>,
    peer: Option<SocketAddr>,
    next_transfer_id: Arc<AtomicU64>,
}

impl ClipboardSender {
    pub fn connected(socket: Arc<UdpSocket>) -> Self {
        Self::new(socket, None)
    }

    pub fn to_peer(socket: Arc<UdpSocket>, peer: SocketAddr) -> Self {
        Self::new(socket, Some(peer))
    }

    pub fn send_text(&self, text: &str) -> Result<()> {
        let bytes = text.as_bytes();
        let chunk_count = if bytes.is_empty() {
            1
        } else {
            bytes.len().div_ceil(MAX_CLIPBOARD_CHUNK_PAYLOAD)
        };

        if chunk_count > u16::MAX as usize {
            return Err(Error::Clipboard(format!(
                "clipboard payload needs {chunk_count} chunks, max supported is {}",
                u16::MAX
            )));
        }

        let transfer_id = self.next_transfer_id.fetch_add(1, Ordering::Relaxed);
        let total = chunk_count as u16;
        let mut packet = Vec::with_capacity(MAX_CLIPBOARD_CHUNK_PAYLOAD + 17);

        for index in 0..chunk_count {
            let start = index * MAX_CLIPBOARD_CHUNK_PAYLOAD;
            let end = (start + MAX_CLIPBOARD_CHUNK_PAYLOAD).min(bytes.len());
            let payload = if start < bytes.len() {
                &bytes[start..end]
            } else {
                &[]
            };

            Packet::Clipboard(ClipboardChunk {
                transfer_id,
                index: index as u16,
                total,
                payload,
            })
            .encode(&mut packet);

            match self.peer {
                Some(peer) => {
                    self.socket.send_to(&packet, peer)?;
                }
                None => {
                    self.socket.send(&packet)?;
                }
            }
        }

        Ok(())
    }

    fn new(socket: Arc<UdpSocket>, peer: Option<SocketAddr>) -> Self {
        Self {
            socket,
            peer,
            next_transfer_id: Arc::new(AtomicU64::new(1)),
        }
    }
}

struct ClipboardReassembler {
    transfers: HashMap<u64, PendingClipboardTransfer>,
    max_age: Duration,
}

impl ClipboardReassembler {
    fn new(max_age: Duration) -> Self {
        Self {
            transfers: HashMap::new(),
            max_age,
        }
    }

    fn push(&mut self, chunk: ClipboardChunk<'_>) -> Result<Option<String>> {
        self.drop_expired();

        let transfer = self
            .transfers
            .entry(chunk.transfer_id)
            .or_insert_with(|| PendingClipboardTransfer::new(chunk.total));

        if transfer.total != chunk.total {
            self.transfers.remove(&chunk.transfer_id);
            return Err(Error::Clipboard(format!(
                "clipboard transfer {} changed chunk count",
                chunk.transfer_id
            )));
        }

        transfer.push(chunk);
        if !transfer.is_complete() {
            return Ok(None);
        }

        let transfer = self.transfers.remove(&chunk.transfer_id).unwrap();
        let bytes = transfer.into_bytes();
        String::from_utf8(bytes)
            .map(Some)
            .map_err(|err| Error::Clipboard(format!("clipboard payload is not UTF-8: {err}")))
    }

    fn drop_expired(&mut self) {
        let max_age = self.max_age;
        self.transfers
            .retain(|_, transfer| transfer.last_updated.elapsed() <= max_age);
    }
}

struct PendingClipboardTransfer {
    total: u16,
    chunks: Vec<Option<Vec<u8>>>,
    received: usize,
    last_updated: Instant,
}

impl PendingClipboardTransfer {
    fn new(total: u16) -> Self {
        Self {
            total,
            chunks: vec![None; total as usize],
            received: 0,
            last_updated: Instant::now(),
        }
    }

    fn push(&mut self, chunk: ClipboardChunk<'_>) {
        self.last_updated = Instant::now();
        let slot = &mut self.chunks[chunk.index as usize];
        if slot.is_none() {
            *slot = Some(chunk.payload.to_vec());
            self.received += 1;
        }
    }

    fn is_complete(&self) -> bool {
        self.received == self.total as usize
    }

    fn into_bytes(self) -> Vec<u8> {
        let size = self
            .chunks
            .iter()
            .filter_map(|chunk| chunk.as_ref())
            .map(Vec::len)
            .sum();
        let mut bytes = Vec::with_capacity(size);
        for chunk in self.chunks.into_iter().flatten() {
            bytes.extend_from_slice(&chunk);
        }
        bytes
    }
}

#[cfg(target_os = "macos")]
mod platform {
    use super::{ClipboardBackend, Result};
    use objc2_app_kit::{NSPasteboard, NSPasteboardTypeString};
    use objc2_foundation::NSString;

    pub struct PlatformClipboard;

    impl PlatformClipboard {
        pub fn new() -> Self {
            Self
        }
    }

    impl ClipboardBackend for PlatformClipboard {
        fn get_text(&mut self) -> Result<Option<String>> {
            let pasteboard = NSPasteboard::generalPasteboard();
            Ok(pasteboard
                .stringForType(unsafe { NSPasteboardTypeString })
                .map(|value| value.to_string()))
        }

        fn set_text(&mut self, text: &str) -> Result<()> {
            let pasteboard = NSPasteboard::generalPasteboard();
            let value = NSString::from_str(text);
            pasteboard.clearContents();
            pasteboard.setString_forType(&value, unsafe { NSPasteboardTypeString });
            Ok(())
        }
    }
}

#[cfg(target_os = "windows")]
mod platform {
    use super::{ClipboardBackend, Error, Result};

    pub struct PlatformClipboard;

    impl PlatformClipboard {
        pub fn new() -> Self {
            Self
        }
    }

    impl ClipboardBackend for PlatformClipboard {
        fn get_text(&mut self) -> Result<Option<String>> {
            use clipboard_win::formats::Format;

            if !clipboard_win::Unicode.is_format_avail() {
                return Ok(None);
            }

            match clipboard_win::get_clipboard_string() {
                Ok(text) => Ok(Some(text)),
                Err(err) => Err(Error::Clipboard(format!(
                    "failed to read Windows clipboard: {err}"
                ))),
            }
        }

        fn set_text(&mut self, text: &str) -> Result<()> {
            clipboard_win::set_clipboard_string(text).map_err(|err| {
                Error::Clipboard(format!("failed to write Windows clipboard: {err}"))
            })
        }
    }
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
mod platform {
    use super::{ClipboardBackend, Result};

    pub struct PlatformClipboard;

    impl PlatformClipboard {
        pub fn new() -> Self {
            Self
        }
    }

    impl ClipboardBackend for PlatformClipboard {
        fn get_text(&mut self) -> Result<Option<String>> {
            Ok(None)
        }

        fn set_text(&mut self, _text: &str) -> Result<()> {
            Ok(())
        }
    }
}

pub type SystemClipboard = Clipboard<platform::PlatformClipboard>;

pub fn connected_clipboard(socket: Arc<UdpSocket>) -> SystemClipboard {
    Clipboard::new(
        platform::PlatformClipboard::new(),
        ClipboardSender::connected(socket),
    )
}

pub fn peer_clipboard(socket: Arc<UdpSocket>, peer: SocketAddr) -> SystemClipboard {
    Clipboard::new(
        platform::PlatformClipboard::new(),
        ClipboardSender::to_peer(socket, peer),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reassembles_out_of_order_clipboard_chunks() {
        let mut reassembler = ClipboardReassembler::new(Duration::from_secs(1));
        let last = ClipboardChunk {
            transfer_id: 1,
            index: 1,
            total: 2,
            payload: b" world",
        };
        let first = ClipboardChunk {
            transfer_id: 1,
            index: 0,
            total: 2,
            payload: b"hello",
        };

        assert_eq!(reassembler.push(last).unwrap(), None);
        assert_eq!(
            reassembler.push(first).unwrap(),
            Some("hello world".to_owned())
        );
    }
}
