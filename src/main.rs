use clap::Parser;
use cli::{Cli, Commands};
use error::Result;

pub mod cli;
pub mod commands;
pub mod config;
pub mod error;

const DEFAULT_TCP_PORT: u16 = 3012;
const DEFAULT_UDP_PORT: u16 = 3013;

fn main() -> Result<()> {
    let cli = Cli::parse();
    match &cli.commands {
        Commands::Record(cmd) => commands::record::exec(cmd)?,
        Commands::Connect(cmd) => commands::connect::exec(cmd)?,
    };
    Ok(())
}
