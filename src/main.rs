use clap::{Args, Parser, Subcommand};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{StreamConfig, SupportedStreamConfig};
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::str::FromStr;
use std::sync::Arc;

const DEFAULT_TCP_PORT: u16 = 3012;
const DEFAULT_UDP_PORT: u16 = 3013;

#[derive(Debug)]
enum Error {
    Io(std::io::Error),
    Config(cpal::DefaultStreamConfigError),
    BuildStream(cpal::BuildStreamError),
    PlayStream(cpal::PlayStreamError),
    Json(serde_json::Error),
    NoAudioDevice,
    UnsupportedSampleFormat(cpal::SampleFormat),
}

impl From<std::io::Error> for Error {
    fn from(err: std::io::Error) -> Self {
        Error::Io(err)
    }
}

impl From<serde_json::Error> for Error {
    fn from(value: serde_json::Error) -> Self {
        Error::Json(value)
    }
}

impl From<cpal::DefaultStreamConfigError> for Error {
    fn from(err: cpal::DefaultStreamConfigError) -> Self {
        Error::Config(err)
    }
}

impl From<cpal::BuildStreamError> for Error {
    fn from(err: cpal::BuildStreamError) -> Self {
        Error::BuildStream(err)
    }
}

impl From<cpal::PlayStreamError> for Error {
    fn from(err: cpal::PlayStreamError) -> Self {
        Error::PlayStream(err)
    }
}

type Result<T> = std::result::Result<T, Error>;

#[derive(Parser)]
#[clap(author = "Louis-Philippe Turmel", version, about, long_about = None)]
struct Cli {
    #[clap(subcommand)]
    commands: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Record system audio
    Record(RecordCmd),
    /// Connect to a remote audio server
    Connect(ConnectCmd),
}

#[derive(Args, Debug)]
struct RecordCmd {
    #[arg(short, long, default_value_t = DEFAULT_TCP_PORT)]
    /// The TCP source port to record on
    tcp_port: u16,
    #[arg(short, long, default_value_t = DEFAULT_UDP_PORT)]
    /// The UDP source port to record on
    udp_port: u16,
}

#[derive(Args, Debug)]
struct ConnectCmd {
    #[arg(short, long, default_value_t = DEFAULT_TCP_PORT)]
    /// The destination TCP port to connect to
    tcp_dest_port: u16,
    #[arg(long, default_value_t = DEFAULT_UDP_PORT)]
    /// The destination UDP port to connect to
    udp_dest_port: u16,

    #[arg(long, default_value_t = DEFAULT_UDP_PORT)]
    /// The UDP source port to use
    udp_src_port: u16,

    #[arg(short, long)]
    /// The destination IP to connect to
    ip: Ipv4Addr,
}

#[derive(Serialize, Deserialize, Debug)]
struct Config {
    sample_format: String,
    sample_rate: u32,
    channels: u16,
    buffer_size: BufferSize,
}

#[derive(Serialize, Deserialize, Debug)]
struct BufferSize {
    min: u32,
    max: u32,
}

