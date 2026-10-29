//! Was die Startseite („Home") aus dem Katalog liest: Erinnerungen an diesen
//! Tag in früheren Jahren, die letzten Tage, Zählungen für die Kopfzeilen und
//! die Suche.
//!
//! Alles hier läuft über den ganzen Katalog statt über eine Seite des Rasters.
//! An einer Bibliothek von rund 20.000 Fotos ist das ein einzelner Durchlauf
//! über die Tabelle — Millisekunden, und es fällt nur beim Öffnen der
//! Startseite oder einer Suche an, nicht beim Scrollen.
//!
//! Tage sind **lokale** Kalendertage (`date(…, 'localtime')`), nicht die des
//! UTC-Zeitstempels: ein Foto von kurz nach Mitternacht gehört zum neuen Tag,
//! so wie es auf der Uhr stand.

use std::path::PathBuf;

use chrono::{Datelike, NaiveDate};
use rusqlite::{ToSql, params_from_iter};

use crate::CacheError;
use crate::albums::Scope;
use crate::db::{Catalog, SORT_KEY, from_sql_bytes, row_to_photo};
use icloud_photos_core::model::Photo;

/// Der lokale Kalendertag eines Fotos als `YYYY-MM-DD`.
fn local_day() -> String {
    format!("date({SORT_KEY}, 'localtime')")
}

/// Bedingung „ist ein Video" — dieselbe Regel wie `PhotoRecord::kind`:
/// `item_type`, sonst die Dateiendung.
const IS_VIDEO: &str = "(item_type = 'movie' OR (item_type IS NULL AND (\
    lower(filename) LIKE '%.mov' OR lower(filename) LIKE '%.mp4' OR lower(filename) LIKE '%.m4v' \
    OR lower(filename) LIKE '%.avi' OR lower(filename) LIKE '%.3gp')))";

/// Fotos eines Tages in einem früheren Jahr — „On This Day".
#[derive(Debug, Clone, PartialEq)]
pub struct Memory {
    pub day: NaiveDate,
    /// Wie viele Fotos und Videos an diesem Tag entstanden sind.
    pub count: u64,
    /// Die ersten davon, in Rasterreihenfolge.
    pub photos: Vec<Photo>,
    /// Der Ort des Tages, falls einer bekannt ist.
    pub place: Option<String>,
}

/// Ein Tag der Bibliothek, wie die Karte „Recent Days" ihn zeigt.
#[derive(Debug, Clone, PartialEq)]
pub struct Day {
    pub day: NaiveDate,
    pub count: u64,
    pub photos: Vec<Photo>,
    /// Der Ort des Tages, falls ein Foto davon schon einen Namen hat — Orte
    /// werden nur für geöffnete Fotos nachgeschlagen, oft fehlt er also.
    pub place: Option<String>,
}

/// Zahlen eines Ausschnitts für die Unterzeile einer Seite.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Summary {
    pub items: u64,
    pub videos: u64,
    pub first: Option<NaiveDate>,
    pub last: Option<NaiveDate>,
}

fn parse_day(raw: &str) -> Option<NaiveDate> {
    NaiveDate::parse_from_str(raw, "%Y-%m-%d").ok()
}

/// `MM-DD` aller Tage höchstens `window` Tage um `today`.
fn nearby_days(today: NaiveDate, window: u32) -> Vec<String> {
    let window = i64::from(window);
    (-window..=window)
        .filter_map(|offset| today.checked_add_signed(chrono::TimeDelta::days(offset)))
        .map(|day| day.format("%m-%d").to_string())
        .collect()
}

