//! Katalog-Zugriff. Besitzt das SQLite-Schema und alle Zeilenoperationen.

use std::path::{Path, PathBuf};

use chrono::{DateTime, SecondsFormat, Utc};
use icloud_photos_core::model::{Photo, PhotoRecord};
use rusqlite::{Connection, OptionalExtension, Row, params, params_from_iter};

use crate::CacheError;

const SCHEMA_VERSION: i32 = 7;

/// Sortierschlüssel der Bibliothek: der Aufnahmezeitpunkt, ersatzweise das
/// Hinzufügedatum.
///
/// `added_date` allein taugt nicht — ein 2019 aufgenommenes Foto, das gestern
/// aus einem Backup in die Bibliothek kam, stünde sonst ganz vorn. Muss
/// wörtlich mit dem Ausdruck im Index `idx_photos_capture` übereinstimmen,
/// sonst sortiert SQLite die ganze Tabelle.
const SORT_KEY: &str = "COALESCE(asset_date, added_date)";

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
                master_id        TEXT,
                filename         TEXT NOT NULL,
                added_date       TEXT NOT NULL,
                asset_date       TEXT,
                checksum         TEXT,
                width            INTEGER,
                height           INTEGER,
                item_type        TEXT,
                lat              REAL,
                lon              REAL,
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
            -- Die Reihenfolge des Rasters. Ohne diesen Index sortiert jede
            -- Seitenabfrage die gesamte Tabelle.
            CREATE INDEX IF NOT EXISTS idx_photos_capture
                ON photos(COALESCE(asset_date, added_date) DESC, id DESC);
            -- Beschleunigt Summenbildung und Eviction-Auswahl, die beide nur
            -- gecachte Originale betrachten.
            CREATE INDEX IF NOT EXISTS idx_photos_cached_lru
                ON photos(last_accessed_at) WHERE is_orig_cached = 1;

            -- Einzeiliger Zustand des Sync. Der zone_sync_token ist das
            -- CloudKit-Pendant zu Apples eigenem Abgleich: aendert er sich
            -- nicht, hat sich in der Bibliothek nichts getan.
            CREATE TABLE IF NOT EXISTS sync_state (
                id               INTEGER PRIMARY KEY CHECK (id = 1),
                zone_sync_token  TEXT,
                baseline_done    INTEGER NOT NULL DEFAULT 0,
                baseline_offset  INTEGER NOT NULL DEFAULT 0,
                last_baseline_at TEXT,
                last_sync_at     TEXT,
                baseline_gap     INTEGER NOT NULL DEFAULT 0
            );
            INSERT OR IGNORE INTO sync_state (id) VALUES (1);

            -- Aufgelöste Ortsnamen. iCloud liefert nur Koordinaten; der Name
            -- kommt von einem Geocoding-Dienst und wird hier festgehalten,
            -- damit dieselbe Stelle nie zweimal angefragt wird. Der Schlüssel
            -- ist die gerundete Koordinate, nicht das Foto: an einem Ort
            -- entstehen viele Aufnahmen.
            CREATE TABLE IF NOT EXISTS places (
                cell         TEXT PRIMARY KEY,
                name         TEXT,
                looked_up_at TEXT NOT NULL
            );
            "#,
        )?;
        // Bestandskataloge nachziehen. `CREATE TABLE IF NOT EXISTS` oben legt
        // nur neue Kataloge vollständig an; eine bestehende Tabelle behält ihre
        // Spalten und braucht jede Erweiterung einzeln.
        if version >= 1 {
            self.add_column("photos", "master_id TEXT")?; // Schema 3
            self.add_column("photos", "item_type TEXT")?; // Schema 4
            self.add_column("sync_state", "last_sync_at TEXT")?; // Schema 5
            self.add_column("photos", "lat REAL")?; // Schema 6
            self.add_column("photos", "lon REAL")?; // Schema 6
            self.add_column("sync_state", "baseline_gap INTEGER NOT NULL DEFAULT 0")?; // Schema 7
        }

        self.conn
            .pragma_update(None, "user_version", SCHEMA_VERSION)?;
        Ok(())
    }

    /// Hängt eine Spalte an eine Tabelle, sofern sie noch fehlt.
    ///
    /// SQLite kennt kein `ADD COLUMN IF NOT EXISTS`; ein zweiter Lauf meldet
    /// deshalb "duplicate column", und genau das ist hier der Normalfall.
    fn add_column(&self, table: &str, definition: &str) -> Result<(), CacheError> {
        match self
            .conn
            .execute(&format!("ALTER TABLE {table} ADD COLUMN {definition}"), [])
        {
            Ok(_) => Ok(()),
            Err(error) if error.to_string().contains("duplicate column") => Ok(()),
            Err(error) => Err(error.into()),
        }
    }

    /// Schreibt Metadaten aus einem Sync. Cache-Spalten bleiben unangetastet,
    /// damit ein Re-Sync keine bereits geladenen Dateien vergisst.
    pub fn upsert_record(&self, record: &PhotoRecord) -> Result<(), CacheError> {
        self.conn
            .execute(UPSERT_SQL, params_from_iter(upsert_params(record)))?;
        Ok(())
    }

    /// Schreibt viele Datensätze in einer Transaktion.
    pub fn upsert_batch(&mut self, records: &[PhotoRecord]) -> Result<(), CacheError> {
        let tx = self.conn.transaction()?;
        {
            let mut stmt = tx.prepare(UPSERT_SQL)?;
            for record in records {
                stmt.execute(params_from_iter(upsert_params(record)))?;
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

    /// Eine Seite des Katalogs, zuletzt aufgenommene zuerst — für die
    /// virtualisierte Grid-View.
    pub fn page(&self, offset: u64, limit: u64) -> Result<Vec<Photo>, CacheError> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT * FROM photos ORDER BY {SORT_KEY} DESC, id DESC LIMIT ?1 OFFSET ?2"
        ))?;
        let rows = stmt.query_map(
            params![to_sql_bytes(limit), to_sql_bytes(offset)],
            row_to_photo,
        )?;
        rows.collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .collect::<Result<Vec<_>, _>>()
    }

    /// Die Monate der Bibliothek, neueste zuerst, mit Anzahl und Position im
    /// Raster.
    ///
    /// `offset` ist der Index des ersten Fotos dieses Monats in derselben
    /// Reihenfolge, die [`Catalog::page`] liefert — damit kann die Ansicht
    /// direkt dorthin springen.
    pub fn month_groups(&self) -> Result<Vec<DateGroup>, CacheError> {
        let mut stmt = self.conn.prepare(&format!(
            r#"
            SELECT substr({SORT_KEY}, 1, 7) AS month, COUNT(*)
              FROM photos
             GROUP BY month
             ORDER BY month DESC
            "#
        ))?;
        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, from_sql_bytes(row.get(1)?)))
        })?;

        // Die Position ergibt sich aus der Summe aller neueren Monate. Das in
        // SQL zu rechnen wäre eine Fensterfunktion für nichts — die Liste ist
        // ein paar Dutzend Zeilen lang.
        let mut offset = 0;
        let mut groups = Vec::new();
        for row in rows {
            let (month, count) = row?;
            groups.push(DateGroup {
                month,
                count,
                offset,
            });
            offset += count;
        }
        Ok(groups)
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

    /// Vermerkt ein vorausschauend geholtes Original, ohne es als benutzt zu
    /// zählen.
    ///
    /// Der Viewer laedt die Nachbarn des offenen Fotos im Voraus. Wuerden die
    /// dabei `last_accessed_at` bekommen, stuende ein nie angesehenes Foto in
    /// der LRU-Reihe vor einem tatsaechlich betrachteten — die Eviction wuerde
    /// das Falsche wegwerfen. Ohne Zeitstempel fliegen sie zuerst raus, und
    /// genau das ist richtig.
    pub fn set_original_prefetched(
        &self,
        id: &str,
        path: &Path,
        bytes: u64,
    ) -> Result<(), CacheError> {
        self.conn.execute(
            r#"
            UPDATE photos
               SET orig_path = ?2, orig_bytes = ?3, is_orig_cached = 1
             WHERE id = ?1
            "#,
            params![id, path.to_string_lossy(), to_sql_bytes(bytes)],
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

    /// Wie viel Platz die Vorschauen belegen, und wie viele es sind.
    ///
    /// Gezaehlt wird, was der Katalog ueber sie weiss — eine Vorschau ohne
    /// Katalogzeile (nach einem Absturz liegengeblieben) fehlt hier.
    pub fn thumbnail_usage(&self) -> Result<(u64, u64), CacheError> {
        let (sum, count): (i64, i64) = self.conn.query_row(
            "SELECT COALESCE(SUM(thumb_bytes), 0), COUNT(*) FROM photos WHERE thumb_path IS NOT NULL",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        Ok((from_sql_bytes(sum), from_sql_bytes(count)))
    }

    /// Der gespeicherte Sync-Zustand.
    pub fn sync_state(&self) -> Result<SyncState, CacheError> {
        let (token, done, offset, baseline_at, sync_at, gap): (
            Option<String>,
            i64,
            i64,
            Option<String>,
            Option<String>,
            i64,
        ) = self.conn.query_row(
            "SELECT zone_sync_token, baseline_done, baseline_offset, last_baseline_at,
                    last_sync_at, baseline_gap
               FROM sync_state WHERE id = 1",
            [],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                ))
            },
        )?;
        Ok(SyncState {
            zone_token: token,
            baseline_done: done != 0,
            baseline_offset: from_sql_bytes(offset),
            last_baseline_at: baseline_at.as_deref().map(decode_ts).transpose()?,
            last_sync_at: sync_at.as_deref().map(decode_ts).transpose()?,
            baseline_gap: from_sql_bytes(gap),
        })
    }

    /// Der gemerkte Ortsname für eine Koordinate.
    ///
    /// Gibt `Some(None)` zurück, wenn die Stelle schon angefragt wurde und der
    /// Dienst nichts wusste — dann lohnt keine zweite Anfrage. `None` heisst:
    /// noch nie nachgesehen.
    pub fn place(&self, cell: &str) -> Result<Option<Option<String>>, CacheError> {
        let found: Option<Option<String>> = self
            .conn
            .query_row(
                "SELECT name FROM places WHERE cell = ?1",
                params![cell],
                |row| row.get(0),
            )
            .optional()?;
        Ok(found)
    }

    /// Hält einen aufgelösten Ortsnamen fest. `None` merkt sich, dass der
    /// Dienst für diese Stelle nichts hergab.
    pub fn remember_place(
        &self,
        cell: &str,
        name: Option<&str>,
        now: DateTime<Utc>,
    ) -> Result<(), CacheError> {
        self.conn.execute(
            "INSERT INTO places (cell, name, looked_up_at) VALUES (?1, ?2, ?3)
             ON CONFLICT(cell) DO UPDATE SET name = excluded.name,
                                             looked_up_at = excluded.looked_up_at",
            params![cell, name, encode_ts(now)],
        )?;
        Ok(())
    }

    /// Merkt, wie viele Assets nach einem vollständigen Durchlauf fehlen.
    ///
    /// iCloud zählt in `photos.all` Dinge mit, die nie als Asset ankommen.
    /// Diese Lücke ist strukturell und kein Grund, den Katalog neu aufzubauen —
    /// wohl aber jede Abweichung *darüber hinaus*.
    pub fn set_baseline_gap(&self, gap: u64) -> Result<(), CacheError> {
        self.conn.execute(
            "UPDATE sync_state SET baseline_gap = ?1 WHERE id = 1",
            params![to_sql_bytes(gap)],
        )?;
        Ok(())
    }

    /// Hält fest, wann zuletzt ein Abgleich durchlief — egal welcher Art.
    ///
    /// `last_baseline_at` taugt dafür nicht: es steht nur nach einem
    /// vollständigen Durchlauf, und die gibt es im Betrieb alle 24 Stunden
    /// einmal. Für die Anzeige "zuletzt aktualisiert" ist das zu grob.
    pub fn set_last_sync(&self, now: DateTime<Utc>) -> Result<(), CacheError> {
        self.conn.execute(
            "UPDATE sync_state SET last_sync_at = ?1 WHERE id = 1",
            params![encode_ts(now)],
        )?;
        Ok(())
    }

    /// Vermerkt den Token, mit dem der naechste Sync vergleicht.
    pub fn set_zone_token(&self, token: Option<&str>) -> Result<(), CacheError> {
        self.conn.execute(
            "UPDATE sync_state SET zone_sync_token = ?1 WHERE id = 1",
            params![token],
        )?;
        Ok(())
    }

    /// Haelt fest, wie weit ein Erstaufbau gekommen ist.
    ///
    /// Solange `done` falsch ist, faengt der naechste Lauf bei `offset` wieder
    /// an, statt die Bibliothek von vorn zu paginieren.
    pub fn set_baseline_progress(
        &self,
        offset: u64,
        done: bool,
        now: DateTime<Utc>,
    ) -> Result<(), CacheError> {
        self.conn.execute(
            r#"
            UPDATE sync_state
               SET baseline_offset  = ?1,
                   baseline_done    = ?2,
                   last_baseline_at = CASE WHEN ?2 THEN ?3 ELSE last_baseline_at END
             WHERE id = 1
            "#,
            params![to_sql_bytes(offset), done, encode_ts(now)],
        )?;
        Ok(())
    }

    /// Entfernt ein in iCloud geloeschtes Foto aus dem Katalog.
    ///
    /// Gibt die Cache-Dateien zurueck, die der Aufrufer noch loeschen muss —
    /// der Katalog fasst das Dateisystem selbst nicht an.
    pub fn delete_photo(&self, id: &str) -> Result<Vec<PathBuf>, CacheError> {
        let paths: Option<(Option<String>, Option<String>)> = self
            .conn
            .query_row(
                "SELECT thumb_path, orig_path FROM photos WHERE id = ?1",
                params![id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let Some((thumb, orig)) = paths else {
            return Ok(Vec::new());
        };
        self.conn
            .execute("DELETE FROM photos WHERE id = ?1", params![id])?;
        Ok([thumb, orig]
            .into_iter()
            .flatten()
            .map(PathBuf::from)
            .collect())
    }

    /// IDs von Fotos ohne Vorschau, neueste zuerst.
    ///
    /// Der Delta-Sync sieht ältere Assets nicht erneut; fehlt dort eine
    /// Vorschau, muss sie gezielt nachgeholt werden.
    pub fn ids_without_thumbnail(&self, limit: u64) -> Result<Vec<String>, CacheError> {
        let mut stmt = self.conn.prepare(&format!(
            r#"
            SELECT id
              FROM photos
             WHERE thumb_path IS NULL
             ORDER BY {SORT_KEY} DESC
             LIMIT ?1
            "#
        ))?;
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

/// Wie weit der Sync ist.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SyncState {
    /// CloudKit-syncToken der Foto-Zone beim letzten erfolgreichen Lauf.
    pub zone_token: Option<String>,
    /// Ob die Bibliothek schon einmal vollstaendig durchlaufen wurde.
    pub baseline_done: bool,
    /// Position, an der ein abgebrochener Erstaufbau weitermacht.
    pub baseline_offset: u64,
    /// Wann zuletzt ein vollständiger Durchlauf fertig wurde.
    pub last_baseline_at: Option<DateTime<Utc>>,
    /// Wann zuletzt überhaupt ein Abgleich durchlief.
    pub last_sync_at: Option<DateTime<Utc>>,
    /// Wie viele Assets nach dem letzten vollständigen Durchlauf fehlten.
    pub baseline_gap: u64,
}

/// Der Schlüssel, unter dem ein Ortsname gemerkt wird.
///
/// Drei Nachkommastellen sind rund 110 Meter — fein genug, dass der Name noch
/// stimmt, grob genug, dass eine Fotoserie an derselben Stelle nur eine
/// einzige Anfrage kostet.
pub fn place_cell(lat: f64, lon: f64) -> String {
    format!("{lat:.3},{lon:.3}")
}

/// Ein Monat der Bibliothek und wo er im Raster beginnt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DateGroup {
    /// `YYYY-MM`, direkt aus dem Zeitstempel geschnitten.
    pub month: String,
    pub count: u64,
    /// Index des ersten Fotos dieses Monats in der Rasterreihenfolge.
    pub offset: u64,
}

/// Ein im Full-Res-Cache liegendes Original.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CachedOriginal {
    pub id: String,
    pub path: PathBuf,
    pub bytes: u64,
}

