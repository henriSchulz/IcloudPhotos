//! Ein Katalog aus der Zeit vor Schema 4 muss sich öffnen lassen, ohne dass
//! Zeilen verloren gehen.
//!
//! Der Weg über `Catalog::open` ist der einzige, der die Migration wirklich
//! ausführt — ein frisch angelegter Katalog geht ihn nie.

use icloud_photos_cache::Catalog;
use rusqlite::Connection;

/// Legt eine Datenbank an, wie Schema 3 sie hinterlassen hat: ohne
/// `item_type` und ohne den Index auf den Aufnahmezeitpunkt.
fn schema_3(path: &std::path::Path) {
    let conn = Connection::open(path).unwrap();
    conn.execute_batch(
        r#"
        CREATE TABLE photos (
            id               TEXT PRIMARY KEY,
            master_id        TEXT,
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
        CREATE TABLE sync_state (
            id               INTEGER PRIMARY KEY CHECK (id = 1),
            zone_sync_token  TEXT,
            baseline_done    INTEGER NOT NULL DEFAULT 0,
            baseline_offset  INTEGER NOT NULL DEFAULT 0,
            last_baseline_at TEXT
        );
        INSERT INTO sync_state (id, zone_sync_token, baseline_done) VALUES (1, 'token-alt', 1);

        INSERT INTO photos (id, filename, added_date, asset_date, thumb_path, is_orig_cached)
        VALUES
            ('a', 'IMG_0001.HEIC', '2026-06-01T00:00:00.000000Z', '2019-10-17T09:00:00.000000Z', '/thumbs/a.jpg', 0),
            ('b', 'IMG_0002.MOV',  '2025-01-01T00:00:00.000000Z', '2024-12-24T18:00:00.000000Z', '/thumbs/b.jpg', 0);
        PRAGMA user_version = 3;
        "#,
    )
    .unwrap();
}

#[test]
fn a_schema_3_catalog_survives_the_upgrade() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("catalog.sqlite");
    schema_3(&path);

    let catalog = Catalog::open(&path).unwrap();

    // Nichts verloren, und der Sync-Zustand steht noch.
    assert_eq!(catalog.count().unwrap(), 2);
    assert_eq!(
        catalog.sync_state().unwrap().zone_token.as_deref(),
        Some("token-alt")
    );
    assert!(catalog.sync_state().unwrap().baseline_done);

    // Die neue Spalte ist da und leer — der Sync fuellt sie nach.
    let photo = catalog.get("a").unwrap().unwrap();
    assert_eq!(photo.record.item_type, None);
    assert_eq!(
        photo.thumb_path.as_ref().unwrap().to_string_lossy(),
        "/thumbs/a.jpg"
    );

    // Ohne item_type entscheidet die Endung — die Videos im Bestand bleiben
    // also auch vor dem naechsten Vollabgleich Videos.
    assert!(catalog.get("b").unwrap().unwrap().record.is_video());
    assert!(!photo.record.is_video());

    // Und die Reihenfolge richtet sich ab jetzt nach dem Aufnahmedatum: "a"
    // wurde zuletzt hinzugefuegt, aber 2019 aufgenommen.
    let ids: Vec<_> = catalog
        .page(0, 10)
        .unwrap()
        .iter()
        .map(|p| p.id().to_string())
        .collect();
    assert_eq!(ids, ["b", "a"]);
}

#[test]
fn opening_twice_is_harmless() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("catalog.sqlite");
    schema_3(&path);

    drop(Catalog::open(&path).unwrap());
    // Der zweite Lauf darf nicht ueber die bereits angelegte Spalte stolpern.
    let catalog = Catalog::open(&path).unwrap();
    assert_eq!(catalog.count().unwrap(), 2);
}
