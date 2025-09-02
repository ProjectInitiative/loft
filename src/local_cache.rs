//! Manages a local SQLite cache of uploaded paths.

use anyhow::Result;
use rusqlite::{Connection, params, ToSql};
use std::path::Path;
use std::sync::Mutex;
use tracing::{info,debug};

/// A local cache to keep track of uploaded store paths.
pub struct LocalCache {
    conn: Mutex<Connection>,
}

impl LocalCache {
    /// Opens or creates a new local cache database.
    pub fn new(path: &Path) -> Result<Self> {
        let conn = Connection::open(path)?;
        Ok(LocalCache { conn: Mutex::new(conn) })
    }

    /// Initializes the database, creating tables if they don't exist.
    pub fn initialize(&self) -> Result<()> {

        let conn = self.conn.lock().unwrap();
        conn.execute(
            "CREATE TABLE IF NOT EXISTS uploaded_paths (
                hash TEXT PRIMARY KEY
            )",
            [],
        )?;
        conn.execute(
            "CREATE TABLE IF NOT EXISTS metadata (
                key TEXT PRIMARY KEY,
                value TEXT
            )",
            [],
        )?;
        info!("Local cache database initialized.");
        Ok(())
    }

    /// Checks if the initial scan is marked as complete.
    pub fn is_scan_complete(&self) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT value FROM metadata WHERE key = 'scan_complete'")?;
        let mut rows = stmt.query([])?;
        if let Some(row) = rows.next()? {
            let value: String = row.get(0)?;
            Ok(value == "true")
        } else {
            Ok(false)
        }
    }

    /// Marks the initial scan as complete.
    pub fn set_scan_complete(&self) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO metadata (key, value) VALUES ('scan_complete', 'true')",
            [],
        )?;
        Ok(())
    }

    /// Clears the initial scan complete flag.
    pub fn clear_scan_complete(&self) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute("DELETE FROM metadata WHERE key = 'scan_complete'", [])?;
        Ok(())
    }

    /// Adds a single path hash to the cache.
    pub fn add_path_hash(&self, hash: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR IGNORE INTO uploaded_paths (hash) VALUES (?1)",
            params![hash],
        )?;
        Ok(())
    }

    /// Checks if a single path hash exists in the cache.
    pub fn has_path_hash(&self, hash: &str) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT 1 FROM uploaded_paths WHERE hash = ?1")?;
        let mut rows = stmt.query(params![hash])?;
        Ok(rows.next()?.is_some())
    }

    /// Adds multiple path hashes to the cache in a transaction.
    pub fn add_many_path_hashes(&self, hashes: &[String]) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let tx = conn.unchecked_transaction()?;
        for hash in hashes {
            tx.execute(
                "INSERT OR IGNORE INTO uploaded_paths (hash) VALUES (?1)",
                params![hash],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Checks which of the given path hashes exist in the cache.
    pub fn find_existing_hashes(&self, hashes: &[String]) -> Result<std::collections::HashSet<String>> {
        let conn = self.conn.lock().unwrap();
        let mut existing_hashes = std::collections::HashSet::new();
        if hashes.is_empty() {
            debug!("find_existing_hashes: input hashes is empty.");
            return Ok(existing_hashes);
        }

        debug!("find_existing_hashes: checking for {} hashes.", hashes.len());

        // SQLite's WHERE IN clause has a limit on the number of parameters (typically 999).
        // We'll split the query into chunks to avoid exceeding this limit.
        const SQLITE_MAX_VARIABLE_NUMBER: usize = 900; // A safe limit

        for chunk in hashes.chunks(SQLITE_MAX_VARIABLE_NUMBER) {
            let query_string = format!(
                "SELECT hash FROM uploaded_paths WHERE hash IN ({})",
                chunk.iter().map(|_| "?").collect::<Vec<_>>().join(",")
            );
            debug!("find_existing_hashes: query string: {}", query_string);

            let mut stmt = conn.prepare(&query_string)?;

            let params: Vec<&dyn rusqlite::ToSql> = chunk
                .iter()
                .map(|s| s as &dyn rusqlite::ToSql)
                .collect();
            let mut rows = stmt.query(&*params)?;

            while let Some(row) = rows.next()? {
                let hash: String = row.get(0)?;
                debug!("find_existing_hashes: found existing hash: {}", hash);
                existing_hashes.insert(hash);
            }
        }

        debug!("find_existing_hashes: found {} existing hashes.", existing_hashes.len());
        Ok(existing_hashes)
    }
}
