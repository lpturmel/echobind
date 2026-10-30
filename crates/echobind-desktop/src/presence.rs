//! Checks whether saved hosts are sharing, without connecting to them.
//!
//! A background thread sends each host a small probe every couple of seconds
//! over its own connected UDP socket. A sharing host answers; a host machine
//! with nothing on the port answers with an ICMP port-unreachable, which the
//! connected socket reports right away; anything else simply times out.

use echobind_core::protocol::{HostPresence, Packet, MAX_DATAGRAM_SIZE};
use std::{
    collections::HashMap,
    io,
    net::{SocketAddr, UdpSocket},
    sync::{Arc, Condvar, Mutex},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

const PROBE_INTERVAL: Duration = Duration::from_secs(2);
const POLL_INTERVAL: Duration = Duration::from_millis(50);
/// A host that never answered is offline after this long.
const FIRST_REPLY_TIMEOUT: Duration = Duration::from_millis(1500);
/// A host that answered before is offline after missing two probes.
const ONLINE_FOR: Duration = Duration::from_millis(5_000);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HostStatus {
    Checking,
    Online(HostPresence),
    Offline,
}

pub type Notifier = Arc<dyn Fn() + Send + Sync>;

struct State {
    running: bool,
    active: bool,
    targets: Vec<SocketAddr>,
    statuses: HashMap<SocketAddr, HostStatus>,
}

struct Shared {
    state: Mutex<State>,
    wake: Condvar,
}

pub struct Presence {
    shared: Arc<Shared>,
    handle: Option<JoinHandle<()>>,
}

impl Presence {
    /// Starts the probe thread, idle until [`Presence::watch`] gives it hosts.
    /// `notify` runs whenever a host's status changes.
    pub fn start(notify: Notifier) -> Self {
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                running: true,
                active: false,
                targets: Vec::new(),
                statuses: HashMap::new(),
            }),
            wake: Condvar::new(),
        });
        let handle = thread::Builder::new()
            .name("echobind-presence".to_owned())
            .spawn({
                let shared = shared.clone();
                move || run(&shared, &notify)
            })
            .expect("unable to start the presence thread");
        Self {
            shared,
            handle: Some(handle),
        }
    }

    /// Probes `targets` while `active`. Pausing forgets every status, so a
    /// host is checked afresh rather than shown with stale news.
    pub fn watch(&self, targets: impl Iterator<Item = SocketAddr>, active: bool) {
        let targets: Vec<_> = targets.collect();
        let mut state = self.shared.state.lock().unwrap();
        if state.active == active && state.targets == targets {
            return;
        }
        if !active {
            state.statuses.clear();
        }
        state.active = active;
        state.targets = targets;
        self.shared.wake.notify_all();
    }

    pub fn status(&self, address: SocketAddr) -> HostStatus {
        self.shared
            .state
            .lock()
            .unwrap()
            .statuses
            .get(&address)
            .copied()
            .unwrap_or(HostStatus::Checking)
    }
}

