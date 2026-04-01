use std::path::Path;
use std::thread;
use std::time::Duration;

use turso::{Builder, Connection, Database};

use crate::shared::constants::{DB_LOCK_RETRY_ATTEMPTS, DB_LOCK_RETRY_DELAY_MS};
use crate::shared::errors::{OneupError, StorageError};

/// A wrapper around a turso database that manages connections.
pub struct Db {
    database: Database,
}

impl Db {
    /// Open a local database at the given path in read-write mode,
    /// creating the file and parent directories if they do not exist.
    pub async fn open_rw(path: &Path) -> Result<Self, OneupError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                StorageError::Connection(format!(
                    "failed to create database directory {}: {e}",
                    parent.display()
                ))
            })?;
        }

        let path_str = path.to_str().ok_or_else(|| {
            StorageError::Connection(format!(
                "database path is not valid UTF-8: {}",
                path.display()
            ))
        })?;

        let database = build_local_with_retry(path_str).await?;

        Ok(Self { database })
    }

    /// Open a local database at the given path in read-only mode.
    /// The database file must already exist.
    pub async fn open_ro(path: &Path) -> Result<Self, OneupError> {
        if !path.exists() {
            return Err(StorageError::Connection(format!(
                "database file not found: {}",
                path.display()
            ))
            .into());
        }

        let path_str = path.to_str().ok_or_else(|| {
            StorageError::Connection(format!(
                "database path is not valid UTF-8: {}",
                path.display()
            ))
        })?;

        let database = build_local_with_retry(path_str).await?;

        Ok(Self { database })
    }

    /// Open an in-memory database (useful for tests).
    #[allow(dead_code)]
    pub async fn open_memory() -> Result<Self, OneupError> {
        let database = Builder::new_local(":memory:")
            .experimental_index_method(true)
            .build()
            .await
            .map_err(|e| StorageError::Connection(e.to_string()))?;

        Ok(Self { database })
    }

    /// Create a new connection from this database handle.
    pub fn connect(&self) -> Result<Connection, OneupError> {
        let retry_delay = Duration::from_millis(DB_LOCK_RETRY_DELAY_MS);
        let mut last_error = None;

        for attempt in 0..DB_LOCK_RETRY_ATTEMPTS {
            match self.database.connect() {
                Ok(connection) => return Ok(connection),
                Err(err) => {
                    let err_text = err.to_string();
                    if !is_lock_error(&err_text) || attempt + 1 == DB_LOCK_RETRY_ATTEMPTS {
                        return Err(StorageError::Connection(err_text).into());
                    }
                    last_error = Some(err_text);
                    thread::sleep(retry_delay);
                }
            }
        }

        Err(StorageError::Connection(
            last_error.unwrap_or_else(|| "database connection failed".to_string()),
        )
        .into())
    }
}

async fn build_local_with_retry(path_str: &str) -> Result<Database, OneupError> {
    let retry_delay = Duration::from_millis(DB_LOCK_RETRY_DELAY_MS);
    let mut last_error = None;

    for attempt in 0..DB_LOCK_RETRY_ATTEMPTS {
        match Builder::new_local(path_str)
            .experimental_index_method(true)
            .build()
            .await
        {
            Ok(database) => return Ok(database),
            Err(err) => {
                let err_text = err.to_string();
                if !is_lock_error(&err_text) || attempt + 1 == DB_LOCK_RETRY_ATTEMPTS {
                    return Err(StorageError::Connection(err_text).into());
                }
                last_error = Some(err_text);
                thread::sleep(retry_delay);
            }
        }
    }

    Err(StorageError::Connection(
        last_error.unwrap_or_else(|| "database open failed".to_string()),
    )
    .into())
}

fn is_lock_error(error: &str) -> bool {
    let lower = error.to_lowercase();
    lower.contains("locking error") || lower.contains("failed locking file") || lower.contains("locked by another process")
}
