use crate::{
    cli::ConnectCmd,
    clipboard::{self, ClipboardBehavior},
    commands::record::DISCONNECT_TIMEOUT,
    config::{count_to_channels, Config},
    error::{Error, Result},
    protocol::Packet,
};
use cpal::{
    traits::{DeviceTrait, HostTrait, StreamTrait},
    BufferSize, SampleRate, Stream, StreamConfig,
};
use std::{
    collections::VecDeque,
    net::{SocketAddr, UdpSocket},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc, Mutex,
    },
    thread,
    time::{Duration, Instant},
};
use tracing::{error, info, warn};

const MAX_BUFFER_MS: u32 = 250;
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(1);

type Shared<T> = Arc<Mutex<T>>;
type AudioBuf = Shared<VecDeque<f32>>;
type SharedInstant = Arc<Mutex<Instant>>;

#[derive(Copy, Clone, Debug)]
enum Ctrl {
    DeviceChanged,
    ServerLost,
}

pub fn exec(cmd: &ConnectCmd) -> Result<()> {
    let host = Arc::new(cpal::default_host());
    let (ctrl_tx, ctrl_rx) = mpsc::channel::<Ctrl>();

    {
        let host = host.clone();
        let tx = ctrl_tx.clone();
        thread::spawn(move || {
            let mut last = host
                .default_output_device()
                .and_then(|d| d.name().ok())
                .unwrap_or_default();
            loop {
                thread::sleep(Duration::from_millis(500));
                if let Some(dev) = host.default_output_device() {
                    if let Ok(name) = dev.name() {
                        if name != last {
                            last = name;
                            let _ = tx.send(Ctrl::DeviceChanged);
                        }
                    }
                }
            }
        });
    }

    loop {
        match run_once(cmd, &host, &ctrl_tx, &ctrl_rx) {
            Ok(_) => return Ok(()),
            Err(Error::ServerDisconnected) => {
                error!("Server unreachable – retrying in 5 s …");
                thread::sleep(Duration::from_secs(5));
                continue;
            }
            Err(e) => return Err(e),
        }
    }
}

