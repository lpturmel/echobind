use clap::{Args, Parser, Subcommand};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{Device, SizedSample, Stream, StreamConfig};
use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
use std::sync::Arc;

#[derive(Debug)]
enum Error {
    Io(std::io::Error),
    Config(cpal::DefaultStreamConfigError),
    BuildStream(cpal::BuildStreamError),
    PlayStream(cpal::PlayStreamError),
    NotAudioDevice,
    UnsupportedSampleFormat(cpal::SampleFormat),
}

impl From<std::io::Error> for Error {
    fn from(err: std::io::Error) -> Self {
        Error::Io(err)
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
    #[arg(short, long = "port", default_value = "3012")]
    /// The source port to record on
    src_port: u16,
}

#[derive(Args, Debug)]
struct ConnectCmd {
    #[arg(short, long)]
    /// The destination port to connect to
    dest_port: u16,

    #[arg(short, long)]
    /// The destination IP to connect to
    ip: Ipv4Addr,
}
fn main() -> Result<()> {
    let cli = Cli::parse();
    match &cli.commands {
        Commands::Record(cmd) => {
            let host = cpal::default_host();
            let device = host.default_input_device().ok_or(Error::NotAudioDevice)?;
            let config = device.default_input_config()?;
            let sample_format = config.sample_format();
            // let sample_rate = config.sample_rate();
            // let channels = config.channels();
            println!("Recording on port {}", cmd.src_port);

            let err_fn = |err| eprintln!("an error occurred on the audio stream: {}", err);
            let addr = format!("0.0.0.0:{}", cmd.src_port);
            let addr = addr.parse().expect("Failed to parse address");
            let addr = SocketAddr::new(addr, cmd.src_port);
            let listener = Arc::new(UdpSocket::bind(addr)?);

            println!("Waiting for a client to start sending audio data...");

            let mut buf = [0; 1024]; // Buffer for the initial message
            let (len, client_addr) = listener.recv_from(&mut buf)?;

            println!(
                "Received {} bytes from {}, starting to send audio data...",
                len, client_addr
            );
            // match listener_for_client.recv_from(&mut buf) {
            //     Ok((_, src_addr)) => {
            //         println!("Connection established with {}", src_addr);
            //         let mut client_addr = client_addr_for_thread.lock().unwrap();
            //         *client_addr = Some(src_addr);
            //     }
            //     Err(e) => println!("Error receiving: {}", e),
            // }
            let stream = match sample_format {
                cpal::SampleFormat::I8 => {
                    get_stream::<i8, _>(listener, client_addr, &config.into(), device, err_fn)
                }
                cpal::SampleFormat::I16 => {
                    get_stream::<i16, _>(listener, client_addr, &config.into(), device, err_fn)
                }
                cpal::SampleFormat::I32 => {
                    get_stream::<i32, _>(listener, client_addr, &config.into(), device, err_fn)
                }
                cpal::SampleFormat::I64 => {
                    get_stream::<i64, _>(listener, client_addr, &config.into(), device, err_fn)
                }
                cpal::SampleFormat::F32 => {
                    get_stream::<f32, _>(listener, client_addr, &config.into(), device, err_fn)
                }
                cpal::SampleFormat::F64 => {
                    get_stream::<f64, _>(listener, client_addr, &config.into(), device, err_fn)
                }
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
            let target_ip = SocketAddr::new(cmd.ip.into(), cmd.dest_port);
            let socket = UdpSocket::bind(format!("0.0.0.0:{}", cmd.dest_port))?;
            socket.connect(target_ip)?;
            println!("Connecting to {}...", target_ip);
            let mut buf = [0u8; 1024];
            loop {
                let (len, addr) = socket.recv_from(&mut buf)?;
                println!("Received {} bytes from {}", len, addr);
            }
        }
    }
}

fn get_stream<I, F>(
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
    device.build_input_stream(
        config,
        move |data: &[I], _: &_| {
            let bytes = data
                .iter()
                .flat_map(|&x| x.to_local_ne_bytes().to_vec())
                .collect::<Vec<u8>>();
            let _ = socket.send_to(&bytes, client_addr);
        },
        err_fn,
        None,
    )
}

trait ByteRep {
    fn to_local_ne_bytes(&self) -> Vec<u8>;
}

impl ByteRep for i8 {
    fn to_local_ne_bytes(&self) -> Vec<u8> {
        self.to_ne_bytes().to_vec()
    }
}

impl ByteRep for i16 {
    fn to_local_ne_bytes(&self) -> Vec<u8> {
        self.to_ne_bytes().to_vec()
    }
}

impl ByteRep for i32 {
    fn to_local_ne_bytes(&self) -> Vec<u8> {
        self.to_ne_bytes().to_vec()
    }
}

impl ByteRep for i64 {
    fn to_local_ne_bytes(&self) -> Vec<u8> {
        self.to_ne_bytes().to_vec()
    }
}

impl ByteRep for f32 {
    fn to_local_ne_bytes(&self) -> Vec<u8> {
        self.to_ne_bytes().to_vec()
    }
}

impl ByteRep for f64 {
    fn to_local_ne_bytes(&self) -> Vec<u8> {
        self.to_ne_bytes().to_vec()
    }
}
