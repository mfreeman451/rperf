// src/lib.rs
extern crate log;

pub mod protocol;
pub mod stream;
pub mod utils;
pub mod client;
pub mod server;

use clap::ArgMatches;
use std::error::Error;

type BoxResult<T> = Result<T, Box<dyn Error>>;

// Public API for running the client
pub fn run_client(args: ArgMatches) -> BoxResult<()> {
    client::execute(args)
}

// Public API for running the server
pub fn run_server(args: ArgMatches) -> BoxResult<()> {
    server::serve(args)
}

// Re-export key types for convenience
pub use protocol::results::{TestResults, TcpTestResults, UdpTestResults};
pub use stream::{TestStream, tcp, udp};