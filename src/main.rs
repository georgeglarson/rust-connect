#![warn(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

//! Rust Connect - A modern, API-first reimplementation of KDE Connect

use clap::Parser;
use rust_connect::cli::Cli;
use rust_connect::{daemon::Daemon, utils::Result};

#[tokio::main]
async fn main() -> Result<()> {
    rust_connect::init_crypto_provider();

    let cli = Cli::parse();

    // Client mode: a subcommand drives the running daemon's REST API.
    if let Some(command) = &cli.command {
        let mut out = std::io::stdout();
        if let Err(e) = rust_connect::cli::run(&cli, command, &mut out) {
            eprintln!("{e}");
            std::process::exit(e.exit_code());
        }
        return Ok(());
    }

    let daemon = Daemon::new_with_overrides(
        cli.config.as_deref(),
        cli.port,
        cli.api_port,
        cli.log_level.as_deref(),
        cli.device_name.as_deref(),
        cli.no_api,
        cli.idle_timeout,
    )
    .await?;
    daemon.run().await
}
