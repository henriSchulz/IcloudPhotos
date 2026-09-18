//! Lokaler Foto-Katalog (SQLite) sowie Thumbnail- und Full-Res-Cache-Verwaltung.

pub mod db;
pub mod evict;
pub mod housekeeping;

pub use db::{Catalog, DateGroup, SyncState, place_cell};
pub use evict::{EvictionReport, enforce_limit};

/// Fehler aus Katalog- und Cache-Operationen.
#[derive(Debug, thiserror::Error)]
pub enum CacheError {
    #[error("Database error")]
    Db(#[from] rusqlite::Error),
    #[error("File system error at {path}")]
    Io {
        path: std::path::PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("Invalid timestamp in the database: {0:?}")]
    Timestamp(String),
}