impl From<Config> for SupportedStreamConfig {
    fn from(value: Config) -> Self {
        let BufferSize { min, max } = value.buffer_size;
        let buffer_size = cpal::SupportedBufferSize::Range { min, max };
        let sample_format = match value.sample_format.as_str() {
            "i8" => cpal::SampleFormat::I8,
            "i16" => cpal::SampleFormat::I16,
            "i32" => cpal::SampleFormat::I32,
            "i64" => cpal::SampleFormat::I64,
            "f32" => cpal::SampleFormat::F32,
            "f64" => cpal::SampleFormat::F64,
            sample_format => panic!("Invalid sample format {}", sample_format),
        };
        let sample_rate = cpal::SampleRate(value.sample_rate);
        SupportedStreamConfig::new(value.channels, sample_rate, buffer_size, sample_format)
    }
}
fn main() -> Result<()> {
    let cli = Cli::parse();
    match &cli.commands {
        Commands::Record(cmd) => {
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
                println!("Waiting for client to connect...");
                let (mut tcp_stream, client_addr) = tcp_listener.accept()?;
                println!("Client connected from {}", client_addr);
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
                println!("Sent config data to client");

                let mut buf = [0; 1024]; // Buffer for the initial message
                let (len, client_addr) = udp_listener.recv_from(&mut buf)?;
                println!(
                    "Received {} bytes from {}, starting to send audio data...",
                    len, client_addr
                );
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
                                let len = encoder
                                    .encode(&samples, &mut output)
                                    .expect("Failed to encode data");
                                output.truncate(len);
                                let _ = udp_listener.send_to(&output, client_addr);
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
                match tcp_stream.read(&mut tcp_buf) {
                    Ok(0) => {
                        println!(
                            "Client {} disconnected, stopping audio stream...",
                            client_addr
                        );
                        drop(stream);
                    }
                    Ok(e) => {
                        println!("Received {} bytes from client: {:?}", e, &tcp_buf[..e]);
                    }
                    Err(e) => {
                        eprintln!("Error reading from TCP stream: {}", e);
                    }
                }
                println!("Audio stream stopped.");
            }
        }
        Commands::Connect(cmd) => {
            let tcp_target_ip = SocketAddr::new(cmd.ip.into(), cmd.tcp_dest_port);
            let udp_target_ip = SocketAddr::new(cmd.ip.into(), cmd.udp_dest_port);
            println!("[TCP] Connecting to {}...", tcp_target_ip);
            let mut tcp_stream =
                TcpStream::connect(tcp_target_ip).expect("Failed to connect to TCP server");
            println!("[TCP] Connected to server");

            let mut length_bytes = [0u8; 4]; // Buffer for the length, assuming we use u64
            tcp_stream.read_exact(&mut length_bytes)?;
            let message_length = u32::from_be_bytes(length_bytes);

            let mut config_data_bytes = vec![0u8; message_length as usize];
            tcp_stream.read_exact(&mut config_data_bytes)?;
            let config_data = String::from_utf8(config_data_bytes).expect("Invalid config data");
            let config: Config = serde_json::from_str(&config_data)?;

            let host = cpal::default_host();
            let device = host.default_output_device().ok_or(Error::NoAudioDevice)?;
            let config = SupportedStreamConfig::from(config);
            let sample_format = config.sample_format();
            println!(
                "Client is configured to process audio: Sample format: {:?}, Sample rate: {:?}, Channels: {:?}, Buffer size: {:?}",
                sample_format,
                config.sample_rate(),
                config.channels(),
                config.buffer_size()
            );

            println!("[UDP] Binding to {}...", cmd.udp_src_port);
            let socket = UdpSocket::bind(format!("0.0.0.0:{}", cmd.udp_src_port))
                .expect("Failed to bind UDP socket");
            socket
                .connect(udp_target_ip)
                .expect("Failed to connect to UDP target");

            println!("[UDP] Sending UDP startup to {}...", udp_target_ip);
            socket.send(b"OK").expect("Failed to send UDP startup");

            let stream_config: StreamConfig = config.clone().into();
            match sample_format {
                cpal::SampleFormat::F32 => {
                    let err_fn = |err| eprintln!("an error occurred on stream: {}", err);

                    let socket = socket.try_clone().expect("Failed to clone socket");

                    let mut decode_buf = vec![0i16; 2048]; // Adjust buffer size as needed
                    let channels = count_to_channels(stream_config.channels);
                    let channel_count = stream_config.channels as usize;

                    let mut decoder = opus::Decoder::new(stream_config.sample_rate.0, channels)
                        .expect("Failed to create decoder");

                    let mut tmp_buf: Vec<f32> = Vec::new();
                    println!("Stream has started. Press Ctrl+C to terminate.");
                    let stream = device.build_output_stream(
                        &stream_config,
                        move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                            let needed_frames = data.len();
                            while tmp_buf.len() < needed_frames {
                                let mut buffer = [0u8; 2048]; // UDP receive buffer size
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
                                    Err(e) => {
                                        writeln!(std::io::stderr(), "Socket receive error: {}", e)
                                            .expect("Failed to write to stderr")
                                    }
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

                    // Keep the thread alive. You might want to handle this differently, depending on your app structure.
                    loop {
                        std::thread::sleep(std::time::Duration::from_millis(1000));
                    }
                }
                _ => Err(Error::UnsupportedSampleFormat(sample_format)),
            }
        }
    }
}

fn count_to_channels(count: u16) -> opus::Channels {
    match count {
        1 => opus::Channels::Mono,
        2 => opus::Channels::Stereo,
        _ => panic!("Unsupported channel count"),
    }
}
// fn run<T>(device: &cpal::Device, config: &StreamConfig, socket: &UdpSocket) -> Result<()>
// where
//     T: cpal::SizedSample + FromSample<f32> + Send + 'static,
// {
//     let err_fn = |err| eprintln!("an error occurred on stream: {}", err);
//
//     let socket = socket.try_clone().expect("Failed to clone socket");
//
//     let sample_size = std::mem::size_of::<T>();
//
//     let mut tmp_buf = Vec::new();
//
//     println!("Stream has started. Press Ctrl+C to terminate.");
//     let stream = device.build_output_stream(
//         config,
//         move |data: &mut [T], _: &cpal::OutputCallbackInfo| {
//             let needed_bytes = std::mem::size_of_val(data); // Calculate the total bytes needed for the current frame
//             while tmp_buf.len() < needed_bytes {
//                 let mut buf = vec![0u8; 4096];
//                 match socket.recv(&mut buf) {
//                     Ok(size) => tmp_buf.extend_from_slice(&buf[..size]),
//                     Err(e) => eprintln!("Socket receive error: {}", e),
//                 }
//             }
//             let (frame_bytes, leftover) = tmp_buf.split_at(needed_bytes);
//
//             for (i, frame_chunk) in frame_bytes.chunks_exact(sample_size).enumerate() {
//                 if let Ok(sample) = frame_chunk.try_into() {
//                     data[i] = T::from_sample(f32::from_ne_bytes(sample));
//                 }
//             }
//             tmp_buf = leftover.to_vec();
//         },
//         err_fn,
//         None,
//     )?;
//
//     stream.play()?;
//
//     // Keep the thread alive. You might want to handle this differently, depending on your app structure.
//     loop {
//         std::thread::sleep(std::time::Duration::from_millis(1000));
//     }
// }
// fn create_input_stream<I, F>(
//     socket: Arc<UdpSocket>,
//     client_addr: SocketAddr,
//     config: &StreamConfig,
//     device: &Device,
//     err_fn: F,
// ) -> std::result::Result<Stream, cpal::BuildStreamError>
// where
//     F: FnMut(cpal::StreamError) + Send + 'static,
//     I: SizedSample + ByteRep + FromSample<i16>,
//     i16: cpal::FromSample<I>,
// {
//     device.build_input_stream(
//         config,
//         move |data: &[I], _: &_| {
//             let bytes = data
//                 .iter()
//                 .flat_map(|&x| x.to_local_ne_bytes().to_vec())
//                 .collect::<Vec<u8>>();
//
//             let _ = socket.send_to(&bytes, client_addr);
//         },
//         err_fn,
//         None,
//     )
// }

// trait ByteRep {
//     fn to_local_ne_bytes(&self) -> Vec<u8>;
//     fn to_local_be_bytes(&self) -> Vec<u8>;
// }
//
// impl ByteRep for i8 {
//     fn to_local_ne_bytes(&self) -> Vec<u8> {
//         self.to_ne_bytes().to_vec()
//     }
//     fn to_local_be_bytes(&self) -> Vec<u8> {
//         self.to_be_bytes().to_vec()
//     }
// }
//
// impl ByteRep for i16 {
//     fn to_local_ne_bytes(&self) -> Vec<u8> {
//         self.to_ne_bytes().to_vec()
//     }
//     fn to_local_be_bytes(&self) -> Vec<u8> {
//         self.to_be_bytes().to_vec()
//     }
// }
//
// impl ByteRep for i32 {
//     fn to_local_ne_bytes(&self) -> Vec<u8> {
//         self.to_ne_bytes().to_vec()
//     }
//     fn to_local_be_bytes(&self) -> Vec<u8> {
//         self.to_be_bytes().to_vec()
//     }
// }
//
// impl ByteRep for i64 {
//     fn to_local_ne_bytes(&self) -> Vec<u8> {
//         self.to_ne_bytes().to_vec()
//     }
//     fn to_local_be_bytes(&self) -> Vec<u8> {
//         self.to_be_bytes().to_vec()
//     }
// }
//
// impl ByteRep for f32 {
//     fn to_local_ne_bytes(&self) -> Vec<u8> {
//         self.to_ne_bytes().to_vec()
//     }
//     fn to_local_be_bytes(&self) -> Vec<u8> {
//         self.to_be_bytes().to_vec()
//     }
// }
//
// impl ByteRep for f64 {
//     fn to_local_ne_bytes(&self) -> Vec<u8> {
//         self.to_ne_bytes().to_vec()
//     }
//     fn to_local_be_bytes(&self) -> Vec<u8> {
//         self.to_be_bytes().to_vec()
//     }
// }
