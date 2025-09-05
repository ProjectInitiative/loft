//! Handles application configuration.

use anyhow::{Context, Result};
use serde::Deserialize;
use std::fs;
use std::path::{Path, PathBuf};

/// Main configuration for the application.
#[derive(Deserialize, Debug, Clone)]
pub struct Config {
    /// S3 storage configuration.
    pub s3: S3Config,
    /// Loft-specific configuration.
    pub loft: LoftConfig,
}

/// Compression algorithm to use.
#[derive(Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Compression {
    Xz,
    Zstd,
}

/// Loft-specific configuration.
#[derive(Deserialize, Debug, Clone)]
pub struct LoftConfig {
    /// The number of concurrent uploads to perform.
    #[serde(default = "default_upload_threads")]
    pub upload_threads: usize,
    /// Whether to perform an initial scan of the store on startup.
    #[serde(default)]
    pub scan_on_startup: bool,
    /// Whether to populate the local cache from S3 on startup if the cache is empty.
    #[serde(default)]
    pub populate_cache_on_startup: bool,
    /// Optional: Path to your Nix signing key file (e.g., /etc/nix/signing-key.sec)
    /// If provided, uploaded paths will be signed.
    pub signing_key_path: Option<PathBuf>,
    /// Optional: Name of your Nix signing key (e.g., "cache.example.org-1")
    /// Required if signing_key_path is provided.
    pub signing_key_name: Option<String>,
    /// Optional: List of public keys whose signed paths should be skipped for upload.
    /// If a path is signed by any of these keys, it will not be uploaded.
    pub skip_signed_by_keys: Option<Vec<String>>,
    /// Optional: Use disk for large NARs instead of memory.
    #[serde(default)]
    pub use_disk_for_large_nars: bool,
    /// Optional: The threshold in MB for what is considered a large NAR.
    #[serde(default = "default_large_nar_threshold_mb")]
    pub large_nar_threshold_mb: u64,
    /// The compression algorithm to use.
    #[serde(default = "default_compression")]
    pub compression: Compression,
}

/// S3-specific configuration.
#[derive(Deserialize, Debug, Clone)]
pub struct S3Config {
    pub bucket: String,
    pub region: String,
    pub endpoint: String,
    pub access_key: Option<String>,
    pub secret_key: Option<String>,
}

/// Sets the default number of upload threads if not specified.
fn default_upload_threads() -> usize {
    4
}

/// Sets the default NAR size threshold in MB.
fn default_large_nar_threshold_mb() -> u64 {
    1024 // 1GB
}

/// Sets the default compression algorithm.
fn default_compression() -> Compression {
    Compression::Zstd
}

impl Config {
    /// Loads configuration from a TOML file.
    pub fn from_file(path: &Path) -> Result<Self> {
        let contents = fs::read_to_string(path)
            .with_context(|| format!("Failed to read configuration file at {:?}", path))?;
        let config: Config = toml::from_str(&contents)
            .with_context(|| "Failed to parse TOML configuration")?;
        Ok(config)
    }
}

