use crate::{
    cli::RecordCmd,
    clipboard::{self, ClipboardBehavior, SystemClipboard},
    config::{count_to_channels, BufferSize, Config},
    error::{Error, Result},
    protocol::Packet,
};
use cpal::{
    traits::{DeviceTrait, HostTrait, StreamTrait},
    StreamConfig,
};
use std::{
    net::{SocketAddr, UdpSocket},
    str::FromStr,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::channel,
        Arc,
    },
    thread,
    time::{Duration, Instant},
};
use tracing::{error, info, warn};

const MAX_RETRIES: usize = 3;
const OPUS_FRAME_MS: usize = 20;
pub const DISCONNECT_TIMEOUT: Duration = Duration::from_secs(3);

pub fn exec(cmd: &RecordCmd) -> Result<()> {
    let host = cpal::default_host();
    let device = host.default_output_device().ok_or(Error::NoAudioDevice)?;
    info!(
        "Using input device: {}",
        device.name().expect("Invalid device name")
    );
    let config = device.default_output_config()?;
    let sample_format = config.sample_format();
    let mut stream_config: StreamConfig = config.clone().into();
    let mut negotiated_sample_rate = stream_config.sample_rate.0;
    const OPUS_SAMPLE_RATES: [u32; 5] = [8_000, 12_000, 16_000, 24_000, 48_000];

    if !OPUS_SAMPLE_RATES.contains(&negotiated_sample_rate) {
        match device.supported_input_configs() {
            Ok(configs) => {
                let configs: Vec<_> = configs.collect();
                let mut selected: Option<StreamConfig> = None;
                'outer: for &candidate in OPUS_SAMPLE_RATES.iter().rev() {
                    for cfg_range in &configs {
                        if cfg_range.channels() != stream_config.channels {
                            continue;
                        }
                        if cfg_range.sample_format() != sample_format {
                            continue;
                        }
                        let min = cfg_range.min_sample_rate().0;
                        let max = cfg_range.max_sample_rate().0;
                        if candidate < min || candidate > max {
                            continue;
                        }
                        let cfg: StreamConfig = cfg_range
                            .with_sample_rate(cpal::SampleRate(candidate))
                            .into();
                        selected = Some(cfg);
                        negotiated_sample_rate = candidate;
                        info!(
                            "Adjusting capture sample rate from {} to Opus-compatible {}",
                            stream_config.sample_rate.0, negotiated_sample_rate
                        );
                        break 'outer;
                    }
                }

                if let Some(cfg) = selected {
                    stream_config = cfg;
                } else {
                    warn!(
                        "Unable to find Opus-compatible sample rate; continuing with {}",
                        negotiated_sample_rate
                    );
                }
            }
            Err(err) => {
                warn!(
                    "Unable to query supported input configs, continuing with default sample rate: {err}"
                );
            }
        }
    }

    info!(
        "Server audio config -> format: {:?}, sample rate: {}, channels: {}, buffer size: {:?}",
        sample_format,
        negotiated_sample_rate,
        stream_config.channels,
        config.buffer_size()
    );

    let udp_addr = format!("0.0.0.0:{}", cmd.port);
    let udp_addr = SocketAddr::from_str(&udp_addr).expect("Invalid address");
    info!("[UDP] Listening on {}...", udp_addr);

    let udp_listener = Arc::new(UdpSocket::bind(udp_addr)?);
    udp_listener.set_read_timeout(Some(Duration::from_millis(200)))?;

    let (min, max) = match config.buffer_size() {
        cpal::SupportedBufferSize::Range { min, max } => (*min, *max),
        cpal::SupportedBufferSize::Unknown => (0_u32, 0_u32),
    };
    let config_data = serde_json::to_vec(&Config {
        sample_format: format!("{}", sample_format),
        sample_rate: negotiated_sample_rate,
        channels: stream_config.channels,
        buffer_size: BufferSize { min, max },
    })?;
    let mut config_packet = Vec::new();
    Packet::Config(&config_data).encode(&mut config_packet);

    loop {
        info!("[UDP] Waiting for client hello...");
        let client_addr = wait_for_client(&udp_listener, &config_packet)?;
        info!("[UDP] Client connected from {}", client_addr);
        let clipboard_running = cmd.clipboard.then(|| Arc::new(AtomicBool::new(true)));
        let clipboard = cmd
            .clipboard
            .then(|| clipboard::peer_clipboard(udp_listener.clone(), client_addr));
        let clipboard_handle = clipboard
            .as_ref()
            .zip(clipboard_running.as_ref())
            .map(|(clipboard, running)| clipboard.spawn_polling(running.clone()));

        let err_fn = |err| error!("an error occurred on the audio stream: {}", err);
        let (tx, rx) = channel::<Vec<f32>>();
        match sample_format {
            cpal::SampleFormat::F32 => {
                let sample_rate = negotiated_sample_rate;
                let channels = count_to_channels(stream_config.channels);
                let channel_count = stream_config.channels as usize;
                if !sample_rate.is_multiple_of(50) {
                    warn!(
                        "Sample rate {} does not align perfectly with 20ms Opus frames; audio may be degraded",
                        sample_rate
                    );
                }
                let frame_len = (sample_rate as usize * OPUS_FRAME_MS / 1000) * channel_count;
                let mut encoder =
                    opus::Encoder::new(sample_rate, channels, opus::Application::LowDelay)?;
                let udp_listener_clone = udp_listener.clone();
                let encoder_handle = thread::spawn(move || {
                    let mut pending = Vec::<f32>::with_capacity(frame_len * 2);
                    let mut output = vec![0; 2048];
                    let mut packet = Vec::with_capacity(output.len() + 5);
                    for data in rx {
                        pending.extend(data);
                        while pending.len() >= frame_len {
                            let mut retries = 0;
                            loop {
                                match encoder.encode_float(&pending[..frame_len], &mut output) {
                                    Ok(len) => {
                                        output.truncate(len);
                                        Packet::Audio(&output).encode(&mut packet);
                                        let _ = udp_listener_clone.send_to(&packet, client_addr);
                                        output.resize(2048, 0);
                                        break;
                                    }
                                    Err(e) => {
                                        if e.code() == opus::ErrorCode::BufferTooSmall {
                                            output.resize(output.len() * 2, 0);
                                            continue;
                                        }
                                        retries += 1;
                                        if retries >= MAX_RETRIES {
                                            warn!("Opus encode failed ({e}); dropping frame");
                                            break;
                                        }
                                    }
                                }
                            }
                            if pending.len() == frame_len {
                                pending.clear();
                            } else {
                                pending.drain(..frame_len);
                            }
                        }
                    }
                });

                let stream = device.build_input_stream(
                    &stream_config,
                    move |data: &[f32], _: &_| {
                        let _ = tx.send(data.to_vec());
                    },
                    err_fn,
                    None,
                )?;
                stream.play()?;
                info!("Audio stream started. Press Ctrl+C to terminate.");
                monitor_client(&udp_listener, client_addr, &config_packet, clipboard);
                if let Some(running) = clipboard_running {
                    running.store(false, Ordering::Relaxed);
                }
                drop(stream);
                let _ = encoder_handle.join();
                if let Some(clipboard_handle) = clipboard_handle {
                    let _ = clipboard_handle.join();
                }
                info!("Audio stream stopped.");
                continue;
            }
            sample_format => {
                if let Some(running) = clipboard_running {
                    running.store(false, Ordering::Relaxed);
                }
                if let Some(clipboard_handle) = clipboard_handle {
                    let _ = clipboard_handle.join();
                }
                return Err(Error::UnsupportedSampleFormat(sample_format));
            }
        }
    }
}

