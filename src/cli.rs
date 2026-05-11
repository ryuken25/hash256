use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(
    name = "hash256",
    version,
    about = "HASH token PoW miner for Ethereum mainnet"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// List OpenCL devices.
    Devices,
    /// Print wallet, contract mining state, challenge, target, effective target.
    State,
    /// Benchmark configured GPU/CPU backend.
    Bench,
    /// Verify a PoW nonce against the current challenge/target.
    Verify {
        #[arg(long)]
        nonce: String,
    },
    /// Standalone mode: mine and submit with PRIVATE_KEY.
    Mine,
    /// Run central submitter/coordinator HTTP server.
    Coordinator,
    /// Run worker connected to coordinator.
    Worker,
    /// Print tx records from SQLite.
    Txs,
    /// Check RPC, OpenCL, wallet balance, gas config.
    Doctor,
}
