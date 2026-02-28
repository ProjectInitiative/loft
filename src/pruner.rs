use anyhow::Result;
use chrono::{Duration, Utc};
use tracing::{info, debug, error};
use std::sync::Arc;

use crate::s3_uploader::{S3Uploader, AwsDateTimeExt};
use crate::config::Config;

pub struct Pruner {
    uploader: Arc<S3Uploader>,
    config: Arc<Config>,
}

impl Pruner {
    pub fn new(uploader: Arc<S3Uploader>, config: Arc<Config>) -> Self {
        Self { uploader, config }
    }

    pub async fn prune_old_objects(&self) -> Result<()> {
        let config_ref = &self.config.loft;

        let mut total_pruned_count = 0;

        // --- Size-based pruning ---
        if let Some(max_size_gb) = config_ref.prune_max_size_gb {
            let max_size_bytes = max_size_gb * 1024 * 1024 * 1024; // Convert GB to bytes
            let mut current_bucket_size_bytes = self.uploader.get_bucket_size().await?; // Declared and assigned here

            if current_bucket_size_bytes > max_size_bytes {
                info!(
                    "Bucket size ({} bytes) exceeds max_size_gb ({} bytes). Initiating size-based pruning.",
                    current_bucket_size_bytes, max_size_bytes
                );

                let target_percentage = config_ref.prune_target_percentage.unwrap_or(80); // Default to 80%
                let target_size_bytes = (max_size_bytes as f64 * (target_percentage as f64 / 100.0)) as u64;

                let mut objects_to_prune = Vec::new();
                let mut continuation_token: Option<String> = None;

                // Collect all objects with their last modified dates and sizes
                loop {
                    let output = self.uploader.list_objects(continuation_token.clone()).await?;
                    if let Some(contents) = output.contents {
                        for object in contents {
                            if let (Some(key), Some(last_modified), Some(size)) = (object.key, object.last_modified, object.size) {
                                objects_to_prune.push((key, last_modified.to_chrono_utc(), size as u64));
                            }
                        }
                    }
                    continuation_token = output.next_continuation_token;
                    if continuation_token.is_none() {
                        break;
                    }
                }

                // Sort objects by last modified date (oldest first)
                objects_to_prune.sort_by_key(|k| k.1);

                // Delete oldest objects until target size is reached
                for (key, _last_modified, size) in objects_to_prune {
                    if current_bucket_size_bytes <= target_size_bytes {
                        break; // Reached target size
                    }
                    debug!("Pruning object by size: {} (Size: {})", key, size);
                    match self.uploader.delete_object(&key).await {
                        Ok(_) => {
                            total_pruned_count += 1;
                            current_bucket_size_bytes -= size;
                        },
                        Err(e) => {
                            error!("Failed to prune object {} by size: {:?}", key, e);
                        }
                    }
                }
                info!("Size-based pruning complete. Current bucket size: {} bytes", current_bucket_size_bytes);
            } else {
                info!("Bucket size ({} bytes) is within max_size_gb ({} bytes). No size-based pruning needed.", current_bucket_size_bytes, max_size_bytes);
            }
        }

        // --- Time-based pruning (only if not already pruned by size or if size-based is disabled) ---
        let retention_days = config_ref.prune_retention_days;
        if retention_days > 0 {
            info!("Starting time-based pruning of objects older than {} days...", retention_days);
            let cutoff_date = Utc::now() - Duration::days(retention_days as i64);

            let mut continuation_token: Option<String> = None;
            let mut time_based_pruned_count = 0;

            loop {
                let output = self.uploader.list_objects(continuation_token.clone()).await?;

                if let Some(contents) = output.contents {
                    for object in contents {
                        if let (Some(key), Some(last_modified)) = (object.key, object.last_modified) {
                            let last_modified_utc = last_modified.to_chrono_utc();
                            if last_modified_utc < cutoff_date {
                                debug!("Pruning object by time: {} (LastModified: {})", key, last_modified_utc);
                                match self.uploader.delete_object(&key).await {
                                    Ok(_) => {
                                        time_based_pruned_count += 1;
                                    },
                                    Err(e) => {
                                        error!("Failed to prune object {} by time: {:?}", key, e);
                                    }
                                }
                            }
                        }
                    }
                }

                continuation_token = output.next_continuation_token;

                if continuation_token.is_none() {
                    break;
                }
            }
            info!("Time-based pruning complete. Objects pruned by time: {}", time_based_pruned_count);
            total_pruned_count += time_based_pruned_count;
        } else {
            info!("Time-based pruning is disabled (prune_retention_days is 0).");
        }

        info!("Pruning complete. Total objects pruned: {}", total_pruned_count);
        Ok(())
    }
}
