//! Handles all interactions with S3-compatible storage.

use anyhow::Result;
use aws_config::defaults;
use aws_config::BehaviorVersion;
use aws_sdk_s3::config::{Credentials, Region};
use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::Client;
use std::path::Path;
use tracing::info;

use crate::config::S3Config;

/// A client for uploading files to an S3 bucket.
pub struct S3Uploader {
    client: Client,
    bucket: String,
}

impl S3Uploader {
    /// Creates a new S3 uploader from the given configuration.
    pub async fn new(config: &S3Config) -> Result<Self> {
        let access_key = config.access_key.clone().or_else(|| std::env::var("AWS_ACCESS_KEY_ID").ok());
        let secret_key = config.secret_key.clone().or_else(|| std::env::var("AWS_SECRET_ACCESS_KEY").ok());

        let (access_key, secret_key) = match (access_key, secret_key) {
            (Some(ak), Some(sk)) => (ak, sk),
            _ => return Err(anyhow::anyhow!("AWS access key and secret key must be provided either in config or as environment variables (AWS_ACCESS_KEY_ID, AWS_SECRET_ACCESS_KEY)")),
        };

        let config_loader = defaults(BehaviorVersion::latest())
            .endpoint_url(config.endpoint.as_str())
            .region(Region::new(config.region.clone()))
            .credentials_provider(Credentials::new(
                &access_key,
                &secret_key,
                None,
                None,
                "default",
            ));

        let sdk_config = config_loader.load().await;

        let s3_config = aws_sdk_s3::config::Builder::from(&sdk_config)
            .force_path_style(true)
            .build();

        let client = Client::from_conf(s3_config);

        Ok(S3Uploader {
            client,
            bucket: config.bucket.clone(),
        })
    }

    /// Checks if a list of store paths already exist in the cache.
    ///
    /// This is a client-side adaptation of the server's "get-missing-paths" endpoint.
    pub async fn check_paths_exist(&self, store_paths: &[String]) -> Result<Vec<String>> {
        let mut missing_paths = Vec::new();

        for path in store_paths {
            let store_hash = Path::new(path)
                .file_name()
                .and_then(|s| s.to_str())
                .map(|s| s.split('-').next().unwrap_or(""))
                .unwrap_or("");

            let key = format!("{}.narinfo", store_hash);

            match self
                .client
                .head_object()
                .bucket(&self.bucket)
                .key(&key)
                .send()
                .await
            {
                Ok(_) => {
                    info!("'{}' already exists in the cache. Skipping.", path);
                }
                Err(_) => {
                    missing_paths.push(path.clone());
                }
            }
        }
        Ok(missing_paths)
    }

    /// Uploads a file to the S3 bucket.
    pub async fn upload_file(&self, file_path: &Path, key: &str) -> Result<()> {
        let stream = ByteStream::from_path(file_path).await?;
        self.client
            .put_object()
            .bucket(&self.bucket)
            .key(key)
            .body(stream)
            .send()
            .await?;
        info!("Successfully uploaded '{}' to '{}'.", file_path.display(), key);
        Ok(())
    }

    /// Uploads bytes to the S3 bucket.
    pub async fn upload_bytes(&self, bytes: Vec<u8>, key: &str) -> Result<()> {
        let stream = ByteStream::from(bytes);
        self.client
            .put_object()
            .bucket(&self.bucket)
            .key(key)
            .body(stream)
            .send()
            .await?;
        info!("Successfully uploaded bytes to '{}'.", key);
        Ok(())
    }
}

