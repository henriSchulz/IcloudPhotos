//! Katalog-Zugriff. Besitzt das SQLite-Schema und alle Zeilenoperationen.

use std::path::{Path, PathBuf};

use chrono::{DateTime, SecondsFormat, Utc};
use icloud_photos_core::model::{Photo, PhotoRecord};
use rusqlite::{Connection, OptionalExtension, Row, params};

use crate::CacheError;

const SCHEMA_VERSION: i32 = 1;

/// Der lokale Foto-Katalog.
pub struct Catalog {
    conn: Connection,
}

impl Catalog {
    /// Öffnet den Katalog und legt Datei samt Schema bei Bedarf an.
    pub fn open(path: &Path) -> Result<Self, CacheError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|source| CacheError::Io {
                path: parent.to_path_buf(),
                source,
            })?;
        }
        Self::from_connection(Connection::open(path)?)
    }

    /// Katalog im Arbeitsspeicher — für Tests.
    pub fn open_in_memory() -> Result<Self, CacheError> {
        Self::from_connection(Connection::open_in_memory()?)
    }

    fn from_connection(conn: Connection) -> Result<Self, CacheError> {
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "foreign_keys", true)?;
        let catalog = Self { conn };
        catalog.migrate()?;
        Ok(catalog)
    }

    fn migrate(&self) -> Result<(), CacheError> {
        let version: i32 = self
            .conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))?;
        if version >= SCHEMA_VERSION {
            return Ok(());
        }
        self.conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS photos (
                id               TEXT PRIMARY KEY,
                filename         TEXT NOT NULL,
                added_date       TEXT NOT NULL,
                asset_date       TEXT,
                checksum         TEXT,
                width            INTEGER,
                height           INTEGER,
                thumb_path       TEXT,
                thumb_bytes      INTEGER,
                orig_path        TEXT,
                orig_bytes       INTEGER,
                is_orig_cached   INTEGER NOT NULL DEFAULT 0,
                last_accessed_at TEXT
            );
            CREATE INDEX IF NOT EXISTS idx_photos_last_accessed
                ON photos(last_accessed_at);
            CREATE INDEX IF NOT EXISTS idx_photos_added_date
                ON photos(added_date);
            -- Beschleunigt Summenbildung und Eviction-Auswahl, die beide nur
            -- gecachte Originale betrachten.
            CREATE INDEX IF NOT EXISTS idx_photos_cached_lru
                ON photos(last_accessed_at) WHERE is_orig_cached = 1;
            "#,
        )?;
        self.conn
            .pragma_update(None, "user_version", SCHEMA_VERSION)?;
        Ok(())
    }

    /// Schreibt Metadaten aus einem Sync. Cache-Spalten bleiben unangetastet,
    /// damit ein Re-Sync keine bereits geladenen Dateien vergisst.
    pub fn upsert_record(&self, record: &PhotoRecord) -> Result<(), CacheError> {
        self.conn.execute(
            r#"
            INSERT INTO photos (id, filename, added_date, asset_date, checksum, width, height)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
            ON CONFLICT(id) DO UPDATE SET
                filename   = excluded.filename,
                added_date = excluded.added_date,
                asset_date = excluded.asset_date,
                checksum   = excluded.checksum,
                width      = excluded.width,
                height     = excluded.height
            "#,
            params![
                record.id,
                record.filename,
                encode_ts(record.added_date),
                record.asset_date.map(encode_ts),
                record.checksum,
                record.width,
                record.height,
            ],
        )?;
        Ok(())
    }

    /// Schreibt viele Datensätze in einer Transaktion.
    pub fn upsert_batch(&mut self, records: &[PhotoRecord]) -> Result<(), CacheError> {
        let tx = self.conn.transaction()?;
        {
            let mut stmt = tx.prepare(
                r#"
                INSERT INTO photos (id, filename, added_date, asset_date, checksum, width, height)
                VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                ON CONFLICT(id) DO UPDATE SET
                    filename   = excluded.filename,
                    added_date = excluded.added_date,
                    asset_date = excluded.asset_date,
                    checksum   = excluded.checksum,
                    width      = excluded.width,
                    height     = excluded.height
                "#,
            )?;
            for record in records {
                stmt.execute(params![
                    record.id,
                    record.filename,
                    encode_ts(record.added_date),
                    record.asset_date.map(encode_ts),
                    record.checksum,
                    record.width,
                    record.height,
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Jüngstes bekanntes `added_date` — der Cursor für den Delta-Sync.
    pub fn sync_cursor(&self) -> Result<Option<DateTime<Utc>>, CacheError> {
        let raw: Option<String> = self
            .conn
            .query_row("SELECT MAX(added_date) FROM photos", [], |row| row.get(0))
            .optional()?
            .flatten();
        raw.map(|s| decode_ts(&s)).transpose()
    }

    pub fn count(&self) -> Result<u64, CacheError> {
        let count: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM photos", [], |row| row.get(0))?;
        Ok(from_sql_bytes(count))
    }

    /// Eine Seite des Katalogs, neueste zuerst — für die virtualisierte Grid-View.
    pub fn page(&self, offset: u64, limit: u64) -> Result<Vec<Photo>, CacheError> {
        let mut stmt = self
            .conn
            .prepare("SELECT * FROM photos ORDER BY added_date DESC, id DESC LIMIT ?1 OFFSET ?2")?;
        let rows = stmt.query_map(
            params![to_sql_bytes(limit), to_sql_bytes(offset)],
            row_to_photo,
        )?;
        rows.collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .collect::<Result<Vec<_>, _>>()
    }

    pub fn get(&self, id: &str) -> Result<Option<Photo>, CacheError> {
        let row = self
            .conn
            .query_row(
                "SELECT * FROM photos WHERE id = ?1",
                params![id],
                row_to_photo,
            )
            .optional()?;
        row.transpose()
    }

    /// Markiert ein Foto als zuletzt benutzt.
    ///
    /// Muss auch beim reinen Ansehen eines bereits gecachten Fotos aufgerufen
    /// werden — sonst verhält sich der Cache wie FIFO statt echtem LRU.
    pub fn touch(&self, id: &str, now: DateTime<Utc>) -> Result<(), CacheError> {
        self.conn.execute(
            "UPDATE photos SET last_accessed_at = ?2 WHERE id = ?1",
            params![id, encode_ts(now)],
        )?;
        Ok(())
    }

    pub fn set_thumbnail(&self, id: &str, path: &Path, bytes: u64) -> Result<(), CacheError> {
        self.conn.execute(
            "UPDATE photos SET thumb_path = ?2, thumb_bytes = ?3 WHERE id = ?1",
            params![id, path.to_string_lossy(), to_sql_bytes(bytes)],
        )?;
        Ok(())
    }

    /// Vermerkt ein frisch heruntergeladenes Original und zählt es als benutzt.
    pub fn set_original_cached(
        &self,
        id: &str,
        path: &Path,
        bytes: u64,
        now: DateTime<Utc>,
    ) -> Result<(), CacheError> {
        self.conn.execute(
            r#"
            UPDATE photos
               SET orig_path = ?2, orig_bytes = ?3, is_orig_cached = 1, last_accessed_at = ?4
             WHERE id = ?1
            "#,
            params![
                id,
                path.to_string_lossy(),
                to_sql_bytes(bytes),
                encode_ts(now)
            ],
        )?;
        Ok(())
    }

    /// Nimmt ein Original aus dem Cache-Bestand. `last_accessed_at` bleibt
    /// erhalten, damit die Zugriffshistorie eines Fotos nicht verloren geht.
    pub fn clear_original(&self, id: &str) -> Result<(), CacheError> {
        self.conn.execute(
            r#"
            UPDATE photos
               SET orig_path = NULL, orig_bytes = NULL, is_orig_cached = 0
             WHERE id = ?1
            "#,
            params![id],
        )?;
        Ok(())
    }

    /// Summe aller gecachten Originale in Bytes.
    pub fn cached_bytes(&self) -> Result<u64, CacheError> {
        let sum: i64 = self.conn.query_row(
            "SELECT COALESCE(SUM(orig_bytes), 0) FROM photos WHERE is_orig_cached = 1",
            [],
            |row| row.get(0),
        )?;
        Ok(from_sql_bytes(sum))
    }

    /// IDs von Fotos ohne Vorschau, neueste zuerst.
    ///
    /// Der Delta-Sync sieht ältere Assets nicht erneut; fehlt dort eine
    /// Vorschau, muss sie gezielt nachgeholt werden.
    pub fn ids_without_thumbnail(&self, limit: u64) -> Result<Vec<String>, CacheError> {
        let mut stmt = self.conn.prepare(
            r#"
            SELECT id
              FROM photos
             WHERE thumb_path IS NULL
             ORDER BY added_date DESC
             LIMIT ?1
            "#,
        )?;
        let rows = stmt.query_map(params![to_sql_bytes(limit)], |row| row.get(0))?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// Gecachte Originale, am längsten ungenutzte zuerst.
    ///
    /// SQLite sortiert `NULL` bei `ASC` an den Anfang — nie geöffnete Fotos
    /// fliegen also zuerst raus. Das ist beabsichtigt.
    pub fn lru_candidates(&self) -> Result<Vec<CachedOriginal>, CacheError> {
        let mut stmt = self.conn.prepare(
            r#"
            SELECT id, orig_path, COALESCE(orig_bytes, 0)
              FROM photos
             WHERE is_orig_cached = 1 AND orig_path IS NOT NULL
             ORDER BY last_accessed_at ASC, id ASC
            "#,
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(CachedOriginal {
                id: row.get(0)?,
                path: PathBuf::from(row.get::<_, String>(1)?),
                bytes: from_sql_bytes(row.get(2)?),
            })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }
}

#[cfg(test)]
impl Catalog {
    /// Simuliert ein gecachtes Original ohne Zugriffshistorie (Prefetch).
    pub(crate) fn touch_to_null_for_test(&self, id: &str) -> Result<(), CacheError> {
        self.conn.execute(
            "UPDATE photos SET last_accessed_at = NULL WHERE id = ?1",
            params![id],
        )?;
        Ok(())
    }
}

/// Ein im Full-Res-Cache liegendes Original.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CachedOriginal {
    pub id: String,
    pub path: PathBuf,
    pub bytes: u64,
}

/// SQLite kennt nur vorzeichenbehaftete 64-Bit-Integer. Dateigrößen jenseits
/// von `i64::MAX` gibt es nicht, deshalb ist die Sättigung hier folgenlos.
fn to_sql_bytes(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

fn from_sql_bytes(value: i64) -> u64 {
    u64::try_from(value).unwrap_or(0)
}

/// Fester, lexikographisch sortierbarer Zeitstempel (immer UTC, feste Breite).
fn encode_ts(value: DateTime<Utc>) -> String {
    value.to_rfc3339_opts(SecondsFormat::Micros, true)
}

fn decode_ts(raw: &str) -> Result<DateTime<Utc>, CacheError> {
    DateTime::parse_from_rfc3339(raw)
        .map(|dt| dt.with_timezone(&Utc))
        .map_err(|_| CacheError::Timestamp(raw.to_string()))
}

/// `rusqlite` erlaubt in der Mapper-Closure nur `rusqlite::Error`, deshalb
/// reicht die Zeile das Zeitstempel-Ergebnis nach außen durch.
fn row_to_photo(row: &Row<'_>) -> rusqlite::Result<Result<Photo, CacheError>> {
    Ok(build_photo(row))
}

fn build_photo(row: &Row<'_>) -> Result<Photo, CacheError> {
    let added_raw: String = row.get("added_date")?;
    let asset_raw: Option<String> = row.get("asset_date")?;
    let accessed_raw: Option<String> = row.get("last_accessed_at")?;

    Ok(Photo {
        record: PhotoRecord {
            id: row.get("id")?,
            filename: row.get("filename")?,
            added_date: decode_ts(&added_raw)?,
            asset_date: asset_raw.as_deref().map(decode_ts).transpose()?,
            checksum: row.get("checksum")?,
            width: row.get("width")?,
            height: row.get("height")?,
        },
        thumb_path: row
            .get::<_, Option<String>>("thumb_path")?
            .map(PathBuf::from),
        thumb_bytes: row
            .get::<_, Option<i64>>("thumb_bytes")?
            .map(from_sql_bytes),
        orig_path: row
            .get::<_, Option<String>>("orig_path")?
            .map(PathBuf::from),
        orig_bytes: row.get::<_, Option<i64>>("orig_bytes")?.map(from_sql_bytes),
        is_orig_cached: row.get::<_, i64>("is_orig_cached")? != 0,
        last_accessed_at: accessed_raw.as_deref().map(decode_ts).transpose()?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(id: &str, added: &str) -> PhotoRecord {
        PhotoRecord {
            id: id.to_string(),
            filename: format!("{id}.HEIC"),
            added_date: DateTime::parse_from_rfc3339(added)
                .unwrap()
                .with_timezone(&Utc),
            asset_date: None,
            checksum: None,
            width: Some(4032),
            height: Some(3024),
        }
    }

    #[test]
    fn roundtrip_record() {
        let catalog = Catalog::open_in_memory().unwrap();
        let rec = record("a", "2026-01-01T00:00:00Z");
        catalog.upsert_record(&rec).unwrap();

        let photo = catalog.get("a").unwrap().unwrap();
        assert_eq!(photo.record, rec);
        assert!(!photo.is_orig_cached);
        assert_eq!(photo.last_accessed_at, None);
    }

    #[test]
    fn upsert_preserves_cache_columns() {
        let catalog = Catalog::open_in_memory().unwrap();
        catalog
            .upsert_record(&record("a", "2026-01-01T00:00:00Z"))
            .unwrap();

        let now = Utc::now();
        catalog
            .set_original_cached("a", Path::new("/cache/a.heic"), 1234, now)
            .unwrap();
        catalog
            .set_thumbnail("a", Path::new("/thumbs/a.jpg"), 42)
            .unwrap();

        // Ein erneuter Sync liefert dieselbe ID mit geändertem Dateinamen.
        let mut updated = record("a", "2026-01-01T00:00:00Z");
        updated.filename = "renamed.HEIC".into();
        catalog.upsert_record(&updated).unwrap();

        let photo = catalog.get("a").unwrap().unwrap();
        assert_eq!(photo.record.filename, "renamed.HEIC");
        assert!(
            photo.is_orig_cached,
            "Cache-Status darf ein Sync nicht verlieren"
        );
        assert_eq!(photo.orig_bytes, Some(1234));
        assert_eq!(photo.thumb_bytes, Some(42));
    }

    #[test]
    fn sync_cursor_is_newest_added_date() {
        let mut catalog = Catalog::open_in_memory().unwrap();
        assert_eq!(catalog.sync_cursor().unwrap(), None);

        catalog
            .upsert_batch(&[
                record("a", "2026-01-01T00:00:00Z"),
                record("c", "2026-03-01T00:00:00Z"),
                record("b", "2026-02-01T00:00:00Z"),
            ])
            .unwrap();

        let cursor = catalog.sync_cursor().unwrap().unwrap();
        assert_eq!(
            cursor.to_rfc3339_opts(SecondsFormat::Secs, true),
            "2026-03-01T00:00:00Z"
        );
    }

    #[test]
    fn page_returns_newest_first() {
        let mut catalog = Catalog::open_in_memory().unwrap();
        catalog
            .upsert_batch(&[
                record("a", "2026-01-01T00:00:00Z"),
                record("b", "2026-02-01T00:00:00Z"),
                record("c", "2026-03-01T00:00:00Z"),
            ])
            .unwrap();

        let ids: Vec<_> = catalog
            .page(0, 2)
            .unwrap()
            .iter()
            .map(|p| p.id().to_string())
            .collect();
        assert_eq!(ids, ["c", "b"]);

        let ids: Vec<_> = catalog
            .page(2, 2)
            .unwrap()
            .iter()
            .map(|p| p.id().to_string())
            .collect();
        assert_eq!(ids, ["a"]);
    }

    #[test]
    fn cached_bytes_counts_only_cached_rows() {
        let mut catalog = Catalog::open_in_memory().unwrap();
        catalog
            .upsert_batch(&[
                record("a", "2026-01-01T00:00:00Z"),
                record("b", "2026-02-01T00:00:00Z"),
            ])
            .unwrap();
        assert_eq!(catalog.cached_bytes().unwrap(), 0);

        let now = Utc::now();
        catalog
            .set_original_cached("a", Path::new("/a"), 100, now)
            .unwrap();
        catalog
            .set_original_cached("b", Path::new("/b"), 200, now)
            .unwrap();
        assert_eq!(catalog.cached_bytes().unwrap(), 300);

        catalog.clear_original("a").unwrap();
        assert_eq!(catalog.cached_bytes().unwrap(), 200);
    }

    #[test]
    fn clear_original_keeps_access_history() {
        let catalog = Catalog::open_in_memory().unwrap();
        catalog
            .upsert_record(&record("a", "2026-01-01T00:00:00Z"))
            .unwrap();
        let now = Utc::now();
        catalog
            .set_original_cached("a", Path::new("/a"), 100, now)
            .unwrap();
        catalog.clear_original("a").unwrap();

        let photo = catalog.get("a").unwrap().unwrap();
        assert!(photo.last_accessed_at.is_some());
        assert_eq!(photo.orig_path, None);
        assert_eq!(photo.orig_bytes, None);
    }

    #[test]
    fn lists_photos_missing_a_thumbnail() {
        let mut catalog = Catalog::open_in_memory().unwrap();
        catalog
            .upsert_batch(&[
                record("a", "2026-01-01T00:00:00Z"),
                record("b", "2026-02-01T00:00:00Z"),
                record("c", "2026-03-01T00:00:00Z"),
            ])
            .unwrap();
        catalog
            .set_thumbnail("b", Path::new("/thumbs/b.jpg"), 10)
            .unwrap();

        // Neueste zuerst, damit die sichtbaren Kacheln zuerst gefüllt werden.
        assert_eq!(catalog.ids_without_thumbnail(10).unwrap(), ["c", "a"]);
        assert_eq!(catalog.ids_without_thumbnail(1).unwrap(), ["c"]);

        catalog
            .set_thumbnail("a", Path::new("/thumbs/a.jpg"), 10)
            .unwrap();
        catalog
            .set_thumbnail("c", Path::new("/thumbs/c.jpg"), 10)
            .unwrap();
        assert!(catalog.ids_without_thumbnail(10).unwrap().is_empty());
    }

    #[test]
    fn lru_candidates_put_never_accessed_first() {
        let mut catalog = Catalog::open_in_memory().unwrap();
        catalog
            .upsert_batch(&[
                record("old", "2026-01-01T00:00:00Z"),
                record("new", "2026-01-01T00:00:00Z"),
                record("never", "2026-01-01T00:00:00Z"),
            ])
            .unwrap();

        let t0 = DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let t1 = DateTime::parse_from_rfc3339("2026-06-01T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        catalog
            .set_original_cached("old", Path::new("/old"), 10, t0)
            .unwrap();
        catalog
            .set_original_cached("new", Path::new("/new"), 10, t1)
            .unwrap();
        // "never" wird gecacht, aber die Zugriffszeit danach auf NULL gesetzt,
        // wie es bei einem Prefetch ohne Ansehen entstünde.
        catalog
            .set_original_cached("never", Path::new("/never"), 10, t1)
            .unwrap();
        catalog
            .conn
            .execute(
                "UPDATE photos SET last_accessed_at = NULL WHERE id = 'never'",
                [],
            )
            .unwrap();

        let ids: Vec<_> = catalog
            .lru_candidates()
            .unwrap()
            .into_iter()
            .map(|c| c.id)
            .collect();
        assert_eq!(ids, ["never", "old", "new"]);
    }

    #[test]
    fn touch_updates_access_time() {
        let catalog = Catalog::open_in_memory().unwrap();
        catalog
            .upsert_record(&record("a", "2026-01-01T00:00:00Z"))
            .unwrap();
        let t0 = DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        catalog
            .set_original_cached("a", Path::new("/a"), 10, t0)
            .unwrap();

        let t1 = DateTime::parse_from_rfc3339("2026-06-01T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        catalog.touch("a", t1).unwrap();

        assert_eq!(
            catalog.get("a").unwrap().unwrap().last_accessed_at,
            Some(t1)
        );
    }

    #[test]
    fn timestamps_sort_lexicographically() {
        // Feste Breite und "Z"-Suffix sind Voraussetzung dafür, dass SQLite
        // TEXT-Spalten chronologisch sortiert.
        let a = encode_ts(
            DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
                .unwrap()
                .with_timezone(&Utc),
        );
        let b = encode_ts(
            DateTime::parse_from_rfc3339("2026-01-01T00:00:00.5Z")
                .unwrap()
                .with_timezone(&Utc),
        );
        let c = encode_ts(
            DateTime::parse_from_rfc3339("2026-01-01T00:00:01Z")
                .unwrap()
                .with_timezone(&Utc),
        );
        assert!(a < b && b < c, "{a} {b} {c}");
        assert_eq!(a.len(), b.len());
        assert_eq!(b.len(), c.len());
    }
}
