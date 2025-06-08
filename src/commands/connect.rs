use crate::{
    cli::ConnectCmd,
    config::{count_to_channels, Config},
    error::{Error, Result},
};
use cpal::{
    traits::{DeviceTrait, HostTrait, StreamTrait},
    BufferSize, SampleRate, Stream, StreamConfig,
};
use std::{
    collections::VecDeque,
    io::{Read, Write},
    net::{SocketAddr, TcpStream, UdpSocket},
    sync::{mpsc, Arc, Mutex},
    thread,
    time::{Duration, Instant},
};
use tracing::{error, info, warn};

type Shared<T> = Arc<Mutex<T>>;
type AudioBuf = Shared<VecDeque<f32>>;

pub fn exec(cmd: &ConnectCmd) -> Result<()> {
    let tcp_target_ip = SocketAddr::new(cmd.ip.into(), cmd.tcp_dest_port);
    info!("[TCP] Connecting to {tcp_target_ip} …");

    let tcp_stream = loop {
        match TcpStream::connect(tcp_target_ip) {
            Ok(s) => break Arc::new(Mutex::new(s)),
            Err(e) => {
                error!("Failed to connect: {e} – retrying in 5 s");
                thread::sleep(Duration::from_secs(5));
            }
        }
    };
    info!("[TCP] Connected");

    tcp_stream
        .lock()
        .unwrap()
        .write_all(&cmd.udp_src_port.to_be_bytes())?;

    let cfg: Config = {
        let mut guard = tcp_stream.lock().unwrap();
        let mut len = [0u8; 4];
        guard.read_exact(&mut len)?;
        let json_len = u32::from_be_bytes(len) as usize;
        let mut json = vec![0u8; json_len];
        guard.read_exact(&mut json)?;
        serde_json::from_slice(&json)?
    };

    info!(
        "[UDP] Binding 0.0.0.0:{}  →  server {}:{}",
        cmd.udp_src_port, cmd.ip, cmd.udp_dest_port
    );
    let udp_socket = UdpSocket::bind(("0.0.0.0", cmd.udp_src_port))?;
    udp_socket.connect(SocketAddr::new(cmd.ip.into(), cmd.udp_dest_port))?;

    let host_rate = SampleRate(cfg.sample_rate);
    let opus_ch = count_to_channels(cfg.channels);
    let mut opus = opus::Decoder::new(host_rate.0, opus_ch).expect("Opus decoder");

    let buffer: AudioBuf = Arc::new(Mutex::new(VecDeque::with_capacity(96_000)));

    {
        let sock = udp_socket.try_clone()?;
        let buf = buffer.clone();
        thread::spawn(move || {
            let mut udp_pkt = [0u8; 1500];
            let mut dec = vec![0.0f32; 4096];
            loop {
                match sock.recv(&mut udp_pkt) {
                    Ok(sz) => match opus.decode_float(&udp_pkt[..sz], &mut dec, false) {
                        Ok(frames) => {
                            let mut g = buf.lock().unwrap();
                            g.extend(&dec[..frames * opus_ch as usize]);
                        }
                        Err(e) => {
                            let code = e.code();
                            if let opus::ErrorCode::BufferTooSmall = code {
                                dec.resize(dec.len() * 2, 0.0);
                            } else {
                                warn!("Opus error: {e}");
                            }
                        }
                    },
                    Err(e) => warn!("UDP recv error: {e}"),
                }
            }
        });
    }

    {
        let tcp = tcp_stream.clone();
        thread::spawn(move || loop {
            thread::sleep(Duration::from_secs(5));
            let start = Instant::now();
            let mut g = tcp.lock().unwrap();
            if g.write_all(b"ping").is_err() || g.flush().is_err() {
                error!("Ping failed – dropping thread");
                break;
            }
            let mut pong = [0u8; 4];
            if g.read_exact(&mut pong).is_err() {
                error!("No pong – server gone?");
                break;
            }
            info!("[TCP] RTT {:?}", start.elapsed());
        });
    }

    let host = Arc::new(cpal::default_host());
    let stream_cfg_template = StreamConfig {
        channels: cfg.channels,
        sample_rate: host_rate,
        buffer_size: BufferSize::Default,
    };

    let mut current_stream = Some(build_stream_for_default_device(
        &host,
        &stream_cfg_template,
        buffer.clone(),
    )?);

    let (tx_dev, rx_dev) = mpsc::channel::<()>();
    {
        let host = host.clone();
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
                            if tx_dev.send(()).is_err() {
                                break;
                            }
                        }
                    }
                }
            }
        });
    }

    info!("🔊 Playback started");
    loop {
        rx_dev.recv().unwrap();
        info!("🎧 Default output changed – rebuilding stream");
        buffer.lock().unwrap().clear();

        current_stream.take();

        current_stream = Some(build_stream_for_default_device(
            &host,
            &stream_cfg_template,
            buffer.clone(),
        )?);
    }
}

