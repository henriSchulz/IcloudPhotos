//! Periodisches Aufräumen als Sicherheitsnetz.
//!
//! Nach einem Absturz können Katalog und Dateisystem auseinanderlaufen: eine
//! Datei ohne Katalogzeile (Download abgebrochen) oder eine Katalogzeile ohne
//! Datei (Löschen ohne Commit). Beides korrigiert ein Sweep.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::{CacheError, Catalog};

/// Ergebnis eines Housekeeping-Laufs.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct HousekeepingReport {
    /// Dateien ohne zugehörige Katalogzeile, die entfernt wurden.
    pub orphan_files_removed: usize,
    pub orphan_bytes_removed: u64,
    /// Katalogzeilen, deren Datei fehlte und die als „nicht gecacht" markiert
    /// wurden.
    pub stale_rows_cleared: usize,
}

impl HousekeepingReport {
    pub fn is_clean(&self) -> bool {
        self.orphan_files_removed == 0 && self.stale_rows_cleared == 0
    }
}

/// Gleicht Full-Res-Cache-Verzeichnis und Katalog ab.
pub fn sweep(catalog: &Catalog, full_res_dir: &Path) -> Result<HousekeepingReport, CacheError> {
    let mut report = HousekeepingReport::default();
    let mut referenced: HashSet<PathBuf> = HashSet::new();

    // Richtung 1: Katalogzeilen, deren Datei verschwunden ist.
    for candidate in catalog.lru_candidates()? {
        if candidate.path.is_file() {
            referenced.insert(candidate.path);
        } else {
            tracing::warn!(
                id = %candidate.id,
                path = %candidate.path.display(),
                "Cache-Datei fehlt, Zeile wird zurückgesetzt"
            );
            catalog.clear_original(&candidate.id)?;
            report.stale_rows_cleared += 1;
        }
    }

    // Richtung 2: Dateien, die kein Katalogeintrag mehr beansprucht.
    let entries = match std::fs::read_dir(full_res_dir) {
        Ok(entries) => entries,
        // Noch nie ein Original geladen — nichts aufzuräumen.
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(report),
        Err(source) => {
            return Err(CacheError::Io {
                path: full_res_dir.to_path_buf(),
                source,
            });
        }
    };

    for entry in entries {
        let entry = entry.map_err(|source| CacheError::Io {
            path: full_res_dir.to_path_buf(),
            source,
        })?;
        let path = entry.path();
        if !path.is_file() || referenced.contains(&path) {
            continue;
        }

        let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
        match std::fs::remove_file(&path) {
            Ok(()) => {
                tracing::info!(path = %path.display(), "verwaiste Cache-Datei entfernt");
                report.orphan_files_removed += 1;
                report.orphan_bytes_removed += size;
            }
            Err(err) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %err,
                    "verwaiste Cache-Datei konnte nicht entfernt werden"
                );
            }
        }
    }

    Ok(report)
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use icloud_photos_core::model::PhotoRecord;

    use super::*;

    fn seed(catalog: &Catalog, id: &str, path: &Path, bytes: u64) {
        catalog
            .upsert_record(&PhotoRecord {
                id: id.to_string(),
                master_id: None,
                filename: format!("{id}.heic"),
                added_date: Utc::now(),
                asset_date: None,
                checksum: None,
                width: None,
                height: None,
                item_type: None,
            })
            .unwrap();
        catalog
            .set_original_cached(id, path, bytes, Utc::now())
            .unwrap();
    }

    #[test]
    fn removes_files_without_catalog_row() {
        let catalog = Catalog::open_in_memory().unwrap();
        let dir = tempfile::tempdir().unwrap();

        let kept = dir.path().join("kept.heic");
        std::fs::write(&kept, b"abcd").unwrap();
        seed(&catalog, "kept", &kept, 4);

        let orphan = dir.path().join("orphan.heic");
        std::fs::write(&orphan, b"abcdefgh").unwrap();

        let report = sweep(&catalog, dir.path()).unwrap();

        assert_eq!(report.orphan_files_removed, 1);
        assert_eq!(report.orphan_bytes_removed, 8);
        assert!(!orphan.exists());
        assert!(kept.exists());
    }

    #[test]
    fn clears_rows_without_file() {
        let catalog = Catalog::open_in_memory().unwrap();
        let dir = tempfile::tempdir().unwrap();

        let gone = dir.path().join("gone.heic");
        std::fs::write(&gone, b"abcd").unwrap();
        seed(&catalog, "gone", &gone, 4);
        std::fs::remove_file(&gone).unwrap();

        let report = sweep(&catalog, dir.path()).unwrap();

        assert_eq!(report.stale_rows_cleared, 1);
        assert_eq!(catalog.cached_bytes().unwrap(), 0);
        assert!(!catalog.get("gone").unwrap().unwrap().is_orig_cached);
    }

    #[test]
    fn clean_state_changes_nothing() {
        let catalog = Catalog::open_in_memory().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.heic");
        std::fs::write(&path, b"abcd").unwrap();
        seed(&catalog, "a", &path, 4);

        assert!(sweep(&catalog, dir.path()).unwrap().is_clean());
        assert!(path.exists());
        assert_eq!(catalog.cached_bytes().unwrap(), 4);
    }

    #[test]
    fn missing_directory_is_not_an_error() {
        let catalog = Catalog::open_in_memory().unwrap();
        let report = sweep(&catalog, Path::new("/nonexistent/fullres")).unwrap();
        assert!(report.is_clean());
    }
}
