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

pub fn exec(cmd: &RecordCmd) -> Result<()> {
    let host = cpal::default_host();
    let device = host.default_output_device().ok_or(Error::NoAudioDevice)?;
    info!(
        "Using input device: {}",
        device.name().expect("Invalid device name")
    );
    let config = device.default_output_config()?;
    let sample_format = config.sample_format();
    let stream_config: StreamConfig = config.clone().into();
    info!(
                "Server is configured to send audio: Sample format: {:?}, Sample rate: {:?}, Channels: {:?}, Buffer size: {:?}",
                sample_format,
                config.sample_rate(),
                config.channels(),
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
            sample_rate: config.sample_rate().0,
            channels: config.channels(),
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
                let sample_rate = stream_config.sample_rate.0;
                let channels = count_to_channels(stream_config.channels);
                let mut encoder =
                    opus::Encoder::new(sample_rate, channels, opus::Application::LowDelay)
                        .expect("Failed to create encoder");
                let udp_listener_clone = udp_listener.clone();
                thread::spawn(move || {
                    for data in rx {
                        let mut output = vec![0; 2048]; // Output buffer for encoded data
                        let mut retries = 0;
                        loop {
                            match encoder.encode_float(&data, &mut output) {
                                Ok(len) => {
                                    output.truncate(len);
                                    let _ = udp_listener_clone.send_to(&output, udp_addr);
                                    break;
                                }
                                Err(_) => {
                                    retries += 1;
                                    if retries >= MAX_RETRIES {
                                        warn!("Max retries reached, dropping frame.");
                                        break;
                                    }
                                }
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
                Ok(len) => {
                    info!("Received {} bytes from client: {:?}", len, &tcp_buf[..len]);
                    let data = &tcp_buf[..len];
                    let str = std::str::from_utf8(data).expect("Invalid UTF-8 data");
                    if str == "ping" {
                        info!("Received ping from client, sending pong...");
                        tcp_stream.write_all(b"pong")?;
                        tcp_stream.flush()?;
                    }
                }
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
