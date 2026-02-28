use anyhow::Result;
use chrono::{Duration, Utc};
use std::sync::Arc;
use tracing::{debug, error, info};

use crate::config::Config;
use crate::local_cache::LocalCache;
use crate::s3_uploader::{AwsDateTimeExt, S3Uploader};

pub struct Pruner {
    uploader: Arc<S3Uploader>,
    config: Arc<Config>,
    local_cache: Arc<LocalCache>,
}

impl Pruner {
    pub fn new(
        uploader: Arc<S3Uploader>,
        config: Arc<Config>,
        local_cache: Arc<LocalCache>,
    ) -> Self {
        Self {
            uploader,
            config,
            local_cache,
        }
    }

    pub async fn prune_old_objects(&self) -> Result<()> {
        let config_ref = &self.config.loft;

        let mut all_objects = Vec::new();
        let mut continuation_token: Option<String> = None;
        let mut current_bucket_size_bytes: u64 = 0;

        info!("Collecting bucket metadata in a single pass...");

        loop {
            let output = self
                .uploader
                .list_objects(continuation_token.clone())
                .await?;
            if let Some(contents) = output.contents {
                for object in contents {
                    if let (Some(key), Some(last_modified), Some(size)) =
                        (object.key, object.last_modified, object.size)
                    {
                        let size_u64 = size as u64;
                        current_bucket_size_bytes += size_u64;
                        all_objects.push((
                            key,
                            last_modified.to_chrono_utc(),
                            size_u64,
                        ));
                    }
                }
            }
            continuation_token = output.next_continuation_token;
            if continuation_token.is_none() {
                break;
            }
        }

        let mut total_pruned_count = 0;
        let mut deleted_keys = std::collections::HashSet::new();

        // --- Time-based pruning ---
        let retention_days = config_ref.prune_retention_days;
        if retention_days > 0 {
            let cutoff_date = Utc::now() - Duration::days(retention_days as i64);
            info!("Starting time-based pruning of objects older than {} days...", retention_days);

            for (key, last_modified, size) in &all_objects {
                if *last_modified < cutoff_date {
                    debug!("Pruning object by time: {} (LastModified: {})", key, last_modified);
                    match self.uploader.delete_object(key).await {
                        Ok(_) => {
                            self.remove_from_local_cache(key);
                            total_pruned_count += 1;
                            current_bucket_size_bytes -= *size;
                            deleted_keys.insert(key.clone());
                        }
                        Err(e) => {
                            error!("Failed to prune object {} by time: {:?}", key, e);
                        }
                    }
                }
            }
        }

        // --- Size-based pruning ---
        if let Some(max_size_gb) = config_ref.prune_max_size_gb {
            let max_size_bytes = max_size_gb * 1024 * 1024 * 1024;

            if current_bucket_size_bytes > max_size_bytes {
                info!(
                    "Bucket size ({} bytes) exceeds max_size_gb ({} bytes). Initiating size-based pruning.",
                    current_bucket_size_bytes, max_size_bytes
                );

                let target_percentage = config_ref.prune_target_percentage.unwrap_or(80);
                let target_size_bytes = (max_size_bytes as f64 * (target_percentage as f64 / 100.0)) as u64;

                // Filter out already deleted and sort by date (oldest first)
                let mut remaining_objects: Vec<_> = all_objects
                    .into_iter()
                    .filter(|(key, _, _)| !deleted_keys.contains(key))
                    .collect();
                
                remaining_objects.sort_by_key(|k| k.1);

                for (key, _, size) in remaining_objects {
                    if current_bucket_size_bytes <= target_size_bytes {
                        break;
                    }
                    debug!("Pruning object by size: {} (Size: {})", key, size);
                    match self.uploader.delete_object(&key).await {
                        Ok(_) => {
                            self.remove_from_local_cache(&key);
                            total_pruned_count += 1;
                            current_bucket_size_bytes -= size;
                        }
                        Err(e) => {
                            error!("Failed to prune object {} by size: {:?}", key, e);
                        }
                    }
                }
            }
        }

        info!(
            "Pruning complete. Total objects pruned: {}. Current bucket size: {} bytes",
            total_pruned_count, current_bucket_size_bytes
        );
        Ok(())
    }

    fn remove_from_local_cache(&self, key: &str) {
        if let Some(stripped) = key.strip_suffix(".narinfo") {
            let hash = stripped.strip_prefix("sha256:").unwrap_or(stripped);
            if let Err(e) = self.local_cache.remove_hash(hash) {
                error!("Failed to remove hash {} from local cache: {:?}", hash, e);
            } else {
                debug!("Removed hash {} from local cache after pruning", hash);
            }
        }
    }
}
