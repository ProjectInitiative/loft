//! # Loft
//!
//! A lightweight, client-only Nix binary cache uploader for S3-compatible storage.

use anyhow::{Context, Result};
use clap::Parser;
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use tracing::{debug, error, info, warn};

use loft::{config, local_cache, nix_store_watcher, pruner, s3_uploader};

use config::Config;
use local_cache::LocalCache;
use nix_store_watcher::process_path; // Import process_path
use pruner::Pruner;

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

    /// Manually trigger pruning of old objects.
    #[arg(long)]
    prune: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize the logging framework.
    use tracing_subscriber::prelude::*;
    let subscriber = tracing_subscriber::registry().with(console_subscriber::spawn());

    // Parse command-line arguments.
    let args = Args::parse();

    let fmt_layer = tracing_subscriber::fmt::layer();

    let filter = if args.debug {
        tracing_subscriber::EnvFilter::new("loft=debug,tower=debug")
    } else {
        tracing_subscriber::EnvFilter::new("loft=info,tower=info")
    };

    let subscriber = subscriber.with(fmt_layer.with_filter(filter));

    tracing::subscriber::set_global_default(subscriber)?;

    // Load the application configuration.
    let config = Config::from_file(&args.config)?;
    info!("Configuration loaded successfully.");

    // Ensure the parent directory for the local cache exists.
    if let Some(parent_dir) = config.loft.local_cache_path.parent() {
        fs::create_dir_all(parent_dir).with_context(|| {
            format!(
                "Failed to create parent directory for local cache at '{}'",
                parent_dir.display()
            )
        })?;
    }
    // Initialize the local cache.
    let local_cache = Arc::new(LocalCache::new(&config.loft.local_cache_path)?);

    if args.clear_cache {
        info!(
            "Clearing local cache at {}...",
            config.loft.local_cache_path.display()
        );
        std::fs::remove_file(&config.loft.local_cache_path)?; // Modify this line
        info!("Local cache cleared.");
        return Ok(());
    }

    if args.reset_initial_scan {
        info!("Resetting initial scan flag...");
        local_cache.clear_scan_complete()?;
        info!("Initial scan flag reset.");
        return Ok(());
    }

    // Initialize the S3 uploader.
    let uploader: Arc<s3_uploader::S3Uploader> =
        Arc::new(s3_uploader::S3Uploader::new(&config.s3).await?);
    info!("S3 uploader initialized for bucket '{}'.", config.s3.bucket);

    // Upload nix-cache-info file.
    uploader.upload_nix_cache_info(&config).await?;

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

    // Handle manual pruning
    if args.prune {
        info!("Manually triggering pruning...");
        let pruner = Pruner::new(uploader.clone(), Arc::new(config.clone()));
        pruner.prune_old_objects().await?;
        info!("Manual pruning complete.");
        return Ok(()); // Exit after manual pruning
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
    nix_store_watcher::watch_store(
        uploader.clone(),
        local_cache.clone(),
        &config,
        args.force_scan,
    )
    .await?;

    // Start pruning task if enabled
    if config.loft.prune_enabled {
        if let Some(schedule_str) = config.loft.prune_schedule.clone() {
            match parse_duration_string(&schedule_str) {
                Ok(duration) => {
                    info!("Starting pruning task with schedule: {:?}", duration);
                    let pruner = Pruner::new(uploader.clone(), Arc::new(config.clone()));
                    tokio::spawn(async move {
                        loop {
                            tokio::time::sleep(duration).await;
                            info!("Running scheduled pruning job...");
                            if let Err(e) = pruner.prune_old_objects().await {
                                error!("Error running scheduled pruning job: {:?}", e);
                            }
                        }
                    });
                }
                Err(e) => {
                    error!("Invalid prune_schedule in config: {:?}", e);
                }
            }
        } else {
            warn!("Pruning is enabled but prune_schedule is not set in config.");
        }
    }

    Ok(())
}

/// Parses a duration string (e.g., "1h", "24h", "7d") into a tokio::time::Duration.
fn parse_duration_string(s: &str) -> Result<tokio::time::Duration> {
    let s = s.trim();
    let (num_str, unit_str) = s.split_at(s.len() - 1);
    let num: u64 = num_str.parse().context("Invalid duration number")?;

    match unit_str {
        "h" => Ok(tokio::time::Duration::from_secs(num * 3600)),
        "d" => Ok(tokio::time::Duration::from_secs(num * 3600 * 24)),
        "m" => Ok(tokio::time::Duration::from_secs(num * 60)),
        "s" => Ok(tokio::time::Duration::from_secs(num)),
        _ => Err(anyhow::anyhow!("Invalid duration unit: {}", unit_str)),
    }
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
