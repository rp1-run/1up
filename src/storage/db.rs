use std::path::Path;

use turso::{Builder, Connection, Database};

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

        let database = Builder::new_local(path_str)
            .experimental_index_method(true)
            .build()
            .await
            .map_err(|e| StorageError::Connection(e.to_string()))?;

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

        let database = Builder::new_local(path_str)
            .experimental_index_method(true)
            .build()
            .await
            .map_err(|e| StorageError::Connection(e.to_string()))?;

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
        self.database
            .connect()
            .map_err(|e| StorageError::Connection(e.to_string()).into())
    }
}
