use crate::{
    cli::ConnectCmd,
    config::{count_to_channels, Config},
    error::{Error, Result},
};
use cpal::{
    traits::{DeviceTrait, HostTrait, StreamTrait},
    StreamConfig, SupportedStreamConfig,
};
use std::{
    io::{Read, Write},
    net::{SocketAddr, TcpStream, UdpSocket},
    sync::{Arc, Mutex},
    time::Instant,
};
use tracing::{error, info};

pub fn exec(cmd: &ConnectCmd) -> Result<()> {
    let tcp_target_ip = SocketAddr::new(cmd.ip.into(), cmd.tcp_dest_port);
    let udp_target_ip = SocketAddr::new(cmd.ip.into(), cmd.udp_dest_port);
    info!("[TCP] Connecting to {}...", tcp_target_ip);
    let tcp_stream = loop {
        match TcpStream::connect(tcp_target_ip) {
            Ok(stream) => break Arc::new(Mutex::new(stream)),
            Err(e) => {
                error!(
                    "Failed to connect to server: {} retrying in 5 seconds...",
                    e
                );
                std::thread::sleep(std::time::Duration::from_secs(5));
            }
        }
    };

    let tcp_stream_clone = tcp_stream.clone();
    info!("[TCP] Connected to server");

    info!("[TCP] Sending UDP port to server...");
    let mut guard = tcp_stream.lock().expect("Failed to lock TCP stream");
    guard
        .write_all(&cmd.udp_src_port.to_be_bytes())
        .expect("Failed to send UDP port to server");

    let mut length_bytes = [0u8; 4];
    guard.read_exact(&mut length_bytes)?;
    let message_length = u32::from_be_bytes(length_bytes);

    let mut config_data_bytes = vec![0u8; message_length as usize];
    guard.read_exact(&mut config_data_bytes)?;

    drop(guard);

    std::thread::spawn(move || loop {
        std::thread::sleep(std::time::Duration::from_secs(5));
        let start = Instant::now();
        let mut guard = tcp_stream_clone.lock().expect("Failed to lock TCP stream");
        if let Err(e) = guard.write_all(b"ping") {
            error!("Failed to send ping: {}", e);
            break;
        }
        if let Err(e) = guard.flush() {
            error!("Failed to flush stream: {}", e);
            break;
        }
        let mut buf = [0; 4];
        if let Err(e) = guard.read_exact(&mut buf) {
            error!("Failed to read pong: {}", e);
            break;
        }
        let end = Instant::now();
        let latency = end.duration_since(start).as_millis();
        info!("[TCP] Latency: {}ms", latency);
        drop(guard);
    });
    let config_data = String::from_utf8(config_data_bytes).expect("Invalid config data");
    let config: Config = serde_json::from_str(&config_data)?;

    let host = cpal::default_host();
    let device = host.default_output_device().ok_or(Error::NoAudioDevice)?;
    let config = SupportedStreamConfig::from(config);
    let sample_format = config.sample_format();
    info!(
                "Client is configured to process audio: Sample format: {:?}, Sample rate: {:?}, Channels: {:?}, Buffer size: {:?}",
                sample_format,
                config.sample_rate(),
                config.channels(),
                config.buffer_size()
            );

    info!("[UDP] Binding to {}...", cmd.udp_src_port);
    let socket = UdpSocket::bind(format!("0.0.0.0:{}", cmd.udp_src_port))
        .expect("Failed to bind UDP socket");
    socket
        .connect(udp_target_ip)
        .expect("Failed to connect to UDP target");

    let stream_config: StreamConfig = config.clone().into();
    match sample_format {
        cpal::SampleFormat::F32 => {
            let err_fn = |err| error!("an error occurred on stream: {}", err);

            let socket = socket.try_clone().expect("Failed to clone socket");

            let mut decode_buf = vec![0i16; 2048]; // Adjust buffer size as needed
            let channels = count_to_channels(stream_config.channels);
            let channel_count = stream_config.channels as usize;

            let mut decoder = opus::Decoder::new(stream_config.sample_rate.0, channels)
                .expect("Failed to create decoder");

            let mut tmp_buf: Vec<f32> = Vec::new();
            info!("Stream has started. Press Ctrl+C to terminate.");
            let stream = device.build_output_stream(
                &stream_config,
                move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                    let needed_frames = data.len();
                    while tmp_buf.len() < needed_frames {
                        let mut buffer = [0u8; 1024]; // UDP receive buffer size
                        match socket.recv(&mut buffer) {
                            Ok(size) => {
                                let received_data = &buffer[..size];
                                let size = decoder
                                    .decode(received_data, &mut decode_buf, false)
                                    .expect("Failed to decode data");
                                let decoded_data = &decode_buf[..size * channel_count]
                                    .iter()
                                    .map(|&x| x as f32 / i16::MAX as f32)
                                    .collect::<Vec<f32>>();
                                tmp_buf.extend_from_slice(decoded_data);
                                decode_buf.resize(size * channel_count, 0);
                            }
                            Err(e) => writeln!(std::io::stderr(), "Socket receive error: {}", e)
                                .expect("Failed to write to stderr"),
                        }
                    }

                    let (frame_bytes, leftover) = tmp_buf.split_at(needed_frames);

                    for (i, &sample) in frame_bytes.iter().enumerate() {
                        data[i] = sample;
                    }
                    tmp_buf = leftover.to_vec();
                },
                err_fn,
                None,
            )?;

            stream.play()?;

            loop {
                std::thread::sleep(std::time::Duration::from_millis(1000));
            }
        }
        _ => Err(Error::UnsupportedSampleFormat(sample_format)),
    }
}
