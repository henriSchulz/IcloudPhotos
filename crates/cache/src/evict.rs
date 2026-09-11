//! LRU-Eviction für den Full-Res-Cache.

use icloud_photos_core::config::Cache as CacheConfig;

use crate::{CacheError, Catalog};

/// Ergebnis eines Eviction-Laufs.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct EvictionReport {
    pub evicted: usize,
    pub freed_bytes: u64,
    /// Belegung nach dem Lauf.
    pub bytes_after: u64,
}

impl EvictionReport {
    pub fn is_noop(&self) -> bool {
        self.evicted == 0
    }
}

/// Räumt den Full-Res-Cache auf, falls er das konfigurierte Limit überschreitet.
///
/// Läuft nach jedem Full-Res-Download. Unterhalb des Limits passiert nichts;
/// oberhalb wird bis auf `eviction_target_ratio * limit` heruntergeräumt, damit
/// nicht jedes einzelne neue Foto eine Eviction auslöst (Hysterese).
pub fn enforce_limit(
    catalog: &Catalog,
    config: &CacheConfig,
) -> Result<EvictionReport, CacheError> {
    let limit = config.limit_bytes();
    let target = config.target_bytes();
    let mut total = catalog.cached_bytes()?;

    let mut report = EvictionReport {
        bytes_after: total,
        ..Default::default()
    };
    if total <= limit {
        return Ok(report);
    }

    tracing::info!(
        total_bytes = total,
        limit_bytes = limit,
        target_bytes = target,
        "Full-Res-Cache über Limit, starte Eviction"
    );

    for candidate in catalog.lru_candidates()? {
        if total <= target {
            break;
        }

        match std::fs::remove_file(&candidate.path) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                // Datei bereits weg (z. B. nach einem Absturz). Die Zeile muss
                // trotzdem aufgeräumt werden, sonst bleibt die Summe zu hoch.
                tracing::debug!(path = %candidate.path.display(), "Cache-Datei fehlte bereits");
            }
            Err(err) => {
                // Zeile trotzdem freigeben, damit die Eviction nicht stecken
                // bleibt; die verwaiste Datei entfernt der Housekeeping-Lauf.
                tracing::warn!(
                    path = %candidate.path.display(),
                    error = %err,
                    "Cache-Datei konnte nicht gelöscht werden"
                );
            }
        }

        catalog.clear_original(&candidate.id)?;
        total = total.saturating_sub(candidate.bytes);
        report.evicted += 1;
        report.freed_bytes += candidate.bytes;
    }

    report.bytes_after = total;
    tracing::info!(
        evicted = report.evicted,
        freed_bytes = report.freed_bytes,
        bytes_after = report.bytes_after,
        "Eviction abgeschlossen"
    );
    Ok(report)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use chrono::{DateTime, TimeDelta, Utc};
    use icloud_photos_core::model::PhotoRecord;

    use super::*;

    struct Fixture {
        catalog: Catalog,
        dir: tempfile::TempDir,
        clock: DateTime<Utc>,
    }

    impl Fixture {
        fn new() -> Self {
            Self {
                catalog: Catalog::open_in_memory().unwrap(),
                dir: tempfile::tempdir().unwrap(),
                clock: DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
                    .unwrap()
                    .with_timezone(&Utc),
            }
        }

        /// Legt ein gecachtes Original an. Jeder Aufruf rückt die Uhr vor, die
        /// Reihenfolge der Aufrufe ist also die LRU-Reihenfolge.
        fn cache(&mut self, id: &str, bytes: u64) -> PathBuf {
            let path = self.dir.path().join(format!("{id}.heic"));
            std::fs::write(&path, vec![0u8; bytes as usize]).unwrap();

            self.catalog
                .upsert_record(&PhotoRecord {
                    id: id.to_string(),
                    filename: format!("{id}.heic"),
                    added_date: self.clock,
                    asset_date: None,
                    checksum: None,
                    width: None,
                    height: None,
                })
                .unwrap();
            self.catalog
                .set_original_cached(id, &path, bytes, self.clock)
                .unwrap();
            self.clock += TimeDelta::minutes(1);
            path
        }

        fn touch(&mut self, id: &str) {
            self.catalog.touch(id, self.clock).unwrap();
            self.clock += TimeDelta::minutes(1);
        }

        fn is_cached(&self, id: &str) -> bool {
            self.catalog.get(id).unwrap().unwrap().is_orig_cached
        }
    }

    fn config(limit_gb: f64, ratio: f64) -> CacheConfig {
        CacheConfig {
            full_res_cache_limit_gb: limit_gb,
            eviction_target_ratio: ratio,
        }
    }

    /// 1000 Bytes Limit, 900 Bytes Ziel.
    fn tiny_config() -> CacheConfig {
        config(1e-6, 0.9)
    }

    #[test]
    fn under_limit_is_a_noop() {
        let mut fx = Fixture::new();
        let path = fx.cache("a", 500);

        let report = enforce_limit(&fx.catalog, &tiny_config()).unwrap();
        assert!(report.is_noop());
        assert_eq!(report.bytes_after, 500);
        assert!(path.exists());
    }

    #[test]
    fn exactly_at_limit_is_a_noop() {
        let mut fx = Fixture::new();
        fx.cache("a", 1000);

        let report = enforce_limit(&fx.catalog, &tiny_config()).unwrap();
        assert!(report.is_noop(), "das Limit selbst ist noch erlaubt");
    }

    #[test]
    fn evicts_least_recently_used_first() {
        let mut fx = Fixture::new();
        let a = fx.cache("a", 400);
        let b = fx.cache("b", 400);
        let c = fx.cache("c", 400); // 1200 > 1000

        let report = enforce_limit(&fx.catalog, &tiny_config()).unwrap();

        assert_eq!(report.evicted, 1);
        assert_eq!(report.freed_bytes, 400);
        assert_eq!(report.bytes_after, 800);
        assert!(!a.exists(), "das älteste Original muss gelöscht sein");
        assert!(b.exists() && c.exists());
        assert!(!fx.is_cached("a"));
        assert!(fx.is_cached("b") && fx.is_cached("c"));
    }

    #[test]
    fn viewing_a_cached_photo_protects_it() {
        let mut fx = Fixture::new();
        let a = fx.cache("a", 400);
        let b = fx.cache("b", 400);

        // "a" wird nur angesehen, nicht neu geladen — das muss reichen, damit
        // stattdessen "b" evictet wird. Sonst wäre der Cache FIFO, nicht LRU.
        fx.touch("a");

        let c = fx.cache("c", 400);
        let report = enforce_limit(&fx.catalog, &tiny_config()).unwrap();

        assert_eq!(report.evicted, 1);
        assert!(!b.exists(), "b war am längsten ungenutzt");
        assert!(a.exists() && c.exists());
    }

    #[test]
    fn evicts_down_to_target_not_just_below_limit() {
        let mut fx = Fixture::new();
        for i in 0..6 {
            fx.cache(&format!("p{i}"), 200); // 1200 > 1000
        }

        let report = enforce_limit(&fx.catalog, &tiny_config()).unwrap();

        // Bis unter das Limit reichte eine Datei; bis auf 900 sind es zwei.
        assert_eq!(report.evicted, 2);
        assert_eq!(report.bytes_after, 800);
        assert!(report.bytes_after <= tiny_config().target_bytes());
    }

    #[test]
    fn missing_file_still_frees_the_row() {
        let mut fx = Fixture::new();
        let a = fx.cache("a", 600);
        fx.cache("b", 600);
        std::fs::remove_file(&a).unwrap(); // Absturz zwischen Löschen und Commit

        let report = enforce_limit(&fx.catalog, &tiny_config()).unwrap();

        assert_eq!(report.evicted, 1);
        assert!(!fx.is_cached("a"));
        assert_eq!(fx.catalog.cached_bytes().unwrap(), 600);
    }

    #[test]
    fn never_accessed_originals_are_evicted_first() {
        let mut fx = Fixture::new();
        let prefetched = fx.cache("prefetched", 400);
        let viewed = fx.cache("viewed", 400);
        fx.touch("viewed");
        fx.cache("fresh", 400);

        // Ein Prefetch ohne Ansehen hinterlässt keine Zugriffszeit.
        fx.catalog.touch_to_null_for_test("prefetched").unwrap();

        enforce_limit(&fx.catalog, &tiny_config()).unwrap();
        assert!(!prefetched.exists());
        assert!(viewed.exists());
    }

    #[test]
    fn ratio_of_one_disables_hysteresis() {
        let mut fx = Fixture::new();
        for i in 0..6 {
            fx.cache(&format!("p{i}"), 200); // 1200 > 1000
        }

        let report = enforce_limit(&fx.catalog, &config(1e-6, 1.0)).unwrap();
        assert_eq!(report.evicted, 1);
        assert_eq!(report.bytes_after, 1000);
    }
}
