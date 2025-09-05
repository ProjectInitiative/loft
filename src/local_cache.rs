//! Manages a local redb cache of uploaded paths with full thread safety.
use anyhow::{anyhow, Result};
use redb::{Database, ReadableDatabase, ReadableTable, ReadableTableMetadata, TableDefinition};
use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;
use tracing::{debug, info};

// Table definitions
const HASHES_TABLE: TableDefinition<&str, &str> = TableDefinition::new("hashes");
const PATHS_TABLE: TableDefinition<&str, &str> = TableDefinition::new("paths");
const METADATA_TABLE: TableDefinition<&str, &str> = TableDefinition::new("metadata");

/// A thread-safe local cache to keep track of uploaded store paths.
#[derive(Clone)]
pub struct LocalCache {
    db: Arc<Database>,
}

impl LocalCache {
    /// Opens or creates a new local cache database.
    pub fn new(path: &Path) -> Result<Self> {
        let db = Database::create(path)?;
        let cache = LocalCache { db: Arc::new(db) };
        cache.initialize()?;
        Ok(cache)
    }

    /// Initializes the database, creating tables if they don't exist.
    pub fn initialize(&self) -> Result<()> {
        let write_txn = self.db.begin_write()?;
        {
            // Create tables (no-op if they already exist)
            write_txn.open_table(HASHES_TABLE)?;
            write_txn.open_table(PATHS_TABLE)?;
            write_txn.open_table(METADATA_TABLE)?;
        }
        write_txn.commit()?;
        info!("Local cache database initialized.");
        Ok(())
    }

    /// Checks if the initial scan is marked as complete.
    pub fn is_scan_complete(&self) -> Result<bool> {
        let read_txn = self.db.begin_read()?;
        let table = read_txn.open_table(METADATA_TABLE)?;
        Ok(table.get("scan_complete")?.is_some())
    }

    /// Marks the initial scan as complete.
    pub fn set_scan_complete(&self) -> Result<()> {
        let write_txn = self.db.begin_write()?;
        {
            let mut table = write_txn.open_table(METADATA_TABLE)?;
            table.insert("scan_complete", "true")?;
        }
        write_txn.commit()?;
        Ok(())
    }

    /// Clears the initial scan complete flag.
    pub fn clear_scan_complete(&self) -> Result<()> {
        let write_txn = self.db.begin_write()?;
        {
            let mut table = write_txn.open_table(METADATA_TABLE)?;
            table.remove("scan_complete")?;
        }
        write_txn.commit()?;
        Ok(())
    }

    /// Extracts the hash from a nix store path.
    /// Example: "/nix/store/87gj1r21740364x1f5n3703dq5c08z83-helix-tree-sitter-bicep" -> "87gj1r21740364x1f5n3703dq5c08z83"
    pub fn extract_hash_from_path(path: &str) -> Result<String> {
        let path = path
            .strip_prefix("/nix/store/")
            .ok_or_else(|| anyhow!("Path does not start with /nix/store/: {}", path))?;

        let hash = path
            .split('-')
            .next()
            .ok_or_else(|| anyhow!("Could not extract hash from path: {}", path))?;

        if hash.len() != 32 {
            return Err(anyhow!(
                "Hash has unexpected length (expected 32): {}",
                hash
            ));
        }

        Ok(hash.to_string())
    }

    /// Looks up the full path for a given hash.
    pub fn lookup_full_path(&self, hash: &str) -> Result<Option<String>> {
        let read_txn = self.db.begin_read()?;
        let table = read_txn.open_table(PATHS_TABLE)?;

        match table.get(hash)? {
            Some(guard) => {
                let path = guard.value().to_string();
                Ok(Some(path))
            }
            None => Ok(None),
        }
    }

    /// Adds a single path to the cache (stores both hash->existence and hash->full_path).
    pub fn add_path(&self, path: &str) -> Result<()> {
        let hash = Self::extract_hash_from_path(path)?;

        let write_txn = self.db.begin_write()?;
        {
            let mut hashes_table = write_txn.open_table(HASHES_TABLE)?;
            let mut paths_table = write_txn.open_table(PATHS_TABLE)?;

            // Store that this hash exists
            hashes_table.insert(hash.as_str(), "")?;
            // Store the full path for lookup
            paths_table.insert(hash.as_str(), path)?;
        }
        write_txn.commit()?;
        Ok(())
    }

    /// Adds a single path hash to the cache (hash only, no full path stored).
    pub fn add_path_hash(&self, hash: &str) -> Result<()> {
        let write_txn = self.db.begin_write()?;
        {
            let mut table = write_txn.open_table(HASHES_TABLE)?;
            table.insert(hash, "")?;
        }
        write_txn.commit()?;
        Ok(())
    }

    /// Checks if a single path hash exists in the cache.
    pub fn has_path_hash(&self, hash: &str) -> Result<bool> {
        let read_txn = self.db.begin_read()?;
        let table = read_txn.open_table(HASHES_TABLE)?;
        Ok(table.get(hash)?.is_some())
    }

    /// Checks if a path exists by extracting its hash.
    pub fn has_path(&self, path: &str) -> Result<bool> {
        let hash = Self::extract_hash_from_path(path)?;
        self.has_path_hash(&hash)
    }