fn build_stream_for_default_device(
    host: &cpal::Host,
    template: &StreamConfig,
    buffer: AudioBuf,
) -> Result<Stream> {
    let device = host.default_output_device().ok_or(Error::NoAudioDevice)?;

    let supported = device
        .supported_output_configs()?
        .find(|f| f.channels() == template.channels)
        .ok_or(Error::NoAudioDevice)?
        .with_sample_rate(template.sample_rate);

    let cfg: StreamConfig = supported.into();
    let err_fn = |e| error!("Stream error: {e}");

    let stream = device.build_output_stream(
        &cfg,
        move |out: &mut [f32], _| {
            let mut buf = buffer.lock().unwrap();
            let avail = buf.len().min(out.len());
            for (dst, src) in out.iter_mut().take(avail).zip(buf.drain(..avail)) {
                *dst = src;
            }
            for dst in &mut out[avail..] {
                *dst = 0.0; // underrun => silence
            }
        },
        err_fn,
        None,
    )?;
    stream.play()?;
    Ok(stream)
}
// pub fn exec(cmd: &ConnectCmd) -> Result<()> {
//     let tcp_target_ip = SocketAddr::new(cmd.ip.into(), cmd.tcp_dest_port);
//     let udp_target_ip = SocketAddr::new(cmd.ip.into(), cmd.udp_dest_port);
//     info!("[TCP] Connecting to {}...", tcp_target_ip);
//     let tcp_stream = loop {
//         match TcpStream::connect(tcp_target_ip) {
//             Ok(stream) => break Arc::new(Mutex::new(stream)),
//             Err(e) => {
//                 error!(
//                     "Failed to connect to server: {} retrying in 5 seconds...",
//                     e
//                 );
//                 std::thread::sleep(std::time::Duration::from_secs(5));
//             }
//         }
//     };
//
//     let tcp_stream_clone = tcp_stream.clone();
//     info!("[TCP] Connected to server");
//
//     info!("[TCP] Sending UDP port to server...");
//     let mut guard = tcp_stream.lock().expect("Failed to lock TCP stream");
//     guard
//         .write_all(&cmd.udp_src_port.to_be_bytes())
//         .expect("Failed to send UDP port to server");
//
//     let mut length_bytes = [0u8; 4];
//     guard.read_exact(&mut length_bytes)?;
//     let message_length = u32::from_be_bytes(length_bytes);
//
//     let mut config_data_bytes = vec![0u8; message_length as usize];
//     guard.read_exact(&mut config_data_bytes)?;
//
//     drop(guard);
//
//     std::thread::spawn(move || loop {
//         std::thread::sleep(std::time::Duration::from_secs(5));
//         let start = Instant::now();
//         let mut guard = tcp_stream_clone.lock().expect("Failed to lock TCP stream");
//         if let Err(e) = guard.write_all(b"ping") {
//             error!("Failed to send ping: {}", e);
//             break;
//         }
//         if let Err(e) = guard.flush() {
//             error!("Failed to flush stream: {}", e);
//             break;
//         }
//         let mut buf = [0; 4];
//         if let Err(e) = guard.read_exact(&mut buf) {
//             error!("Failed to read pong: {}", e);
//             break;
//         }
//         let end = Instant::now();
//         let latency = end.duration_since(start);
//         info!("[TCP] Latency: {:?}", latency);
//         drop(guard);
//     });
//     let config_data = String::from_utf8(config_data_bytes).expect("Invalid config data");
//     let config: Config = serde_json::from_str(&config_data)?;
//
//     let host = cpal::default_host();
//     let device = host.default_output_device().ok_or(Error::NoAudioDevice)?;
//     let config = SupportedStreamConfig::from(config);
//     let sample_format = config.sample_format();
//     info!(
//                 "Client is configured to process audio: Sample format: {:?}, Sample rate: {:?}, Channels: {:?}, Buffer size: {:?}",
//                 sample_format,
//                 config.sample_rate(),
//                 config.channels(),
//                 config.buffer_size()
//             );
//
//     info!("[UDP] Binding to {}...", cmd.udp_src_port);
//     let socket = UdpSocket::bind(format!("0.0.0.0:{}", cmd.udp_src_port))
//         .expect("Failed to bind UDP socket");
//     socket
//         .connect(udp_target_ip)
//         .expect("Failed to connect to UDP target");
//
//     let stream_config: StreamConfig = config.clone().into();
//     match sample_format {
//         cpal::SampleFormat::F32 => {
//             let err_fn = |err| error!("an error occurred on stream: {}", err);
//
//             let socket = socket.try_clone().expect("Failed to clone socket");
//
//             let mut decode_buf = vec![0i16; 2048];
//             let channels = count_to_channels(stream_config.channels);
//             let channel_count = stream_config.channels as usize;
//
//             let mut decoder = opus::Decoder::new(stream_config.sample_rate.0, channels)
//                 .expect("Failed to create decoder");
//
//             let mut tmp_buf: Vec<f32> = Vec::new();
//             info!("Stream has started. Press Ctrl+C to terminate.");
//             let stream = device.build_output_stream(
//                 &stream_config,
//                 move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
//                     let needed_frames = data.len();
//                     while tmp_buf.len() < needed_frames {
//                         let mut buffer = [0u8; 1024];
//                         match socket.recv(&mut buffer) {
//                             Ok(size) => {
//                                 let received_data = &buffer[..size];
//                                 let size = decoder
//                                     .decode(received_data, &mut decode_buf, false)
//                                     .expect("Failed to decode data");
//                                 let decoded_data = &decode_buf[..size * channel_count]
//                                     .iter()
//                                     .map(|&x| x as f32 / i16::MAX as f32)
//                                     .collect::<Vec<f32>>();
//                                 tmp_buf.extend_from_slice(decoded_data);
//                                 decode_buf.resize(size * channel_count, 0);
//                             }
//                             Err(e) => writeln!(std::io::stderr(), "Socket receive error: {}", e)
//                                 .expect("Failed to write to stderr"),
//                         }
//                     }
//
//                     let (frame_bytes, leftover) = tmp_buf.split_at(needed_frames);
//
//                     for (i, &sample) in frame_bytes.iter().enumerate() {
//                         data[i] = sample;
//                     }
//                     tmp_buf = leftover.to_vec();
//                 },
//                 err_fn,
//                 None,
//             )?;
//
//             stream.play()?;
//
//             loop {
//                 std::thread::sleep(std::time::Duration::from_millis(1000));
//             }
//         }
//         _ => Err(Error::UnsupportedSampleFormat(sample_format)),
//     }
// }
