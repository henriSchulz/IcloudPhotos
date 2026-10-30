//! Lokaler Foto-Katalog (SQLite) sowie Thumbnail- und Full-Res-Cache-Verwaltung.

pub mod albums;
pub mod db;
pub mod evict;
pub mod home;
pub mod housekeeping;
pub mod located;

pub use albums::{Album, AlbumContent, Scope};
pub use db::{Catalog, DateGroup, SyncState, place_cell};
pub use evict::{EvictionReport, enforce_limit};
pub use home::{Day, DayGroup, Memory, Summary};
pub use located::LocatedPhoto;

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
