//! Foto-Modell: der Datensatz, den der Sidecar liefert, und die Katalogzeile.

use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Ein Metadatensatz, wie ihn der Python-Sidecar als NDJSON-Zeile ausgibt.
///
/// Enthält bewusst keine Cache-Felder — die gehören dem lokalen Katalog und
/// werden von einem Sync nie überschrieben.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PhotoRecord {
    pub id: String,
    pub filename: String,
    /// Zeitpunkt, zu dem das Asset der iCloud-Bibliothek hinzugefügt wurde.
    /// Dient als Sync-Cursor.
    pub added_date: DateTime<Utc>,
    /// Aufnahmezeitpunkt. Fehlt bei manchen Assets.
    #[serde(default)]
    pub asset_date: Option<DateTime<Utc>>,
    #[serde(default)]
    pub checksum: Option<String>,
    #[serde(default)]
    pub width: Option<u32>,
    #[serde(default)]
    pub height: Option<u32>,
}

/// Eine vollständige Zeile der Tabelle `photos`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Photo {
    pub record: PhotoRecord,
    pub thumb_path: Option<PathBuf>,
    pub thumb_bytes: Option<u64>,
    pub orig_path: Option<PathBuf>,
    pub orig_bytes: Option<u64>,
    pub is_orig_cached: bool,
    pub last_accessed_at: Option<DateTime<Utc>>,
}

impl Photo {
    /// Katalogzeile für ein frisch gesynctes Asset, noch ohne Cache-Dateien.
    pub fn new(record: PhotoRecord) -> Self {
        Self {
            record,
            thumb_path: None,
            thumb_bytes: None,
            orig_path: None,
            orig_bytes: None,
            is_orig_cached: false,
            last_accessed_at: None,
        }
    }

    pub fn id(&self) -> &str {
        &self.record.id
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_sidecar_line() {
        let line = r#"{"id":"abc","filename":"IMG_1234.HEIC","added_date":"2026-01-02T03:04:05Z","asset_date":"2025-12-24T18:00:00Z","checksum":"deadbeef","width":4032,"height":3024}"#;
        let record: PhotoRecord = serde_json::from_str(line).unwrap();
        assert_eq!(record.id, "abc");
        assert_eq!(record.width, Some(4032));
        assert!(record.asset_date.is_some());
    }

    #[test]
    fn optional_fields_may_be_absent() {
        let line = r#"{"id":"abc","filename":"x.jpg","added_date":"2026-01-02T03:04:05Z"}"#;
        let record: PhotoRecord = serde_json::from_str(line).unwrap();
        assert_eq!(record.asset_date, None);
        assert_eq!(record.checksum, None);
    }

    #[test]
    fn optional_fields_may_be_null() {
        let line = r#"{"id":"abc","filename":"x.jpg","added_date":"2026-01-02T03:04:05Z","asset_date":null,"checksum":null,"width":null,"height":null}"#;
        let record: PhotoRecord = serde_json::from_str(line).unwrap();
        assert_eq!(record.width, None);
    }
}
