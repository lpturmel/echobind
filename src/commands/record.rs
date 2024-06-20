use crate::cli::RecordCmd;
use crate::config::{count_to_channels, BufferSize, Config};
use crate::error::{Error, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::StreamConfig;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, UdpSocket};
use std::str::FromStr;
use std::sync::Arc;

pub fn exec(cmd: &RecordCmd) -> Result<()> {
    let host = cpal::default_host();
    let device = host.default_output_device().ok_or(Error::NoAudioDevice)?;
    println!(
        "Using input device: {}",
        device.name().expect("Invalid device name")
    );
    let config = device.default_output_config()?;
    let sample_format = config.sample_format();
    let stream_config: StreamConfig = config.clone().into();
    println!(
                "Server is configured to send audio: Sample format: {:?}, Sample rate: {:?}, Channels: {:?}, Buffer size: {:?}",
                sample_format,
                config.sample_rate(),
                config.channels(),
                config.buffer_size()
            );

    let tcp_addr = format!("0.0.0.0:{}", cmd.tcp_port);
    let tcp_addr = SocketAddr::from_str(&tcp_addr).expect("Invalid address");
    println!("[TCP] Listening on {}...", tcp_addr);
    let udp_addr = format!("0.0.0.0:{}", cmd.udp_port);
    let udp_addr = SocketAddr::from_str(&udp_addr).expect("Invalid address");
    println!("[UDP] Listening on {}...", udp_addr);

    let tcp_listener = TcpListener::bind(tcp_addr)?;
    let udp_listener = Arc::new(UdpSocket::bind(udp_addr)?);

    loop {
        let udp_listener = udp_listener.clone();
        println!("[TCP] Waiting for client to connect...");
        let (mut tcp_stream, client_addr) = tcp_listener.accept()?;
        println!("[TCP] Client connected from {}", client_addr);

        println!("[UDP] Waiting for client to send UDP port...");
        let mut udp_port_buf = [0; 2];
        tcp_stream.read_exact(&mut udp_port_buf)?;
        let udp_port = u16::from_be_bytes(udp_port_buf);
        println!("[UDP] Client using UDP port: {}", udp_port);

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
        tcp_stream.write_all(&data_length.to_be_bytes())?; // Send the length of the config data
        tcp_stream.write_all(config_data)?; // Send the config data
        tcp_stream.flush()?;
        println!("Sent stream configuration to client");

        let err_fn = |err| eprintln!("an error occurred on the audio stream: {}", err);
        let stream = match sample_format {
            cpal::SampleFormat::F32 => {
                let sample_rate = stream_config.sample_rate.0;
                let channels = count_to_channels(stream_config.channels);
                let mut encoder =
                    opus::Encoder::new(sample_rate, channels, opus::Application::LowDelay)
                        .expect("Failed to create encoder");
                device.build_input_stream(
                    &stream_config,
                    move |data: &[f32], _: &_| {
                        let samples = data
                            .iter()
                            .map(|&sample| (sample * i16::MAX as f32) as i16)
                            .collect::<Vec<i16>>();
                        let mut output = vec![0; samples.len() * 2]; // Output buffer for encoded data
                        match encoder.encode(&samples, &mut output) {
                            Ok(len) => {
                                output.truncate(len);
                                let _ = udp_listener.send_to(&output, udp_addr);
                            }
                            Err(e) => {
                                eprintln!("Error encoding bytes... {}", e);
                            }
                        }
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
        println!("Audio stream started. Press Ctrl+C to terminate.");
        let mut tcp_buf = [0; 512];
        let mut connected = true;
        while connected {
            match tcp_stream.read(&mut tcp_buf) {
                Ok(0) => {
                    println!(
                        "Client {} disconnected, stopping audio stream...",
                        client_addr
                    );
                    connected = false;
                }
                Ok(len) => {
                    println!("Received {} bytes from client: {:?}", len, &tcp_buf[..len]);
                    let data = &tcp_buf[..len];
                    let str = std::str::from_utf8(data).expect("Invalid UTF-8 data");
                    if str == "ping" {
                        println!("Received ping from client, sending pong...");
                        tcp_stream.write_all(b"pong")?;
                        tcp_stream.flush()?;
                    }
                }
                Err(e) => {
                    eprintln!("Error reading from TCP stream: {}", e);
                }
            }
        }
        drop(tcp_stream);
        drop(stream);
        println!("Audio stream stopped.");
    }
}
