//! # Loft
//!
//! A lightweight, client-only Nix binary cache uploader for S3-compatible storage.

use anyhow::Result;
use clap::Parser;
use std::path::PathBuf;
use std::sync::Arc;
use tracing::{debug, error, info, Level};

use loft::{config, local_cache, nix_store_watcher, s3_uploader};

use config::Config;
use local_cache::LocalCache;
use nix_store_watcher::process_path; // Import process_path

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

    /// Clear the local cache.
    #[arg(long)]
    clear_cache: bool,

    /// Reset the initial scan complete flag.
    #[arg(long)]
    reset_initial_scan: bool,

    /// Force a full scan, bypassing the local cache.
    #[arg(long)]
    force_scan: bool,

    /// Populate the local cache from S3.
    #[arg(long)]
    populate_cache: bool,

    /// Manually upload a specific Nix store path. Can be specified multiple times.
    #[arg(long, value_name = "PATH")]
    upload_path: Option<Vec<PathBuf>>,
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

    // Initialize the local cache.
    let local_cache = Arc::new(LocalCache::new(&PathBuf::from(".loft_cache.db"))?);
    local_cache.initialize()?;

    if args.clear_cache {
        info!("Clearing local cache...");
        std::fs::remove_file(".loft_cache.db")?;
        info!("Local cache cleared.");
        return Ok(());
    }

    if args.reset_initial_scan {
        info!("Resetting initial scan flag...");
        local_cache.clear_scan_complete()?;
        info!("Initial scan flag reset.");
        return Ok(());
    }

    // Load the application configuration.
    let config = Config::from_file(&args.config)?;
    info!("Configuration loaded successfully.");

    // Initialize the S3 uploader.
    let uploader: Arc<s3_uploader::S3Uploader> =
        Arc::new(s3_uploader::S3Uploader::new(&config.s3).await?);
    info!("S3 uploader initialized for bucket '{}'.", config.s3.bucket);

    // Upload nix-cache-info file.
    uploader.upload_nix_cache_info().await?;

    // Handle manual path uploads
    if let Some(paths_to_upload) = args.upload_path {
        info!("Manually uploading specified paths...");
        let local_cache_clone = local_cache.clone();
        let uploader_clone = uploader.clone();
        let config_clone = config.clone();

        for path in paths_to_upload {
            info!("Processing manual upload path: {}", path.display());
            if let Err(e) = process_path(
                uploader_clone.clone(),
                local_cache_clone.clone(),
                &path,
                &config_clone,
                true, // Force scan for manual uploads
            )
            .await
            {
                error!("Failed to manually upload '{}': {:?}", path.display(), e);
            }
        }
        info!("Finished manual uploads.");
        return Ok(()); // Exit after manual uploads
    }

    if args.populate_cache {
        populate_local_cache_from_s3(uploader.clone(), local_cache.clone()).await?;
        return Ok(());
    }

    if config.loft.populate_cache_on_startup && !local_cache.is_scan_complete()? {
        info!("Populating local cache from S3 on startup...");
        populate_local_cache_from_s3(uploader.clone(), local_cache.clone()).await?;
        info!("Finished populating local cache from S3.");
    }

    info!("scan on startup: {}", config.loft.scan_on_startup);
    info!("scan already complete: {}", local_cache.is_scan_complete()?);
    if config.loft.scan_on_startup && !local_cache.is_scan_complete()? {
        // Scan existing paths and upload them.
        info!("Scanning existing store paths...");
        nix_store_watcher::scan_and_process_existing_paths(
            uploader.clone(),
            local_cache.clone(),
            &config,
            args.force_scan,
        )
        .await?;
        info!("Finished scanning existing store paths.");
        // Mark the initial scan as complete.
        local_cache.set_scan_complete()?;
    }

    // Start watching the Nix store for new paths.
    info!("Watching for new store paths...");
    nix_store_watcher::watch_store(uploader, local_cache, &config, args.force_scan).await?;

    Ok(())
}

async fn populate_local_cache_from_s3(
    uploader: Arc<s3_uploader::S3Uploader>,
    local_cache: Arc<local_cache::LocalCache>,
) -> Result<()> {
    info!("Populating local cache from S3...");
    let all_narinfo_keys = uploader.list_all_narinfo_keys().await?;
    info!("Found {} .narinfo keys in S3.", all_narinfo_keys.len());

    let mut hashes = Vec::new();
    for key in all_narinfo_keys {
        if let Some(hash) = key.strip_suffix(".narinfo") {
            // Do NOT add "sha256:" prefix here. Store as plain hash.
            hashes.push(hash.to_string());
        }
    }
    debug!(
        "Adding {} hashes to local cache: {:?}",
        hashes.len(),
        hashes
    );

    local_cache.add_many_path_hashes(&hashes)?;
    local_cache.set_scan_complete()?;
    info!("Local cache populated from S3.");

    Ok(())
}
