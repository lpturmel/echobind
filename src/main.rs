use clap::{Args, Parser, Subcommand};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{Device, SizedSample, Stream, StreamConfig};
use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
use std::sync::Arc;
// use tokio::io::Error as AsyncIoError;
// use tokio::net::UdpSocket;

#[derive(Debug)]
enum Error {
    // AsyncIo(AsyncIoError),
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
//#[tokio::main]
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
            let addr = SocketAddr::new(Ipv4Addr::LOCALHOST.into(), cmd.src_port);
            let listener = Arc::new(UdpSocket::bind(addr)?);
            let stream = match sample_format {
                cpal::SampleFormat::I8 => {
                    get_stream::<i8, _>(listener, &config.into(), device, err_fn)
                }
                cpal::SampleFormat::I16 => {
                    get_stream::<i16, _>(listener, &config.into(), device, err_fn)
                }
                cpal::SampleFormat::I32 => {
                    get_stream::<i32, _>(listener, &config.into(), device, err_fn)
                }
                cpal::SampleFormat::I64 => {
                    get_stream::<i64, _>(listener, &config.into(), device, err_fn)
                }
                cpal::SampleFormat::F32 => {
                    get_stream::<f32, _>(listener, &config.into(), device, err_fn)
                }
                cpal::SampleFormat::F64 => {
                    get_stream::<f64, _>(listener, &config.into(), device, err_fn)
                }
                sample_format => {
                    return Err(Error::UnsupportedSampleFormat(sample_format));
                }
            }?;

            stream.play()?;
            println!("Audio stream started. Press Ctrl+C to terminate.");
            loop {
                std::thread::park();
            }
        }
        Commands::Connect(cmd) => {
            let target_ip = SocketAddr::new(cmd.ip.into(), cmd.dest_port);
            println!("Connecting to {} on port {}", cmd.ip, cmd.dest_port);
            let socket = UdpSocket::bind(target_ip)?;
            let mut buf = [0u8; 1024];
            loop {
                let (len, addr) = socket.recv_from(&mut buf)?;
                println!("Received {} bytes from {}", len, addr);
            }
        }
    }
}

fn get_stream<I, F>(
    listener: Arc<UdpSocket>,
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
                .collect();
            let listener_c = Arc::clone(&listener);
            let _ = write_to_socket(listener_c, bytes);
            // tokio::spawn(write_to_socket(listener_c, bytes));
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

fn write_to_socket(socket: Arc<UdpSocket>, data: Vec<u8>) -> Result<()> {
    socket.send(&data)?;
    Ok(())
}