impl Catalog {
    /// „On This Day": der Tag eines früheren Jahres, der `today` am nächsten
    /// liegt — höchstens `window` Tage daneben —, aus dem jüngsten Jahr, das
    /// einen hat. Wie `bin/velum-photo-picks` in Velum, nur aus den Vorschauen
    /// statt aus den Originalen: zeigen lässt sich jedes Foto.
    pub fn on_this_day(
        &self,
        today: NaiveDate,
        window: u32,
        limit: u64,
    ) -> Result<Option<Memory>, CacheError> {
        let days = nearby_days(today, window);
        let marks = vec!["?"; days.len()].join(", ");
        let day = local_day();
        let sql = format!(
            "SELECT {day} AS d, COUNT(*) FROM photos \
             WHERE substr({day}, 6, 5) IN ({marks}) AND CAST(substr({day}, 1, 4) AS INTEGER) < ? \
             GROUP BY d"
        );
        let mut values: Vec<Box<dyn ToSql>> = days
            .iter()
            .map(|d| Box::new(d.clone()) as Box<dyn ToSql>)
            .collect();
        values.push(Box::new(today.year()));
        let mut stmt = self.conn.prepare(&sql)?;
        let found: Vec<(String, i64)> = stmt
            .query_map(params_from_iter(values.iter()), |row| {
                Ok((row.get(0)?, row.get(1)?))
            })?
            .collect::<Result<_, _>>()?;

        // Jüngstes Jahr zuerst, darin der Tag mit dem kleinsten Abstand.
        let best = found
            .iter()
            .filter_map(|(raw, count)| Some((parse_day(raw)?, from_sql_bytes(*count))))
            .min_by_key(|(day, _)| {
                let distance = day
                    .with_year(today.year())
                    .map(|same| (same - today).num_days().abs())
                    .unwrap_or(i64::MAX);
                (std::cmp::Reverse(day.year()), distance)
            });
        let Some((day, count)) = best else {
            return Ok(None);
        };
        let photos = self.photos_of_day(day, limit)?;
        let place = self.place_of_day(day)?;
        Ok(Some(Memory {
            day,
            count,
            photos,
            place,
        }))
    }

