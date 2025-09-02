//! Handles application configuration.

use anyhow::{Context, Result};
use serde::Deserialize;
use std::fs;
use std::path::Path;

/// Main configuration for the application.
#[derive(Deserialize, Debug)]
pub struct Config {
    /// S3 storage configuration.
    pub s3: S3Config,
    /// The number of concurrent uploads to perform.
    #[serde(default = "default_upload_threads")]
    pub upload_threads: usize,
    /// Whether to perform an initial scan of the store on startup.
    #[serde(default)]
    pub scan_on_startup: bool,
}

/// S3-specific configuration.
#[derive(Deserialize, Debug)]
pub struct S3Config {
    pub bucket: String,
    pub region: String,
    pub endpoint: String,
    pub access_key: String,
    pub secret_key: String,
}

/// Sets the default number of upload threads if not specified.
fn default_upload_threads() -> usize {
    4
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

