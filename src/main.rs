use clap::{Args, Parser, Subcommand};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{Device, FromSample, SizedSample, Stream, StreamConfig, SupportedStreamConfig};
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::str::FromStr;
use std::sync::{Arc, Mutex};

const DEFAULT_TCP_PORT: u16 = 3012;
const DEFAULT_UDP_PORT: u16 = 3013;

#[derive(Debug)]
enum Error {
    Io(std::io::Error),
    Config(cpal::DefaultStreamConfigError),
    BuildStream(cpal::BuildStreamError),
    PlayStream(cpal::PlayStreamError),
    Json(serde_json::Error),
    NotAudioDevice,
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
            let device = host.default_input_device().ok_or(Error::NotAudioDevice)?;
            let config = device.default_input_config()?;
            let sample_format = config.sample_format();
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

            let (mut tcp_stream, _client_addr) = tcp_listener.accept()?;
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
            println!("Sending config data through TCP...");
            tcp_stream.write_all(config_data.as_bytes())?; // Send serialized config data over TCP
            tcp_stream.flush()?;
            drop(tcp_stream);

            println!("Waiting for a client to start sending UDP data...");

            let mut buf = [0; 1024]; // Buffer for the initial message
            let (len, client_addr) = udp_listener.recv_from(&mut buf)?;

            println!(
                "Received {} bytes from {}, starting to send audio data...",
                len, client_addr
            );
            let err_fn = |err| eprintln!("an error occurred on the audio stream: {}", err);
            let stream = match sample_format {
                cpal::SampleFormat::I8 => create_input_stream::<i8, _>(
                    udp_listener,
                    client_addr,
                    &config.into(),
                    device,
                    err_fn,
                ),
                cpal::SampleFormat::I16 => create_input_stream::<i16, _>(
                    udp_listener,
                    client_addr,
                    &config.into(),
                    device,
                    err_fn,
                ),
                cpal::SampleFormat::I32 => create_input_stream::<i32, _>(
                    udp_listener,
                    client_addr,
                    &config.into(),
                    device,
                    err_fn,
                ),
                cpal::SampleFormat::I64 => create_input_stream::<i64, _>(
                    udp_listener,
                    client_addr,
                    &config.into(),
                    device,
                    err_fn,
                ),
                cpal::SampleFormat::F32 => create_input_stream::<f32, _>(
                    udp_listener,
                    client_addr,
                    &config.into(),
                    device,
                    err_fn,
                ),
                cpal::SampleFormat::F64 => create_input_stream::<f64, _>(
                    udp_listener,
                    client_addr,
                    &config.into(),
                    device,
                    err_fn,
                ),
                sample_format => {
                    return Err(Error::UnsupportedSampleFormat(sample_format));
                }
            }?;

            stream.play()?;
            println!("Audio stream started. Press Ctrl+C to terminate.");
            loop {
                std::thread::sleep(std::time::Duration::from_secs(3600)); // Keep running
            }
        }
        Commands::Connect(cmd) => {
            let tcp_target_ip = SocketAddr::new(cmd.ip.into(), cmd.tcp_dest_port);
            let udp_target_ip = SocketAddr::new(cmd.ip.into(), cmd.udp_dest_port);
            println!("[TCP] Connecting to {}...", tcp_target_ip);
            let mut tcp_stream =
                TcpStream::connect(tcp_target_ip).expect("Failed to connect to TCP server");
            println!("[TCP] Connected to server, receiving config data...");

            let mut buffer = Vec::new();
            tcp_stream.read_to_end(&mut buffer)?;

            // close the connection
            drop(tcp_stream);

            let buffer = String::from_utf8(buffer).expect("Invalid UTF-8 data");
            let config: Config = serde_json::from_str(&buffer)?;

            let host = cpal::default_host();
            let device = host.default_output_device().ok_or(Error::NotAudioDevice)?;
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

            match sample_format {
                cpal::SampleFormat::I8 => {
                    run::<i8>(&device, &config.into(), &socket)?;
                }
                cpal::SampleFormat::I16 => {
                    run::<i16>(&device, &config.into(), &socket)?;
                }
                cpal::SampleFormat::I32 => {
                    run::<i32>(&device, &config.into(), &socket)?;
                }
                cpal::SampleFormat::I64 => {
                    run::<i64>(&device, &config.into(), &socket)?;
                }
                cpal::SampleFormat::U8 => {
                    run::<u8>(&device, &config.into(), &socket)?;
                }
                cpal::SampleFormat::U16 => {
                    run::<u16>(&device, &config.into(), &socket)?;
                }
                cpal::SampleFormat::U32 => {
                    run::<u32>(&device, &config.into(), &socket)?;
                }
                cpal::SampleFormat::U64 => {
                    run::<u64>(&device, &config.into(), &socket)?;
                }
                cpal::SampleFormat::F32 => {
                    run::<f32>(&device, &config.into(), &socket)?;
                }
                cpal::SampleFormat::F64 => {
                    run::<f64>(&device, &config.into(), &socket)?;
                }
                _ => return Err(Error::UnsupportedSampleFormat(sample_format)),
            }
            Ok(())
        }
    }
}