    /// Die jüngsten `days` Tage der Bibliothek mit je bis zu `per_day` Fotos.
    pub fn recent_days(&self, days: u64, per_day: u64) -> Result<Vec<Day>, CacheError> {
        let day = local_day();
        // Durch den Index rückwärts, bis genug Tage beisammen sind: die
        // jüngsten Tage liegen vorn, eine Gruppierung der ganzen Tabelle wäre
        // Arbeit für nichts.
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {day} FROM photos ORDER BY {SORT_KEY} DESC, id DESC LIMIT ?1"
        ))?;
        let mut counts: Vec<(String, u64)> = Vec::new();
        let mut window = 512u64;
        loop {
            counts.clear();
            let rows: Vec<String> = stmt
                .query_map([crate::db::to_sql_bytes(window)], |row| row.get(0))?
                .collect::<Result<_, _>>()?;
            for raw in &rows {
                match counts.last_mut() {
                    Some((last, count)) if last == raw => *count += 1,
                    _ => counts.push((raw.clone(), 1)),
                }
            }
            let complete = (rows.len() as u64) < window || counts.len() as u64 > days;
            if complete {
                break;
            }
            window *= 4;
        }
        let mut out = Vec::new();
        for (raw, count) in counts.into_iter().take(days as usize) {
            let Some(day) = parse_day(&raw) else {
                continue;
            };
            out.push(Day {
                day,
                count,
                photos: self.photos_of_day(day, per_day)?,
                place: self.place_of_day(day)?,
            });
        }
        Ok(out)
    }

    /// Fotos eines lokalen Kalendertags in Rasterreihenfolge.
    pub fn photos_of_day(&self, day: NaiveDate, limit: u64) -> Result<Vec<Photo>, CacheError> {
        let local = local_day();
        let mut stmt = self.conn.prepare(&format!(
            "SELECT photos.* FROM photos WHERE {local} = ?1 \
             ORDER BY {SORT_KEY} DESC, id DESC LIMIT ?2"
        ))?;
        let rows = stmt
            .query_map(
                rusqlite::params![
                    day.format("%Y-%m-%d").to_string(),
                    crate::db::to_sql_bytes(limit)
                ],
                row_to_photo,
            )?
            .collect::<Result<Vec<_>, _>>()?;
        rows.into_iter().collect()
    }

    /// Der Ortsname eines Tages, aus dem ersten seiner Fotos mit bekanntem Ort.
    fn place_of_day(&self, day: NaiveDate) -> Result<Option<String>, CacheError> {
        let local = local_day();
        let mut stmt = self.conn.prepare(&format!(
            "SELECT places.name FROM photos JOIN places \
               ON places.cell = printf('%.3f,%.3f', photos.lat, photos.lon) \
             WHERE {local} = ?1 AND photos.lat IS NOT NULL AND places.name IS NOT NULL \
             ORDER BY {SORT_KEY} DESC LIMIT 1"
        ))?;
        let mut rows = stmt.query([day.format("%Y-%m-%d").to_string()])?;
        Ok(match rows.next()? {
            Some(row) => Some(row.get(0)?),
            None => None,
        })
    }

    /// Wie viele Videos im Katalog liegen.
    pub fn count_videos(&self) -> Result<u64, CacheError> {
        let count: i64 = self.conn.query_row(
            &format!("SELECT COUNT(*) FROM photos WHERE {IS_VIDEO}"),
            [],
            |row| row.get(0),
        )?;
        Ok(from_sql_bytes(count))
    }

    /// Anzahl, Videos und Zeitspanne eines Ausschnitts — die Unterzeile eines
    /// Albums („62 photos · 4 videos · October 10 – 12, 2026").
    pub fn summary_in(&self, scope: &Scope) -> Result<Summary, CacheError> {
        let (source, value) = scope.source(1);
        let local = local_day();
        let sql = format!(
            "SELECT COUNT(*), COALESCE(SUM({IS_VIDEO}), 0), MIN({local}), MAX({local}) FROM {source}"
        );
        let read = |row: &rusqlite::Row<'_>| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, Option<String>>(3)?,
            ))
        };
        let (items, videos, first, last) = match value {
            Some(value) => self.conn.query_row(&sql, [value.as_ref()], read)?,
            None => self.conn.query_row(&sql, [], read)?,
        };
        Ok(Summary {
            items: from_sql_bytes(items),
            videos: from_sql_bytes(videos),
            first: first.as_deref().and_then(parse_day),
            last: last.as_deref().and_then(parse_day),
        })
    }

    /// Fotos, auf die eine Suche passt — jedes Wort muss treffen: der
    /// Dateiname, der Ortsname, ein Monatsname („october") oder ein Jahr
    /// („2025"). In Rasterreihenfolge, höchstens `limit`.
    ///
    /// Gesucht wird nur in dem, was der Katalog weiß. Orte kennt er nur für
    /// Fotos, die schon einmal geöffnet waren (Nominatim wird nie für die
    /// ganze Bibliothek gefragt).
    pub fn search_ids(&self, query: &str, limit: u64) -> Result<Vec<String>, CacheError> {
        let words: Vec<String> = query
            .split_whitespace()
            .map(|word| {
                word.trim_matches(|c: char| c == ',' || c == '.')
                    .to_lowercase()
            })
            .filter(|word| !word.is_empty())
            .collect();
        if words.is_empty() {
            return Ok(Vec::new());
        }
        let local = local_day();
        let mut clauses = Vec::new();
        let mut values: Vec<Box<dyn ToSql>> = Vec::new();
        for word in &words {
            if let Some(month) = month_number(word) {
                clauses.push(format!("substr({local}, 6, 2) = ?"));
                values.push(Box::new(format!("{month:02}")));
            } else if word.len() == 4 && word.chars().all(|c| c.is_ascii_digit()) {
                clauses.push(format!("substr({local}, 1, 4) = ?"));
                values.push(Box::new(word.clone()));
            } else {
                let like = format!("%{}%", word.replace(['%', '_'], ""));
                clauses.push(
                    "(lower(photos.filename) LIKE ? OR lower(COALESCE(places.name, '')) LIKE ?)"
                        .to_string(),
                );
                values.push(Box::new(like.clone()));
                values.push(Box::new(like));
            }
        }
        values.push(Box::new(crate::db::to_sql_bytes(limit)));
        let sql = format!(
            "SELECT photos.id FROM photos LEFT JOIN places \
               ON photos.lat IS NOT NULL AND places.cell = printf('%.3f,%.3f', photos.lat, photos.lon) \
             WHERE {} ORDER BY {SORT_KEY} DESC, photos.id DESC LIMIT ?",
            clauses.join(" AND ")
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let ids = stmt
            .query_map(params_from_iter(values.iter()), |row| row.get(0))?
            .collect::<Result<Vec<String>, _>>()?;
        Ok(ids)
    }

    /// Die Vorschau des jüngsten Fotos eines Ausschnitts — Titelbild einer
    /// Karte, wenn das Album keins mitbringt.
    pub fn covers_in(&self, scope: &Scope, limit: u64) -> Result<Vec<PathBuf>, CacheError> {
        Ok(self
            .page_in(scope, 0, limit)?
            .into_iter()
            .filter_map(|photo| photo.thumb_path)
            .collect())
    }
}

