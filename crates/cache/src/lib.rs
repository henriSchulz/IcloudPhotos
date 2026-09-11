//! Lokaler Foto-Katalog (SQLite) sowie Thumbnail- und Full-Res-Cache-Verwaltung.

pub mod db;
pub mod evict;
pub mod housekeeping;

pub use db::Catalog;
pub use evict::{EvictionReport, enforce_limit};

/// Fehler aus Katalog- und Cache-Operationen.
#[derive(Debug, thiserror::Error)]
pub enum CacheError {
    #[error("Datenbankfehler")]
    Db(#[from] rusqlite::Error),
    #[error("Dateisystemfehler bei {path}")]
    Io {
        path: std::path::PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("Ungültiger Zeitstempel in der Datenbank: {0:?}")]
    Timestamp(String),
}
