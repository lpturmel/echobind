use crate::{
    cli::RecordCmd,
    config::{count_to_channels, BufferSize, Config},
    error::{Error, Result},
};
use cpal::{
    traits::{DeviceTrait, HostTrait, StreamTrait},
    StreamConfig,
};
use std::{
    io::{Read, Write},
    net::{SocketAddr, TcpListener, UdpSocket},
    str::FromStr,
    sync::{mpsc::channel, Arc},
    thread,
};
use tracing::{error, info, warn};

const MAX_RETRIES: usize = 3;
const OPUS_FRAME_MS: usize = 20;

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

    let tcp_addr = format!("0.0.0.0:{}", cmd.tcp_port);
    let tcp_addr = SocketAddr::from_str(&tcp_addr).expect("Invalid address");
    info!("[TCP] Listening on {}...", tcp_addr);
    let udp_addr = format!("0.0.0.0:{}", cmd.udp_port);
    let udp_addr = SocketAddr::from_str(&udp_addr).expect("Invalid address");
    info!("[UDP] Listening on {}...", udp_addr);

    let tcp_listener = TcpListener::bind(tcp_addr)?;
    let udp_listener = Arc::new(UdpSocket::bind(udp_addr)?);

    loop {
        let udp_listener = udp_listener.clone();
        info!("[TCP] Waiting for client to connect...");
        let (mut tcp_stream, client_addr) = tcp_listener.accept()?;
        info!("[TCP] Client connected from {}", client_addr);

        info!("[UDP] Waiting for client to send UDP port...");
        let mut udp_port_buf = [0; 2];
        tcp_stream.read_exact(&mut udp_port_buf)?;
        let udp_port = u16::from_be_bytes(udp_port_buf);
        info!("[UDP] Client using UDP port: {}", udp_port);

        let udp_addr = format!("{}:{}", client_addr.ip(), udp_port);
        let udp_addr = SocketAddr::from_str(&udp_addr).expect("Invalid address");

        let (min, max) = match config.buffer_size() {
            cpal::SupportedBufferSize::Range { min, max } => (*min, *max),
            cpal::SupportedBufferSize::Unknown => (0_u32, 0_u32),
        };
        let config_data = serde_json::to_string(&Config {
            sample_format: format!("{}", sample_format),
            sample_rate: negotiated_sample_rate,
            channels: stream_config.channels,
            buffer_size: BufferSize { min, max },
        })?;
        let config_data = config_data.as_bytes();
        let data_length = config_data.len() as u32;
        tcp_stream.write_all(&data_length.to_be_bytes())?;
        tcp_stream.write_all(config_data)?;
        tcp_stream.flush()?;

        let err_fn = |err| error!("an error occurred on the audio stream: {}", err);
        let (tx, rx) = channel::<Vec<f32>>();
        let stream = match sample_format {
            cpal::SampleFormat::F32 => {
                let sample_rate = negotiated_sample_rate;
                let channels = count_to_channels(stream_config.channels);
                let channel_count = stream_config.channels as usize;
                if sample_rate % 50 != 0 {
                    warn!(
                        "Sample rate {} does not align perfectly with 20ms Opus frames; audio may be degraded",
                        sample_rate
                    );
                }
                let frame_len = (sample_rate as usize * OPUS_FRAME_MS / 1000) * channel_count;
                let mut encoder =
                    opus::Encoder::new(sample_rate, channels, opus::Application::LowDelay)?;
                let udp_listener_clone = udp_listener.clone();
                thread::spawn(move || {
                    let mut pending = Vec::<f32>::with_capacity(frame_len * 2);
                    let mut output = vec![0; 2048];
                    for data in rx {
                        pending.extend(data);
                        while pending.len() >= frame_len {
                            let mut retries = 0;
                            loop {
                                match encoder.encode_float(&pending[..frame_len], &mut output) {
                                    Ok(len) => {
                                        output.truncate(len);
                                        let _ = udp_listener_clone.send_to(&output, udp_addr);
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

                device.build_input_stream(
                    &stream_config,
                    move |data: &[f32], _: &_| {
                        let _ = tx.send(data.to_vec());
                    },
                    err_fn,
                    None,
                )
            }
            sample_format => {
                return Err(Error::UnsupportedSampleFormat(sample_format));
            }
        }?;

        stream.play()?;
        info!("Audio stream started. Press Ctrl+C to terminate.");
        let mut tcp_buf = [0; 512];
        let mut connected = true;
        while connected {
            match tcp_stream.read(&mut tcp_buf) {
                Ok(0) => {
                    info!(
                        "Client {} disconnected, stopping audio stream...",
                        client_addr
                    );
                    connected = false;
                }
                Ok(_) => {}
                Err(e) => {
                    error!("Error reading from TCP stream: {}", e);
                }
            }
        }
        drop(tcp_stream);
        drop(stream);
        info!("Audio stream stopped.");
    }
}