fn run_once(
    cmd: &ConnectCmd,
    host: &Arc<cpal::Host>,
    ctrl_tx: &mpsc::Sender<Ctrl>,
    ctrl_rx: &mpsc::Receiver<Ctrl>,
) -> Result<()> {
    info!(
        "[UDP] Binding 0.0.0.0:{} -> {}:{}",
        cmd.src_port, cmd.ip, cmd.dest_port
    );
    let udp_socket = UdpSocket::bind(("0.0.0.0", cmd.src_port))?;
    udp_socket.set_read_timeout(Some(Duration::from_millis(200)))?;
    udp_socket.connect(SocketAddr::new(cmd.ip.into(), cmd.dest_port))?;
    let udp_socket = Arc::new(udp_socket);

    let cfg = request_config(&udp_socket)?;

    let host_rate = SampleRate(cfg.sample_rate);
    let opus_ch = count_to_channels(cfg.channels);
    let mut opus_decoder = opus::Decoder::new(host_rate.0, opus_ch).expect("Opus");

    let buf_cap = (host_rate.0 as u32 * MAX_BUFFER_MS / 1000 * cfg.channels as u32) as usize;
    let buffer: AudioBuf = Arc::new(Mutex::new(VecDeque::with_capacity(buf_cap)));
    let playing = Arc::new(AtomicBool::new(true));
    let running = Arc::new(AtomicBool::new(true));
    let last_server_response: SharedInstant = Arc::new(Mutex::new(Instant::now()));
    let clipboard = cmd
        .clipboard
        .then(|| clipboard::connected_clipboard(udp_socket.clone()));
    let mut clipboard_handle = clipboard
        .as_ref()
        .map(|clipboard| clipboard.spawn_polling(running.clone()));

    let mut stream = Some(build_stream(
        host,
        &StreamConfig {
            channels: cfg.channels,
            sample_rate: host_rate,
            buffer_size: BufferSize::Default,
        },
        buffer.clone(),
        playing.clone(),
    )?);

    let prod_handle = {
        let sock = udp_socket.try_clone()?;
        let buf = buffer.clone();
        let play = playing.clone();
        let run = running.clone();
        let last_response = last_server_response.clone();
        let clipboard = clipboard.clone();
        thread::spawn(move || {
            let mut pkt = [0u8; 1500];
            let mut tmp = vec![0.0f32; 4096];
            while run.load(Ordering::Relaxed) {
                match sock.recv(&mut pkt) {
                    Ok(sz) => match Packet::try_from(&pkt[..sz]) {
                        Ok(Packet::Audio(payload)) => {
                            *last_response.lock().unwrap() = Instant::now();
                            if !play.load(Ordering::Relaxed) {
                                continue;
                            }
                            match opus_decoder.decode_float(payload, &mut tmp, false) {
                                Ok(fr) => {
                                    let mut g = buf.lock().unwrap();
                                    let samples = &tmp[..fr * opus_ch as usize];
                                    while g.len() + samples.len() > buf_cap {
                                        g.pop_front();
                                    }
                                    g.extend(samples);
                                }
                                Err(e) => {
                                    let code = e.code();
                                    if let opus::ErrorCode::BufferTooSmall = code {
                                        tmp.resize(tmp.len() * 2, 0.0);
                                    }
                                    warn!("Opus err: {e}")
                                }
                            }
                        }
                        Ok(Packet::Pong(_)) | Ok(Packet::Config(_)) => {
                            *last_response.lock().unwrap() = Instant::now();
                        }
                        Ok(Packet::Clipboard(chunk)) => {
                            *last_response.lock().unwrap() = Instant::now();
                            if let Some(clipboard) = &clipboard {
                                if let Err(err) = clipboard.receive_chunk(chunk) {
                                    warn!("Failed to apply remote clipboard: {err}");
                                }
                            }
                        }
                        Ok(Packet::Hello) | Ok(Packet::Ping(_)) => {}
                        Err(_) => warn!("Ignoring invalid UDP packet from server"),
                    },
                    Err(ref e)
                        if e.kind() == std::io::ErrorKind::WouldBlock
                            || e.kind() == std::io::ErrorKind::TimedOut =>
                    {
                        continue;
                    }
                    Err(e) => {
                        warn!("UDP recv error: {e}");
                        break;
                    }
                }
            }
            info!("UDP producer thread exited");
        })
    };

    let heartbeat_handle = {
        let sock = udp_socket.try_clone()?;
        let run = running.clone();
        thread::spawn(move || {
            let mut packet = Vec::new();
            let mut id = 0_u64;
            while run.load(Ordering::Relaxed) {
                Packet::Ping(id).encode(&mut packet);
                if let Err(err) = sock.send(&packet) {
                    warn!("UDP ping failed: {err}");
                }
                id = id.wrapping_add(1);
                thread::sleep(HEARTBEAT_INTERVAL);
            }
        })
    };

    let monitor_handle = {
        let tx = ctrl_tx.clone();
        let run = running.clone();
        let last_response = last_server_response.clone();
        thread::spawn(move || {
            while run.load(Ordering::Relaxed) {
                let elapsed = last_response.lock().unwrap().elapsed();
                if elapsed >= DISCONNECT_TIMEOUT {
                    let _ = tx.send(Ctrl::ServerLost);
                    break;
                }
                thread::sleep(Duration::from_millis(100));
            }
        })
    };

    info!("🔊 Playback started – awaiting events");

    loop {
        match ctrl_rx.recv().unwrap() {
            Ctrl::DeviceChanged => {
                playing.store(false, Ordering::Relaxed);
                buffer.lock().unwrap().clear();
                stream.take();
                let rebuilt = build_stream(
                    host,
                    &StreamConfig {
                        channels: cfg.channels,
                        sample_rate: host_rate,
                        buffer_size: BufferSize::Default,
                    },
                    buffer.clone(),
                    playing.clone(),
                );
                match rebuilt {
                    Ok(new_stream) => {
                        stream = Some(new_stream);
                        playing.store(true, Ordering::Relaxed);
                    }
                    Err(err) => {
                        running.store(false, Ordering::Relaxed);
                        prod_handle.join().unwrap();
                        heartbeat_handle.join().unwrap();
                        monitor_handle.join().unwrap();
                        if let Some(clipboard_handle) = clipboard_handle.take() {
                            clipboard_handle.join().unwrap();
                        }
                        return Err(err);
                    }
                }
            }
            Ctrl::ServerLost => {
                info!("UDP server timed out – cleaning up");
                playing.store(false, Ordering::Relaxed);
                running.store(false, Ordering::Relaxed);
                buffer.lock().unwrap().clear();
                prod_handle.join().unwrap();
                heartbeat_handle.join().unwrap();
                monitor_handle.join().unwrap();
                if let Some(clipboard_handle) = clipboard_handle.take() {
                    clipboard_handle.join().unwrap();
                }
                stream.take();
                return Err(Error::ServerDisconnected);
            }
        }
    }
}