impl Drop for Presence {
    fn drop(&mut self) {
        self.shared.state.lock().unwrap().running = false;
        self.shared.wake.notify_all();
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// One host's probe socket and what it has heard back.
struct Probe {
    socket: Option<UdpSocket>,
    first_sent: Instant,
    last_reply: Option<(Instant, HostPresence)>,
    port_closed: bool,
}

impl Probe {
    fn open(address: SocketAddr) -> Self {
        Self {
            socket: open_socket(address).ok(),
            first_sent: Instant::now(),
            last_reply: None,
            port_closed: false,
        }
    }

    fn send(&mut self, buffer: &mut Vec<u8>) {
        let Some(socket) = &self.socket else {
            return;
        };
        Packet::Probe.encode(buffer);
        if let Err(error) = socket.send(buffer) {
            self.port_closed |= port_closed(&error);
        }
    }

    fn receive(&mut self, buffer: &mut [u8]) {
        let Some(socket) = &self.socket else {
            return;
        };
        loop {
            match socket.recv(buffer) {
                Ok(size) => {
                    if let Ok(Packet::ProbeReply(presence)) = Packet::try_from(&buffer[..size]) {
                        self.last_reply = Some((Instant::now(), presence));
                        self.port_closed = false;
                    }
                }
                Err(error) => {
                    self.port_closed |= port_closed(&error);
                    return;
                }
            }
        }
    }

    fn status(&self, now: Instant) -> HostStatus {
        status_at(
            now,
            self.socket.is_some(),
            self.first_sent,
            self.last_reply,
            self.port_closed,
        )
    }
}

fn status_at(
    now: Instant,
    has_socket: bool,
    first_sent: Instant,
    last_reply: Option<(Instant, HostPresence)>,
    port_closed: bool,
) -> HostStatus {
    if !has_socket || port_closed {
        return HostStatus::Offline;
    }
    match last_reply {
        Some((at, presence)) if now.saturating_duration_since(at) < ONLINE_FOR => {
            HostStatus::Online(presence)
        }
        Some(_) => HostStatus::Offline,
        None if now.saturating_duration_since(first_sent) < FIRST_REPLY_TIMEOUT => {
            HostStatus::Checking
        }
        None => HostStatus::Offline,
    }
}

fn run(shared: &Shared, notify: &Notifier) {
    let mut probes = HashMap::<SocketAddr, Probe>::new();
    let mut outgoing = Vec::with_capacity(16);
    let mut incoming = [0_u8; MAX_DATAGRAM_SIZE];
    let mut next_probe = Instant::now();
    loop {
        let targets = {
            let mut state = shared.state.lock().unwrap();
            while state.running && !(state.active && !state.targets.is_empty()) {
                probes.clear();
                state = shared.wake.wait(state).unwrap();
            }
            if !state.running {
                return;
            }
            state.targets.clone()
        };

        probes.retain(|address, _| targets.contains(address));
        let now = Instant::now();
        let mut fresh = false;
        for &address in &targets {
            probes.entry(address).or_insert_with(|| {
                fresh = true;
                Probe::open(address)
            });
        }
        if fresh || now >= next_probe {
            for probe in probes.values_mut() {
                probe.send(&mut outgoing);
            }
            next_probe = now + PROBE_INTERVAL;
        }
        for probe in probes.values_mut() {
            probe.receive(&mut incoming);
        }

        let now = Instant::now();
        let mut state = shared.state.lock().unwrap();
        let mut changed = false;
        for (&address, probe) in &probes {
            let status = probe.status(now);
            if state.statuses.insert(address, status) != Some(status) {
                changed = true;
            }
        }
        if changed {
            drop(state);
            notify();
            state = shared.state.lock().unwrap();
        }
        // Sleep between polls, but wake at once for new hosts or a pause.
        let _ = shared.wake.wait_timeout(state, POLL_INTERVAL).unwrap();
    }
}

fn open_socket(address: SocketAddr) -> io::Result<UdpSocket> {
    let bind = if address.is_ipv4() {
        "0.0.0.0:0"
    } else {
        "[::]:0"
    };
    let socket = UdpSocket::bind(bind)?;
    socket.connect(address)?;
    socket.set_nonblocking(true)?;
    Ok(socket)
}

/// A connected UDP socket reports an ICMP port-unreachable reply as a
/// refused (BSD) or reset (Windows) connection.
fn port_closed(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::ConnectionRefused | io::ErrorKind::ConnectionReset
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHARING: HostPresence = HostPresence {
        video: true,
        busy: false,
    };

    #[test]
    fn a_host_is_checked_until_its_first_reply_is_overdue() {
        let sent = Instant::now();
        assert_eq!(
            status_at(sent, true, sent, None, false),
            HostStatus::Checking
        );
        assert_eq!(
            status_at(sent + FIRST_REPLY_TIMEOUT, true, sent, None, false),
            HostStatus::Offline
        );
    }

    #[test]
    fn a_host_stays_online_until_it_misses_two_probes() {
        let sent = Instant::now();
        let reply = Some((sent, SHARING));
        assert_eq!(
            status_at(sent + PROBE_INTERVAL, true, sent, reply, false),
            HostStatus::Online(SHARING)
        );
        assert_eq!(
            status_at(sent + ONLINE_FOR, true, sent, reply, false),
            HostStatus::Offline
        );
    }

    #[test]
    fn a_closed_port_or_missing_socket_is_offline_at_once() {
        let sent = Instant::now();
        assert_eq!(status_at(sent, true, sent, None, true), HostStatus::Offline);
        assert_eq!(
            status_at(sent, false, sent, None, false),
            HostStatus::Offline
        );
    }

    fn wait_for(presence: &Presence, address: SocketAddr, wanted: HostStatus) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while presence.status(address) != wanted {
            assert!(
                Instant::now() < deadline,
                "{address} stayed {:?}",
                presence.status(address)
            );
            thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn probes_tell_a_sharing_host_from_a_closed_port() {
        let host = UdpSocket::bind("127.0.0.1:0").unwrap();
        host.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let sharing = host.local_addr().unwrap();
        let closed = UdpSocket::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap();
        let busy = HostPresence {
            video: false,
            busy: true,
        };
        let responder = thread::spawn(move || {
            let mut buffer = [0_u8; MAX_DATAGRAM_SIZE];
            let (size, viewer) = host.recv_from(&mut buffer).unwrap();
            assert_eq!(Packet::try_from(&buffer[..size]), Ok(Packet::Probe));
            let mut reply = Vec::new();
            Packet::ProbeReply(busy).encode(&mut reply);
            host.send_to(&reply, viewer).unwrap();
        });

        let presence = Presence::start(Arc::new(|| {}));
        presence.watch([sharing, closed].into_iter(), true);
        wait_for(&presence, sharing, HostStatus::Online(busy));
        wait_for(&presence, closed, HostStatus::Offline);
        responder.join().unwrap();

        presence.watch([sharing, closed].into_iter(), false);
        assert_eq!(presence.status(sharing), HostStatus::Checking);
    }
}