fn run<T>(device: &cpal::Device, config: &StreamConfig, socket: &UdpSocket) -> Result<()>
where
    T: cpal::SizedSample + FromSample<f32> + Send + 'static,
{
    println!("Processing T: {:?}", std::any::type_name::<T>());
    let err_fn = |err| eprintln!("an error occurred on stream: {}", err);

    let socket = socket.try_clone().expect("Failed to clone socket");
    let socket = Arc::new(Mutex::new(socket));
    let channels = config.channels as usize;

    let stream = device.build_output_stream(
        config,
        move |data: &mut [T], _: &cpal::OutputCallbackInfo| {
            let socket_lock = socket.lock().unwrap();
            let mut buf = vec![0u8; std::mem::size_of_val(data)];

            match socket_lock.recv(&mut buf) {
                Ok(size) => {
                    let received_bytes = &buf[..size];
                    for (sample_chunk, output_frame) in received_bytes
                        .chunks_exact(4)
                        .zip(data.chunks_mut(channels))
                    {
                        let sample = f32::from_be_bytes(
                            sample_chunk.try_into().expect("Invalid sample size"),
                        );
                        assert!((-1.0..=1.0).contains(&sample), "Sample out of range");
                        assert_eq!(output_frame.len(), channels, "Output frame size mismatch");
                        let sample = T::from_sample(f32::from_be_bytes(
                            sample_chunk.try_into().expect("Invalid sample size"),
                        ));
                        for out in output_frame.iter_mut() {
                            *out = sample;
                        }
                    }
                }
                Err(e) => {
                    eprintln!("Socket receive error: {}", e);
                }
            }
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
fn create_input_stream<I, F>(
    socket: Arc<UdpSocket>,
    client_addr: SocketAddr,
    config: &StreamConfig,
    device: Device,
    err_fn: F,
) -> std::result::Result<Stream, cpal::BuildStreamError>
where
    F: FnMut(cpal::StreamError) + Send + 'static,
    I: SizedSample + ByteRep,
{
    println!(
        "Creating input stream in Sample Format: {:?}",
        std::any::type_name::<I>()
    );
    device.build_input_stream(
        config,
        move |data: &[I], _: &_| {
            let bytes = data
                .iter()
                .flat_map(|&x| x.to_local_be_bytes().to_vec())
                .collect::<Vec<u8>>();
            let _ = socket.send_to(&bytes, client_addr);
        },
        err_fn,
        None,
    )
}

trait ByteRep {
    fn to_local_ne_bytes(&self) -> Vec<u8>;
    fn to_local_be_bytes(&self) -> Vec<u8>;
}

impl ByteRep for i8 {
    fn to_local_ne_bytes(&self) -> Vec<u8> {
        self.to_ne_bytes().to_vec()
    }
    fn to_local_be_bytes(&self) -> Vec<u8> {
        self.to_be_bytes().to_vec()
    }
}

impl ByteRep for i16 {
    fn to_local_ne_bytes(&self) -> Vec<u8> {
        self.to_ne_bytes().to_vec()
    }
    fn to_local_be_bytes(&self) -> Vec<u8> {
        self.to_be_bytes().to_vec()
    }
}

impl ByteRep for i32 {
    fn to_local_ne_bytes(&self) -> Vec<u8> {
        self.to_ne_bytes().to_vec()
    }
    fn to_local_be_bytes(&self) -> Vec<u8> {
        self.to_be_bytes().to_vec()
    }
}

impl ByteRep for i64 {
    fn to_local_ne_bytes(&self) -> Vec<u8> {
        self.to_ne_bytes().to_vec()
    }
    fn to_local_be_bytes(&self) -> Vec<u8> {
        self.to_be_bytes().to_vec()
    }
}

impl ByteRep for f32 {
    fn to_local_ne_bytes(&self) -> Vec<u8> {
        self.to_ne_bytes().to_vec()
    }
    fn to_local_be_bytes(&self) -> Vec<u8> {
        self.to_be_bytes().to_vec()
    }
}

impl ByteRep for f64 {
    fn to_local_ne_bytes(&self) -> Vec<u8> {
        self.to_ne_bytes().to_vec()
    }
    fn to_local_be_bytes(&self) -> Vec<u8> {
        self.to_be_bytes().to_vec()
    }
}
