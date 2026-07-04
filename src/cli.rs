use crate::DEFAULT_UDP_PORT;
use clap::{Args, Parser, Subcommand};
use std::net::Ipv4Addr;

#[derive(Parser)]
#[clap(author = "Louis-Philippe Turmel", version, about, long_about = None)]
pub struct Cli {
    #[clap(subcommand)]
    pub commands: Commands,
}

#[derive(Subcommand)]
pub enum Commands {
    /// Record system audio
    Record(RecordCmd),
    /// Connect to a remote audio server
    Connect(ConnectCmd),
}

#[derive(Args, Debug)]
pub struct RecordCmd {
    #[arg(short, long, default_value_t = DEFAULT_UDP_PORT)]
    /// The UDP port to record on
    pub port: u16,
}

#[derive(Args, Debug)]
pub struct ConnectCmd {
    #[arg(short, long, default_value_t = DEFAULT_UDP_PORT)]
    /// The destination UDP port to connect to
    pub dest_port: u16,

    #[arg(long, default_value_t = DEFAULT_UDP_PORT)]
    /// The UDP source port to use
    pub src_port: u16,

    #[arg(short, long)]
    /// The destination IP to connect to
    pub ip: Ipv4Addr,
}
