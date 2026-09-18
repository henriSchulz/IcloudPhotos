//! Fotos mit Aufnahmeort — der Stoff, aus dem die Karte ihre Punkte macht.

use std::path::PathBuf;

use rusqlite::params;

use crate::CacheError;
use crate::db::{Catalog, SORT_KEY};

/// Ein Foto, wie die Karte es braucht: Stelle und Vorschau, sonst nichts.
#[derive(Debug, Clone, PartialEq)]
pub struct LocatedPhoto {
    pub id: String,
    pub lat: f64,
    pub lon: f64,
    pub thumb_path: Option<PathBuf>,
}

impl Catalog {
    /// Alle Fotos mit Aufnahmeort, zuletzt aufgenommene zuerst.
    ///
    /// Die Reihenfolge ist die des Rasters: so ist das erste Foto einer Gruppe
    /// auch ihr neuestes, und die Karte muss für das Titelbild nichts
    /// sortieren. Genau (0, 0) zählt wie in `PhotoRecord::coordinates` als
    /// leeres Feld. An der echten Bibliothek (19.587 Fotos, 10.651 mit Ort)
    /// dauert die Abfrage rund 11 ms — sie läuft durch `idx_photos_capture`,
    /// eine Sortierung fällt nicht an.
    pub fn located_photos(&self) -> Result<Vec<LocatedPhoto>, CacheError> {
        let mut stmt = self.conn.prepare(&located_sql())?;
        let rows = stmt.query_map(params![], |row| {
            Ok(LocatedPhoto {
                id: row.get(0)?,
                lat: row.get(1)?,
                lon: row.get(2)?,
                thumb_path: row.get::<_, Option<String>>(3)?.map(PathBuf::from),
            })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }
}

fn located_sql() -> String {
    format!(
        r#"
        SELECT id, lat, lon, thumb_path
          FROM photos
         WHERE lat IS NOT NULL AND lon IS NOT NULL AND (lat != 0 OR lon != 0)
         ORDER BY {SORT_KEY} DESC, id DESC
        "#
    )
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::Arc;

    use chrono::{DateTime, Utc};
    use icloud_photos_core::model::PhotoRecord;

    use super::*;
    use crate::Scope;
    use crate::db::page_sql;

    fn record(id: &str, day: u32, at: Option<(f64, f64)>) -> PhotoRecord {
        PhotoRecord {
            id: id.into(),
            master_id: None,
            filename: format!("{id}.HEIC"),
            added_date: DateTime::parse_from_rfc3339(&format!("2026-01-{day:02}T00:00:00Z"))
                .unwrap()
                .with_timezone(&Utc),
            asset_date: None,
            checksum: None,
            width: None,
            height: None,
            item_type: None,
            lat: at.map(|(lat, _)| lat),
            lon: at.map(|(_, lon)| lon),
        }
    }

    fn catalog() -> Catalog {
        let mut catalog = Catalog::open_in_memory().unwrap();
        let mut half = record("half", 5, None);
        half.lat = Some(48.1);
        catalog
            .upsert_batch(&[
                record("munich", 1, Some((48.137, 11.575))),
                record("nowhere", 2, None),
                record("paris", 3, Some((48.857, 2.352))),
                record("island", 4, Some((0.0, 0.0))),
                half,
                record("equator", 6, Some((0.0, 32.5))),
            ])
            .unwrap();
        catalog
    }

    #[test]
    fn only_photos_with_a_real_location_newest_first() {
        let catalog = catalog();
        catalog
            .set_thumbnail("paris", Path::new("/thumbs/paris.jpg"), 1)
            .unwrap();
        let located = catalog.located_photos().unwrap();
        let ids: Vec<_> = located.iter().map(|p| p.id.as_str()).collect();
        // (0, 0) ist ein leeres Feld, eine halbe Angabe keine — der Äquator
        // selbst aber sehr wohl ein Ort.
        assert_eq!(ids, ["equator", "paris", "munich"]);
        assert_eq!(located[1].lat, 48.857);
        assert_eq!(
            located[1].thumb_path.as_deref(),
            Some(Path::new("/thumbs/paris.jpg"))
        );
        assert_eq!(located[2].thumb_path, None);
    }

    #[test]
    fn a_photo_scope_shows_exactly_its_photos() {
        let catalog = catalog();
        let scope = Scope::Photos(Arc::from(vec![
            "munich".to_string(),
            "paris".to_string(),
            // Inzwischen gelöscht — fällt still heraus.
            "gone".to_string(),
        ]));
        let ids: Vec<_> = catalog
            .page_in(&scope, 0, 10)
            .unwrap()
            .iter()
            .map(|p| p.id().to_string())
            .collect();
        assert_eq!(ids, ["paris", "munich"]);
        assert_eq!(catalog.count_in(&scope).unwrap(), 2);
        let second: Vec<_> = catalog
            .page_in(&scope, 1, 10)
            .unwrap()
            .iter()
            .map(|p| p.id().to_string())
            .collect();
        assert_eq!(second, ["munich"]);
        let months = catalog.month_groups_in(&scope).unwrap();
        assert_eq!(months.len(), 1);
        assert_eq!(months[0].count, 2);

        let empty = Scope::Photos(Arc::from(Vec::<String>::new()));
        assert_eq!(catalog.count_in(&empty).unwrap(), 0);
    }

    #[test]
    fn ids_with_quotes_survive_the_json_list() {
        let catalog = Catalog::open_in_memory().unwrap();
        catalog
            .upsert_record(&record("a\"b\\c", 1, Some((1.0, 1.0))))
            .unwrap();
        let scope = Scope::Photos(Arc::from(vec!["a\"b\\c".to_string()]));
        assert_eq!(catalog.count_in(&scope).unwrap(), 1);
    }

    fn plan(catalog: &Catalog, sql: &str) -> String {
        let mut stmt = catalog
            .conn
            .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
            .unwrap();
        // Für den Plan zählen nur die Platzhalter, nicht ihre Werte.
        let nulls = vec![rusqlite::types::Null; stmt.parameter_count()];
        let rows = stmt
            .query_map(rusqlite::params_from_iter(nulls), |row| {
                row.get::<_, String>(3)
            })
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        rows.join("\n")
    }

    /// Die Bibliothek muss ihre Seiten weiter direkt aus dem Index lesen —
    /// der neue Ausschnitt darf daran nichts ändern.
    #[test]
    fn pages_read_the_capture_index_without_sorting() {
        let catalog = catalog();
        for scope in [
            Scope::Library,
            Scope::Photos(Arc::from(vec!["paris".to_string()])),
        ] {
            let (source, _) = scope.source(3);
            let plan = plan(&catalog, &page_sql(&source));
            assert!(plan.contains("idx_photos_capture"), "{scope:?}: {plan}");
            assert!(!plan.contains("TEMP B-TREE"), "{scope:?}: {plan}");
            if scope == Scope::Library {
                assert!(!plan.contains("album_photos"), "{plan}");
                assert!(!plan.contains("json_each"), "{plan}");
            }
        }
        let plan = plan(&catalog, &located_sql());
        assert!(plan.contains("idx_photos_capture"), "{plan}");
        assert!(!plan.contains("TEMP B-TREE"), "{plan}");
    }
}