fn wait_for_client(socket: &UdpSocket, config_packet: &[u8]) -> Result<SocketAddr> {
    let mut pkt = [0u8; 1500];
    loop {
        let (sz, addr) = match socket.recv_from(&mut pkt) {
            Ok(packet) => packet,
            Err(ref err)
                if err.kind() == std::io::ErrorKind::WouldBlock
                    || err.kind() == std::io::ErrorKind::TimedOut =>
            {
                continue;
            }
            Err(err) => return Err(err.into()),
        };
        match Packet::try_from(&pkt[..sz]) {
            Ok(Packet::Hello) => {
                socket.send_to(config_packet, addr)?;
                return Ok(addr);
            }
            Ok(Packet::Ping(id)) => {
                let mut pong = Vec::new();
                Packet::Pong(id).encode(&mut pong);
                socket.send_to(&pong, addr)?;
            }
            Ok(_) => {}
            Err(_) => warn!("Ignoring invalid UDP packet from {addr}"),
        }
    }
}

fn monitor_client(
    socket: &UdpSocket,
    client_addr: SocketAddr,
    config_packet: &[u8],
    clipboard: Option<SystemClipboard>,
) {
    let mut pkt = [0u8; 1500];
    let mut response = Vec::new();
    let mut last_seen = Instant::now();

    loop {
        if last_seen.elapsed() >= DISCONNECT_TIMEOUT {
            info!(
                "Client {} timed out after {} seconds without a UDP answer",
                client_addr,
                DISCONNECT_TIMEOUT.as_secs()
            );
            break;
        }

        match socket.recv_from(&mut pkt) {
            Ok((sz, addr)) if addr == client_addr => match Packet::try_from(&pkt[..sz]) {
                Ok(Packet::Hello) => {
                    last_seen = Instant::now();
                    if let Err(err) = socket.send_to(config_packet, client_addr) {
                        warn!("Failed to resend config to {client_addr}: {err}");
                    }
                }
                Ok(Packet::Ping(id)) => {
                    last_seen = Instant::now();
                    Packet::Pong(id).encode(&mut response);
                    if let Err(err) = socket.send_to(&response, client_addr) {
                        warn!("Failed to send pong to {client_addr}: {err}");
                    }
                }
                Ok(Packet::Clipboard(chunk)) => {
                    last_seen = Instant::now();
                    if let Some(clipboard) = &clipboard {
                        if let Err(err) = clipboard.receive_chunk(chunk) {
                            warn!("Failed to apply remote clipboard from {client_addr}: {err}");
                        }
                    }
                }
                Ok(_) => {
                    last_seen = Instant::now();
                }
                Err(_) => warn!("Ignoring invalid UDP packet from {addr}"),
            },
            Ok((_, addr)) => warn!("Ignoring UDP packet from non-active client {addr}"),
            Err(ref err)
                if err.kind() == std::io::ErrorKind::WouldBlock
                    || err.kind() == std::io::ErrorKind::TimedOut => {}
            Err(err) => {
                warn!("UDP receive error while monitoring {client_addr}: {err}");
                break;
            }
        }
    }
}