    /// Adds multiple paths to the cache in a single transaction.
    pub fn add_many_paths(&self, paths: &[String]) -> Result<()> {
        if paths.is_empty() {
            return Ok(());
        }

        let write_txn = self.db.begin_write()?;
        {
            let mut hashes_table = write_txn.open_table(HASHES_TABLE)?;
            let mut paths_table = write_txn.open_table(PATHS_TABLE)?;

            for path in paths {
                let hash = Self::extract_hash_from_path(path)?;
                hashes_table.insert(hash.as_str(), "")?;
                paths_table.insert(hash.as_str(), path.as_str())?;
            }
        }
        write_txn.commit()?;
        Ok(())
    }

    /// Adds multiple path hashes to the cache in a single transaction.
    pub fn add_many_path_hashes(&self, hashes: &[String]) -> Result<()> {
        if hashes.is_empty() {
            return Ok(());
        }

        let write_txn = self.db.begin_write()?;
        {
            let mut table = write_txn.open_table(HASHES_TABLE)?;
            for hash in hashes {
                table.insert(hash.as_str(), "")?;
            }
        }
        write_txn.commit()?;
        Ok(())
    }

    /// Checks which of the given path hashes exist in the cache.
    pub fn find_existing_hashes(&self, hashes: &[String]) -> Result<HashSet<String>> {
        let mut existing_hashes = HashSet::new();

        if hashes.is_empty() {
            debug!("find_existing_hashes: input hashes is empty.");
            return Ok(existing_hashes);
        }

        debug!(
            "find_existing_hashes: checking for {} hashes.",
            hashes.len()
        );

        let read_txn = self.db.begin_read()?;
        let table = read_txn.open_table(HASHES_TABLE)?;

        for hash in hashes {
            if table.get(hash.as_str())?.is_some() {
                debug!("find_existing_hashes: found existing hash: {}", hash);
                existing_hashes.insert(hash.clone());
            }
        }

        debug!(
            "find_existing_hashes: found {} existing hashes.",
            existing_hashes.len()
        );
        Ok(existing_hashes)
    }

    /// Checks which of the given paths exist in the cache (by extracting their hashes).
    pub fn find_existing_paths(&self, paths: &[String]) -> Result<HashSet<String>> {
        let mut existing_paths = HashSet::new();

        if paths.is_empty() {
            debug!("find_existing_paths: input paths is empty.");
            return Ok(existing_paths);
        }

        debug!("find_existing_paths: checking for {} paths.", paths.len());

        let read_txn = self.db.begin_read()?;
        let table = read_txn.open_table(HASHES_TABLE)?;

        for path in paths {
            let hash = Self::extract_hash_from_path(path)?;
            if table.get(hash.as_str())?.is_some() {
                debug!("find_existing_paths: found existing path: {}", path);
                existing_paths.insert(path.clone());
            }
        }

        debug!(
            "find_existing_paths: found {} existing paths.",
            existing_paths.len()
        );
        Ok(existing_paths)
    }

    /// Gets the total number of cached hashes.
    pub fn count(&self) -> Result<usize> {
        let read_txn = self.db.begin_read()?;
        let table = read_txn.open_table(HASHES_TABLE)?;
        Ok(table.len()? as usize)
    }

    /// Gets all cached hashes (useful for debugging or bulk operations).
    pub fn get_all_hashes(&self) -> Result<Vec<String>> {
        let read_txn = self.db.begin_read()?;
        let table = read_txn.open_table(HASHES_TABLE)?;

        let mut hashes = Vec::new();
        for entry in table.iter()? {
            let (key, _) = entry?;
            hashes.push(key.value().to_string());
        }

        Ok(hashes)
    }

    /// Removes a hash from the cache (and its associated path if stored).
    pub fn remove_hash(&self, hash: &str) -> Result<bool> {
        let write_txn = self.db.begin_write()?;
        let existed;
        {
            let mut hashes_table = write_txn.open_table(HASHES_TABLE)?;
            let mut paths_table = write_txn.open_table(PATHS_TABLE)?;

            existed = hashes_table.remove(hash)?.is_some();
            paths_table.remove(hash)?; // Remove path mapping if it exists
        }
        write_txn.commit()?;
        Ok(existed)
    }

    /// Clears all cached data.
    pub fn clear(&self) -> Result<()> {
        let write_txn = self.db.begin_write()?;
        {
            let mut hashes_table = write_txn.open_table(HASHES_TABLE)?;
            let mut paths_table = write_txn.open_table(PATHS_TABLE)?;
            let mut metadata_table = write_txn.open_table(METADATA_TABLE)?;

            // Collect keys first, then remove them
            let hash_keys: Result<Vec<String>, _> = hashes_table
                .iter()?
                .map(|entry| entry.map(|(key, _)| key.value().to_string()))
                .collect();
            for key in hash_keys? {
                hashes_table.remove(key.as_str())?;
            }

            let path_keys: Result<Vec<String>, _> = paths_table
                .iter()?
                .map(|entry| entry.map(|(key, _)| key.value().to_string()))
                .collect();
            for key in path_keys? {
                paths_table.remove(key.as_str())?;
            }

            let metadata_keys: Result<Vec<String>, _> = metadata_table
                .iter()?
                .map(|entry| entry.map(|(key, _)| key.value().to_string()))
                .collect();
            for key in metadata_keys? {
                metadata_table.remove(key.as_str())?;
            }
        }
        write_txn.commit()?;
        Ok(())
    }
}

// Thread safety is automatic due to Arc<Database> and redb's internal thread safety
unsafe impl Send for LocalCache {}
unsafe impl Sync for LocalCache {}
