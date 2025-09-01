//! # Loft
//!
//! A lightweight, client-only Nix binary cache uploader for S3-compatible storage.

use anyhow::Result;
use clap::Parser;
use std::path::PathBuf;
use tracing::info;

mod config;
mod nix;
mod s3;
mod watcher;

use config::Config;

/// Command-line arguments for Loft.
#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Args {
    /// Path to the configuration file.
    #[arg(short, long, default_value = "loft.toml")]
    config: PathBuf,
}

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize the logging framework.
    tracing_subscriber::fmt::init();

    // Parse command-line arguments.
    let args = Args::parse();

    // Load the application configuration.
    let config = Config::from_file(&args.config)?;
    info!("Configuration loaded successfully.");

    // Initialize the S3 uploader.
    let uploader = s3::S3Uploader::new(&config.s3).await?;
    info!("S3 uploader initialized for bucket '{}'.", config.s3.bucket);

    // Start watching the Nix store for new paths.
    watcher::watch_store(uploader, config.upload_threads).await?;

    Ok(())
}

