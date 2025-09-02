//! # Loft
//!
//! A lightweight, client-only Nix binary cache uploader for S3-compatible storage.

use anyhow::Result;
use clap::Parser;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tracing::{info, Level};

mod config;
mod nix_store_watcher;
mod nix_utils;
mod s3_uploader;
mod nix_manifest;

use config::Config;

/// Command-line arguments for Loft.
#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Args {
    /// Path to the configuration file.
    #[arg(short, long, default_value = "loft.toml")]
    config: PathBuf,

    /// Enable debug logging.
    #[arg(long)]
    debug: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    // Parse command-line arguments.
    let args = Args::parse();

    // Initialize the logging framework.
    let subscriber = tracing_subscriber::fmt();

    if args.debug {
        subscriber.with_max_level(Level::DEBUG).init();
    } else {
        subscriber.with_max_level(Level::INFO).init(); // Default to INFO
    }

    // Load the application configuration.
    let config = Config::from_file(&args.config)?;
    info!("Configuration loaded successfully.");

    // Initialize the S3 uploader.
    let uploader = Arc::new(s3_uploader::S3Uploader::new(&config.s3).await?);
    info!(
        "S3 uploader initialized for bucket '{}'.",
        config.s3.bucket
    );

    let marker_file = Path::new(".loft_scan_complete");
    info!("scan on startup: {}", config.loft.scan_on_startup);
    info!("marker file exists: {}", marker_file.exists());
    if config.loft.scan_on_startup && !marker_file.exists() {
        // Scan existing paths and upload them.
        info!("Scanning existing store paths...");
        nix_store_watcher::scan_and_process_existing_paths(
            uploader.clone(),
            &config,
        )
        .await?;
        info!("Finished scanning existing store paths.");
        // Create the marker file to indicate that the initial scan is complete.
        fs::File::create(marker_file)?;
    }

    // Start watching the Nix store for new paths.
    info!("Watching for new store paths...");
    nix_store_watcher::watch_store(uploader, &config).await?;

    Ok(())
}