fn request_config(socket: &UdpSocket) -> Result<Config> {
    let mut hello = Vec::new();
    let mut pkt = [0u8; 1500];
    let mut last_hello = None::<Instant>;
    let started = Instant::now();

    Packet::Hello.encode(&mut hello);
    info!("[UDP] Sending hello and waiting for audio config");

    loop {
        if last_hello
            .map(|sent| sent.elapsed() >= Duration::from_millis(500))
            .unwrap_or(true)
        {
            socket.send(&hello)?;
            last_hello = Some(Instant::now());
        }

        match socket.recv(&mut pkt) {
            Ok(sz) => match Packet::try_from(&pkt[..sz]) {
                Ok(Packet::Config(json)) => return Ok(serde_json::from_slice(json)?),
                Ok(Packet::Pong(_)) | Ok(Packet::Audio(_)) | Ok(Packet::Clipboard(_)) => {}
                Ok(Packet::Hello) | Ok(Packet::Ping(_)) => {}
                Err(_) => warn!("Ignoring invalid UDP packet while waiting for config"),
            },
            Err(ref err)
                if err.kind() == std::io::ErrorKind::WouldBlock
                    || err.kind() == std::io::ErrorKind::TimedOut => {}
            Err(err) => return Err(err.into()),
        }

        if started.elapsed() >= DISCONNECT_TIMEOUT {
            return Err(Error::ServerDisconnected);
        }
    }
}

fn build_stream(
    host: &cpal::Host,
    template: &StreamConfig,
    buffer: AudioBuf,
    playing: Arc<AtomicBool>,
) -> Result<Stream> {
    let device = host.default_output_device().ok_or(Error::NoAudioDevice)?;

    let want_rate = template.sample_rate.0;
    let cfg_range = device
        .supported_output_configs()?
        .filter(|c| c.channels() == template.channels)
        .min_by_key(|c| {
            let min = c.min_sample_rate().0 as i32;
            let max = c.max_sample_rate().0 as i32;
            if min <= want_rate as i32 && want_rate as i32 <= max {
                0
            } else {
                let d1 = (min - want_rate as i32).abs();
                let d2 = (max - want_rate as i32).abs();
                d1.min(d2)
            }
        })
        .ok_or(Error::NoAudioDevice)?;

    let out_rate = if cfg_range.min_sample_rate().0 <= want_rate
        && want_rate <= cfg_range.max_sample_rate().0
    {
        want_rate
    } else {
        cfg_range.max_sample_rate().0
    };

    let out_cfg: StreamConfig = cfg_range.with_sample_rate(SampleRate(out_rate)).into();

    let ratio = want_rate as f32 / out_rate as f32;
    let err_fn = |e| error!("Stream error: {e}");

    let mut first_call = true;
    let mut frac = 0.0_f32;

    let stream = device.build_output_stream(
        &out_cfg,
        move |out: &mut [f32], _| {
            if first_call {
                buffer.lock().unwrap().clear();
                first_call = false;
            }

            let mut buf = buffer.lock().unwrap();
            if (ratio - 1.0).abs() < f32::EPSILON {
                let n = buf.len().min(out.len());
                for (d, s) in out.iter_mut().take(n).zip(buf.drain(..n)) {
                    *d = s.clamp(-1.0, 1.0);
                }
                out[n..].fill(0.0);
            } else {
                for dst in out.iter_mut() {
                    if buf.len() < 2 {
                        *dst = 0.0;
                        continue;
                    }
                    let s0 = buf[0];
                    let s1 = buf[1];
                    *dst = (s0 + (s1 - s0) * frac).clamp(-1.0, 1.0);

                    frac += ratio;
                    while frac >= 1.0 && !buf.is_empty() {
                        buf.pop_front();
                        frac -= 1.0;
                    }
                }
            }
        },
        err_fn,
        None,
    )?;
    stream.play()?;
    playing.store(true, Ordering::Relaxed);
    Ok(stream)
}