/// Monatsname (englisch, ganz oder die ersten drei Buchstaben) → 1…12.
fn month_number(word: &str) -> Option<u32> {
    const MONTHS: [&str; 12] = [
        "january",
        "february",
        "march",
        "april",
        "may",
        "june",
        "july",
        "august",
        "september",
        "october",
        "november",
        "december",
    ];
    MONTHS
        .iter()
        .position(|name| *name == word || (word.len() >= 3 && name.starts_with(word)))
        .map(|index| index as u32 + 1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{DateTime, Utc};
    use icloud_photos_core::model::PhotoRecord;

    fn record(id: &str, when: &str, filename: &str) -> PhotoRecord {
        PhotoRecord {
            id: id.into(),
            master_id: None,
            filename: filename.into(),
            added_date: Utc::now(),
            asset_date: Some(
                DateTime::parse_from_rfc3339(when)
                    .unwrap()
                    .with_timezone(&Utc),
            ),
            checksum: None,
            width: Some(4032),
            height: Some(3024),
            lat: None,
            lon: None,
            item_type: None,
        }
    }

    /// Mittags UTC: an jedem Ort von UTC−11 bis UTC+11 derselbe Kalendertag.
    fn catalog(rows: &[(&str, &str, &str)]) -> Catalog {
        let catalog = Catalog::open_in_memory().unwrap();
        for (id, when, name) in rows {
            catalog.upsert_record(&record(id, when, name)).unwrap();
        }
        catalog
    }

    fn day(raw: &str) -> NaiveDate {
        NaiveDate::parse_from_str(raw, "%Y-%m-%d").unwrap()
    }

    #[test]
    fn on_this_day_takes_the_latest_year_and_the_nearest_day() {
        let catalog = catalog(&[
            ("a", "2023-10-29T12:00:00Z", "A.HEIC"),
            ("b", "2025-10-27T12:00:00Z", "B.HEIC"),
            ("c", "2025-10-30T12:00:00Z", "C.HEIC"),
            ("d", "2025-10-30T12:30:00Z", "D.HEIC"),
            ("e", "2026-10-29T12:00:00Z", "E.HEIC"), // dieses Jahr: nie
            ("f", "2024-11-20T12:00:00Z", "F.HEIC"), // zu weit weg
        ]);
        let memory = catalog
            .on_this_day(day("2026-10-29"), 3, 10)
            .unwrap()
            .unwrap();
        assert_eq!(memory.day, day("2025-10-30"));
        assert_eq!(memory.count, 2);
        let ids: Vec<&str> = memory.photos.iter().map(|p| p.id()).collect();
        assert_eq!(ids, ["d", "c"]);
    }

    #[test]
    fn on_this_day_wraps_around_the_turn_of_the_year() {
        let catalog = catalog(&[("a", "2024-12-31T12:00:00Z", "A.HEIC")]);
        let memory = catalog.on_this_day(day("2026-01-02"), 3, 10).unwrap();
        assert_eq!(memory.map(|m| m.day), Some(day("2024-12-31")));
        assert!(
            catalog
                .on_this_day(day("2026-06-01"), 3, 10)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn recent_days_group_by_day_newest_first() {
        let catalog = catalog(&[
            ("a", "2026-10-29T12:00:00Z", "A.HEIC"),
            ("b", "2026-10-29T11:00:00Z", "B.HEIC"),
            ("c", "2026-10-27T12:00:00Z", "C.MOV"),
            ("d", "2026-10-20T12:00:00Z", "D.HEIC"),
            ("e", "2026-09-01T12:00:00Z", "E.HEIC"),
        ]);
        let days = catalog.recent_days(3, 1).unwrap();
        let got: Vec<(NaiveDate, u64, usize)> = days
            .iter()
            .map(|d| (d.day, d.count, d.photos.len()))
            .collect();
        assert_eq!(
            got,
            [
                (day("2026-10-29"), 2, 1),
                (day("2026-10-27"), 1, 1),
                (day("2026-10-20"), 1, 1)
            ]
        );
        assert_eq!(days[0].photos[0].id(), "a");
    }

    #[test]
    fn summary_counts_videos_and_the_span() {
        let catalog = catalog(&[
            ("a", "2026-10-12T12:00:00Z", "A.HEIC"),
            ("b", "2026-10-10T12:00:00Z", "B.MOV"),
            ("c", "2026-10-11T12:00:00Z", "C.HEIC"),
        ]);
        let summary = catalog.summary_in(&Scope::Library).unwrap();
        assert_eq!(summary.items, 3);
        assert_eq!(summary.videos, 1);
        assert_eq!(summary.first, Some(day("2026-10-10")));
        assert_eq!(summary.last, Some(day("2026-10-12")));
        assert_eq!(catalog.count_videos().unwrap(), 1);
        let only = Scope::Photos(vec!["a".to_string()].into());
        assert_eq!(catalog.summary_in(&only).unwrap().items, 1);
    }

    #[test]
    fn search_matches_every_word() {
        let catalog = catalog(&[
            ("a", "2026-10-12T12:00:00Z", "IMG_0001.HEIC"),
            ("b", "2025-10-10T12:00:00Z", "IMG_0002.MOV"),
            ("c", "2025-03-11T12:00:00Z", "Screenshot.png"),
        ]);
        assert_eq!(catalog.search_ids("img", 10).unwrap(), ["a", "b"]);
        assert_eq!(catalog.search_ids("img 2025", 10).unwrap(), ["b"]);
        assert_eq!(catalog.search_ids("October", 10).unwrap(), ["a", "b"]);
        assert_eq!(catalog.search_ids("mar screenshot", 10).unwrap(), ["c"]);
        assert!(catalog.search_ids("   ", 10).unwrap().is_empty());
        assert!(catalog.search_ids("100%", 10).unwrap().is_empty());
    }

    #[test]
    fn search_finds_known_places() {
        let catalog = Catalog::open_in_memory().unwrap();
        let mut rec = record("a", "2026-10-12T12:00:00Z", "IMG_0001.HEIC");
        rec.lat = Some(51.3397);
        rec.lon = Some(12.3731);
        catalog.upsert_record(&rec).unwrap();
        catalog
            .upsert_record(&record("b", "2026-10-11T12:00:00Z", "IMG_0002.HEIC"))
            .unwrap();
        catalog
            .remember_place(
                &crate::db::place_cell(51.3397, 12.3731),
                Some("Markt, Leipzig"),
                Utc::now(),
            )
            .unwrap();
        assert_eq!(catalog.search_ids("leipzig", 10).unwrap(), ["a"]);
        let days = catalog.recent_days(1, 1).unwrap();
        assert_eq!(days[0].place.as_deref(), Some("Markt, Leipzig"));
    }

    #[test]
    fn month_names_and_short_forms() {
        assert_eq!(month_number("oct"), Some(10));
        assert_eq!(month_number("may"), Some(5));
        assert_eq!(month_number("ma"), None);
        assert_eq!(month_number("leipzig"), None);
    }
}