/// Der Upsert eines Metadatensatzes — einmal formuliert, von Einzel- und
/// Stapelschreibung benutzt.
///
/// `master_id` und `item_type` werden nur überschrieben, wenn der neue
/// Datensatz sie wirklich mitbringt: ein älterer Sidecar liefert sie nicht,
/// und ein bereits bekannter Wert ist besser als keiner.
const UPSERT_SQL: &str = r#"
    INSERT INTO photos (id, master_id, filename, added_date, asset_date, checksum, width, height, item_type, lat, lon)
    VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
    ON CONFLICT(id) DO UPDATE SET
        master_id  = COALESCE(excluded.master_id, photos.master_id),
        filename   = excluded.filename,
        added_date = excluded.added_date,
        asset_date = excluded.asset_date,
        checksum   = excluded.checksum,
        width      = excluded.width,
        height     = excluded.height,
        item_type  = COALESCE(excluded.item_type, photos.item_type),
        lat        = COALESCE(excluded.lat, photos.lat),
        lon        = COALESCE(excluded.lon, photos.lon)
"#;

fn upsert_params(record: &PhotoRecord) -> Vec<Box<dyn rusqlite::ToSql>> {
    vec![
        Box::new(record.id.clone()),
        Box::new(record.master_id.clone()),
        Box::new(record.filename.clone()),
        Box::new(encode_ts(record.added_date)),
        Box::new(record.asset_date.map(encode_ts)),
        Box::new(record.checksum.clone()),
        Box::new(record.width),
        Box::new(record.height),
        Box::new(record.item_type.clone()),
        Box::new(record.lat),
        Box::new(record.lon),
    ]
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
            master_id: row.get("master_id")?,
            filename: row.get("filename")?,
            added_date: decode_ts(&added_raw)?,
            asset_date: asset_raw.as_deref().map(decode_ts).transpose()?,
            checksum: row.get("checksum")?,
            width: row.get("width")?,
            height: row.get("height")?,
            item_type: row.get("item_type")?,
            lat: row.get("lat")?,
            lon: row.get("lon")?,
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
            master_id: Some(format!("master-{id}")),
            filename: format!("{id}.HEIC"),
            added_date: DateTime::parse_from_rfc3339(added)
                .unwrap()
                .with_timezone(&Utc),
            asset_date: None,
            checksum: None,
            width: Some(4032),
            height: Some(3024),
            lat: None,
            lon: None,
            item_type: None,
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
    fn thumbnail_usage_sums_only_existing_previews() {
        let mut catalog = Catalog::open_in_memory().unwrap();
        catalog
            .upsert_batch(&[
                record("a", "2026-01-01T00:00:00Z"),
                record("b", "2026-02-01T00:00:00Z"),
                record("c", "2026-03-01T00:00:00Z"),
            ])
            .unwrap();
        assert_eq!(catalog.thumbnail_usage().unwrap(), (0, 0));

        catalog
            .set_thumbnail("a", Path::new("/t/a.jpg"), 30_000)
            .unwrap();
        catalog
            .set_thumbnail("b", Path::new("/t/b.jpg"), 45_000)
            .unwrap();
        assert_eq!(catalog.thumbnail_usage().unwrap(), (75_000, 2));
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
    fn page_orders_by_capture_date_not_by_added_date() {
        let mut catalog = Catalog::open_in_memory().unwrap();
        let mut old_shot = record("alt", "2026-06-01T00:00:00Z");
        // Gestern aus einem Backup hinzugefuegt, aber 2019 aufgenommen: im
        // Raster gehoert es zu 2019, nicht an den Anfang.
        old_shot.asset_date = Some(
            DateTime::parse_from_rfc3339("2019-10-17T09:00:00Z")
                .unwrap()
                .with_timezone(&Utc),
        );
        let mut recent = record("neu", "2026-01-01T00:00:00Z");
        recent.asset_date = Some(
            DateTime::parse_from_rfc3339("2025-12-24T18:00:00Z")
                .unwrap()
                .with_timezone(&Utc),
        );
        // Ohne Aufnahmedatum zaehlt das Hinzufuegedatum.
        let undated = record("ohne", "2022-05-05T00:00:00Z");
        catalog.upsert_batch(&[old_shot, recent, undated]).unwrap();

        let ids: Vec<_> = catalog
            .page(0, 10)
            .unwrap()
            .iter()
            .map(|p| p.id().to_string())
            .collect();
        assert_eq!(ids, ["neu", "ohne", "alt"]);
    }

    #[test]
    fn month_groups_count_and_locate_each_month() {
        let mut catalog = Catalog::open_in_memory().unwrap();
        catalog
            .upsert_batch(&[
                record("a", "2026-03-02T00:00:00Z"),
                record("b", "2026-03-01T00:00:00Z"),
                record("c", "2026-01-15T00:00:00Z"),
                record("d", "2025-12-31T00:00:00Z"),
            ])
            .unwrap();

        let groups = catalog.month_groups().unwrap();
        assert_eq!(
            groups,
            vec![
                DateGroup {
                    month: "2026-03".into(),
                    count: 2,
                    offset: 0
                },
                DateGroup {
                    month: "2026-01".into(),
                    count: 1,
                    offset: 2
                },
                DateGroup {
                    month: "2025-12".into(),
                    count: 1,
                    offset: 3
                },
            ]
        );

        // Der gemeldete Offset muss wirklich auf das erste Foto des Monats
        // zeigen — sonst springt die Ansicht daneben.
        for group in groups {
            let page = catalog.page(group.offset, 1).unwrap();
            assert!(
                page[0]
                    .record
                    .capture_date()
                    .to_rfc3339()
                    .starts_with(&group.month)
            );
        }
    }

    #[test]
    fn a_place_is_looked_up_once_even_when_it_has_no_name() {
        let catalog = Catalog::open_in_memory().unwrap();
        let cell = place_cell(48.1372, 11.5756);
        assert_eq!(cell, "48.137,11.576");

        // Noch nie nachgesehen.
        assert_eq!(catalog.place(&cell).unwrap(), None);

        catalog
            .remember_place(&cell, Some("Marienplatz, München"), Utc::now())
            .unwrap();
        assert_eq!(
            catalog.place(&cell).unwrap(),
            Some(Some("Marienplatz, München".to_string()))
        );

        // Mitten im Meer weiss auch der Dienst nichts. Das gemerkte "nichts"
        // unterscheidet sich von "noch nicht gefragt" — sonst liefe jede
        // Anzeige erneut ins Netz.
        let empty = place_cell(0.5, -30.0);
        catalog.remember_place(&empty, None, Utc::now()).unwrap();
        assert_eq!(catalog.place(&empty).unwrap(), Some(None));
    }

    #[test]
    fn nearby_photos_share_one_place_lookup() {
        // 20 Meter auseinander: dieselbe Zelle, also eine Anfrage statt zwei.
        assert_eq!(place_cell(48.1372, 11.5756), place_cell(48.1374, 11.5757));
        // Ein Kilometer weiter ist es eine andere.
        assert_ne!(place_cell(48.1372, 11.5756), place_cell(48.1472, 11.5756));
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
    fn prefetched_originals_stay_first_in_line_for_eviction() {
        let mut catalog = Catalog::open_in_memory().unwrap();
        catalog
            .upsert_batch(&[
                record("angesehen", "2026-01-01T00:00:00Z"),
                record("vorgeladen", "2026-01-01T00:00:00Z"),
            ])
            .unwrap();

        let now = Utc::now();
        catalog
            .set_original_cached("angesehen", Path::new("/a"), 10, now)
            .unwrap();
        catalog
            .set_original_prefetched("vorgeladen", Path::new("/b"), 10)
            .unwrap();

        assert!(catalog.get("vorgeladen").unwrap().unwrap().is_orig_cached);
        assert_eq!(
            catalog.get("vorgeladen").unwrap().unwrap().last_accessed_at,
            None,
            "ein Prefetch ist kein Zugriff"
        );
        assert_eq!(catalog.cached_bytes().unwrap(), 20);

        let ids: Vec<_> = catalog
            .lru_candidates()
            .unwrap()
            .into_iter()
            .map(|c| c.id)
            .collect();
        assert_eq!(ids, ["vorgeladen", "angesehen"]);
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
    fn sync_state_starts_empty_and_persists() {
        let catalog = Catalog::open_in_memory().unwrap();
        let state = catalog.sync_state().unwrap();
        assert_eq!(state.zone_token, None);
        assert!(!state.baseline_done);
        assert_eq!(state.baseline_offset, 0);

        catalog.set_zone_token(Some("token-abc")).unwrap();
        let now = Utc::now();
        catalog.set_baseline_progress(1200, false, now).unwrap();

        let state = catalog.sync_state().unwrap();
        assert_eq!(state.zone_token.as_deref(), Some("token-abc"));
        assert_eq!(state.baseline_offset, 1200);
        assert!(!state.baseline_done);
        // Der Zeitstempel zaehlt erst, wenn der Durchlauf wirklich durch ist.
        assert_eq!(state.last_baseline_at, None);
        // Und "zuletzt aktualisiert" ist davon unabhaengig.
        assert_eq!(state.last_sync_at, None);
        catalog.set_last_sync(now).unwrap();
        assert!(catalog.sync_state().unwrap().last_sync_at.is_some());

        catalog.set_baseline_progress(1700, true, now).unwrap();
        let state = catalog.sync_state().unwrap();
        assert!(state.baseline_done);
        assert!(state.last_baseline_at.is_some());
    }

    #[test]
    fn delete_photo_reports_files_to_remove() {
        let catalog = Catalog::open_in_memory().unwrap();
        catalog
            .upsert_record(&record("a", "2026-01-01T00:00:00Z"))
            .unwrap();
        catalog
            .set_thumbnail("a", Path::new("/thumbs/a.jpg"), 10)
            .unwrap();
        catalog
            .set_original_cached("a", Path::new("/fullres/a.heic"), 99, Utc::now())
            .unwrap();

        let files = catalog.delete_photo("a").unwrap();
        assert_eq!(
            files,
            vec![
                PathBuf::from("/thumbs/a.jpg"),
                PathBuf::from("/fullres/a.heic")
            ]
        );
        assert_eq!(catalog.get("a").unwrap(), None);
        assert_eq!(catalog.cached_bytes().unwrap(), 0);
    }

    #[test]
    fn deleting_an_unknown_photo_is_harmless() {
        let catalog = Catalog::open_in_memory().unwrap();
        assert!(catalog.delete_photo("gibt-es-nicht").unwrap().is_empty());
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
