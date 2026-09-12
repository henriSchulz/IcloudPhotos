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
    /// Name des CPLMaster-Records. Spart beim Full-Res-Download einen Lookup.
    #[serde(default)]
    pub master_id: Option<String>,
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
    /// Was iCloud als Medientyp meldet — `"image"` oder `"movie"`.
    ///
    /// Fehlt bei Katalogzeilen aus der Zeit vor Schema 4; für die springt
    /// [`PhotoRecord::kind`] auf die Dateiendung zurück.
    #[serde(default)]
    pub item_type: Option<String>,
}

/// Medientyp eines Assets. Entscheidet, ob der Viewer ein Bild zeigt oder
/// einen Abspieler aufbaut.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaKind {
    Image,
    Video,
}

/// Endungen, an denen ein Video auch ohne `item_type` erkennbar ist.
const VIDEO_EXTENSIONS: [&str; 5] = ["mov", "mp4", "m4v", "avi", "3gp"];

impl PhotoRecord {
    /// Bild oder Video.
    ///
    /// `item_type` ist die verlässliche Quelle, aber Bestandskataloge haben
    /// das Feld noch nicht — und ein erneuter Vollabgleich nur deshalb wäre
    /// teuer. Die Endung reicht für die Unterscheidung völlig aus.
    pub fn kind(&self) -> MediaKind {
        match self.item_type.as_deref() {
            Some("movie") => MediaKind::Video,
            Some(_) => MediaKind::Image,
            None => match extension(&self.filename) {
                Some(ext) if VIDEO_EXTENSIONS.contains(&ext.as_str()) => MediaKind::Video,
                _ => MediaKind::Image,
            },
        }
    }

    pub fn is_video(&self) -> bool {
        self.kind() == MediaKind::Video
    }

    /// Der Zeitpunkt, nach dem die Bibliothek geordnet wird.
    ///
    /// Der Aufnahmezeitpunkt, so wie es auch macOS Photos hält — wann ein Foto
    /// in die Bibliothek geraten ist, sagt über seinen Platz darin nichts.
    /// Fehlt er, bleibt nur `added_date`.
    pub fn capture_date(&self) -> DateTime<Utc> {
        self.asset_date.unwrap_or(self.added_date)
    }
}

fn extension(filename: &str) -> Option<String> {
    std::path::Path::new(filename)
        .extension()
        .map(|ext| ext.to_string_lossy().to_lowercase())
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
        let line = r#"{"id":"abc","master_id":"AdRJ","filename":"IMG_1234.HEIC","added_date":"2026-01-02T03:04:05Z","asset_date":"2025-12-24T18:00:00Z","checksum":"deadbeef","width":4032,"height":3024}"#;
        let record: PhotoRecord = serde_json::from_str(line).unwrap();
        assert_eq!(record.id, "abc");
        assert_eq!(record.master_id.as_deref(), Some("AdRJ"));
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

    fn record(filename: &str, item_type: Option<&str>) -> PhotoRecord {
        PhotoRecord {
            id: "a".into(),
            master_id: None,
            filename: filename.into(),
            added_date: DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
                .unwrap()
                .with_timezone(&Utc),
            asset_date: None,
            checksum: None,
            width: None,
            height: None,
            item_type: item_type.map(str::to_string),
        }
    }

    #[test]
    fn item_type_decides_the_media_kind() {
        assert_eq!(record("x.HEIC", Some("image")).kind(), MediaKind::Image);
        assert_eq!(record("x.MOV", Some("movie")).kind(), MediaKind::Video);
        // Fremde Werte sind jedenfalls kein Video.
        assert_eq!(record("x.HEIC", Some("bursts")).kind(), MediaKind::Image);
    }

    #[test]
    fn without_item_type_the_extension_decides() {
        // Bestandszeilen aus der Zeit vor Schema 4 haben kein item_type. Ohne
        // diesen Rueckfall waeren alle Videos im Katalog stumme Bilder.
        assert!(record("IMG_0001.MOV", None).is_video());
        assert!(record("IMG_0002.mp4", None).is_video());
        assert!(!record("IMG_0003.HEIC", None).is_video());
        assert!(!record("ohne-endung", None).is_video());
        // Kommt in echten Bibliotheken vor: doppelte Endung.
        assert!(record("IMG_0004.MP4.MOV", None).is_video());
    }

    #[test]
    fn capture_date_prefers_the_asset_date() {
        let mut rec = record("x.HEIC", None);
        assert_eq!(rec.capture_date(), rec.added_date);

        let shot = DateTime::parse_from_rfc3339("2019-10-17T09:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        rec.asset_date = Some(shot);
        assert_eq!(rec.capture_date(), shot);
    }

    #[test]
    fn optional_fields_may_be_null() {
        let line = r#"{"id":"abc","filename":"x.jpg","added_date":"2026-01-02T03:04:05Z","asset_date":null,"checksum":null,"width":null,"height":null}"#;
        let record: PhotoRecord = serde_json::from_str(line).unwrap();
        assert_eq!(record.width, None);
    }
}
