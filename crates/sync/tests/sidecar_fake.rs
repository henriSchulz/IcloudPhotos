//! Integrationstests gegen den echten Sidecar-Prozess im Fake-Modus.
//!
//! Der Fake-Modus importiert `pyicloud` nicht, ein blankes `python3` genügt.
//! Damit deckt der Test Prozessstart, NDJSON-Parsing, Batching und den
//! 2FA-Handshake ab, ohne Zugangsdaten oder Netz.

use std::path::PathBuf;

use icloud_photos_cache::Catalog;
use icloud_photos_sync::{Prompts, SecretRequest, SidecarOptions, SyncError, SyncEvent, run_sync};
use tokio::sync::mpsc;

fn script() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../sidecar/pyicloud_sync.py")
        .canonicalize()
        .expect("Sidecar-Skript nicht gefunden")
}

fn python() -> Option<String> {
    if let Ok(path) = std::env::var("ICLOUD_PHOTOS_PYTHON") {
        return Some(path);
    }
    std::process::Command::new("python3")
        .arg("--version")
        .output()
        .ok()
        .filter(|out| out.status.success())
        .map(|_| "python3".to_string())
}

/// Gibt `None` zurück, wenn kein Python verfügbar ist — der Test gilt dann als
/// bestanden, statt auf Maschinen ohne Python rot zu werden.
macro_rules! python_or_skip {
    () => {
        match python() {
            Some(python) => python,
            None => {
                eprintln!("übersprungen: kein python3 gefunden");
                return;
            }
        }
    };
}

fn options(python: &str, fake: u64) -> SidecarOptions {
    let mut options = SidecarOptions::new(python, script(), "tester@example.com");
    options.fake = Some(fake);
    options
}

#[tokio::test]
async fn stores_all_records_from_fake_run() {
    let python = python_or_skip!();
    let mut catalog = Catalog::open_in_memory().unwrap();

    // Mehr als BATCH_SIZE (256), damit auch das Zwischen-Flushen läuft.
    let summary = run_sync(&mut catalog, &options(&python, 300), &Prompts::default())
        .await
        .unwrap();

    assert_eq!(summary.stored, 300);
    assert_eq!(summary.reported, 300);
    assert!(summary.cursor.is_some());
    assert_eq!(catalog.count().unwrap(), 300);

    let newest = catalog.page(0, 1).unwrap().remove(0);
    assert_eq!(newest.record.width, Some(4032));
    assert!(newest.record.checksum.is_some());
}

#[tokio::test]
async fn reports_progress_and_auth() {
    let python = python_or_skip!();
    let mut catalog = Catalog::open_in_memory().unwrap();
    let (tx, mut rx) = mpsc::unbounded_channel();

    run_sync(
        &mut catalog,
        &options(&python, 300),
        &Prompts::events_only(tx),
    )
    .await
    .unwrap();

    let mut events = Vec::new();
    while let Ok(event) = rx.try_recv() {
        events.push(event);
    }

    assert!(events.contains(&SyncEvent::Authenticated { trusted: true }));
    let progress: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            SyncEvent::Progress { stored } => Some(*stored),
            _ => None,
        })
        .collect();

    // Der Erstaufbau meldet regelmaessig Zwischenstaende, damit ein Abbruch
    // nicht den ganzen Durchlauf kostet.
    assert!(
        progress.len() > 2,
        "zu wenige Zwischenstaende: {progress:?}"
    );
    assert!(
        progress.windows(2).all(|w| w[0] <= w[1]),
        "Fortschritt muss monoton sein: {progress:?}"
    );
    assert_eq!(progress.last().copied(), Some(300));
}

#[tokio::test]
async fn two_factor_code_is_forwarded_to_stdin() {
    let python = python_or_skip!();
    let mut catalog = Catalog::open_in_memory().unwrap();

    let mut options = options(&python, 2);
    options.fake_two_factor = true;

    let (tx, mut rx) = mpsc::unbounded_channel::<SecretRequest>();
    let responder = tokio::spawn(async move {
        let request = rx.recv().await.expect("2FA wurde nicht angefragt");
        request.send("123456".to_string()).unwrap();
    });

    let summary = run_sync(
        &mut catalog,
        &options,
        &Prompts {
            two_factor: Some(tx),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    responder.await.unwrap();

    assert_eq!(summary.stored, 2);
}

#[tokio::test]
async fn wrong_two_factor_code_fails_the_sync() {
    let python = python_or_skip!();
    let mut catalog = Catalog::open_in_memory().unwrap();

    let mut options = options(&python, 2);
    options.fake_two_factor = true;

    let (tx, mut rx) = mpsc::unbounded_channel::<SecretRequest>();
    tokio::spawn(async move {
        let request = rx.recv().await.unwrap();
        request.send("000000".to_string()).unwrap();
    });

    let error = run_sync(
        &mut catalog,
        &options,
        &Prompts {
            two_factor: Some(tx),
            ..Default::default()
        },
    )
    .await
    .unwrap_err();

    assert!(
        matches!(&error, SyncError::Sidecar { kind, .. } if kind == "auth"),
        "unerwartet: {error:?}"
    );
    assert_eq!(catalog.count().unwrap(), 0);
}

#[tokio::test]
async fn missing_two_factor_channel_aborts() {
    let python = python_or_skip!();
    let mut catalog = Catalog::open_in_memory().unwrap();

    let mut options = options(&python, 2);
    options.fake_two_factor = true;

    let error = run_sync(&mut catalog, &options, &Prompts::default())
        .await
        .unwrap_err();
    assert!(
        matches!(error, SyncError::TwoFactorAborted),
        "unerwartet: {error:?}"
    );
}

#[tokio::test]
async fn resync_is_idempotent() {
    let python = python_or_skip!();
    let mut catalog = Catalog::open_in_memory().unwrap();

    run_sync(&mut catalog, &options(&python, 10), &Prompts::default())
        .await
        .unwrap();
    run_sync(&mut catalog, &options(&python, 10), &Prompts::default())
        .await
        .unwrap();

    assert_eq!(
        catalog.count().unwrap(),
        10,
        "derselbe Lauf darf nicht duplizieren"
    );
}

#[tokio::test]
async fn missing_interpreter_is_a_spawn_error() {
    let mut catalog = Catalog::open_in_memory().unwrap();
    let error = run_sync(
        &mut catalog,
        &options("definitiv-kein-python", 1),
        &Prompts::default(),
    )
    .await
    .unwrap_err();
    assert!(
        matches!(error, SyncError::Spawn { .. }),
        "unerwartet: {error:?}"
    );
}

#[tokio::test]
async fn sync_downloads_thumbnails() {
    let python = python_or_skip!();
    let mut catalog = Catalog::open_in_memory().unwrap();
    let dir = tempfile::tempdir().unwrap();

    let mut options = options(&python, 5);
    options.thumbnail_dir = Some(dir.path().to_path_buf());

    let summary = run_sync(&mut catalog, &options, &Prompts::default())
        .await
        .unwrap();

    assert_eq!(summary.stored, 5);
    assert_eq!(summary.thumbnails, 5);
    assert_eq!(summary.thumbnails_failed, 0);

    // Jede Katalogzeile zeigt auf eine Datei, die es wirklich gibt.
    for photo in catalog.page(0, 10).unwrap() {
        let path = photo.thumb_path.expect("thumb_path fehlt");
        assert!(path.is_file(), "{} existiert nicht", path.display());
        assert!(photo.thumb_bytes.unwrap() > 0);
        assert_eq!(path.metadata().unwrap().len(), photo.thumb_bytes.unwrap());
    }
}

#[tokio::test]
async fn thumbnails_survive_a_resync() {
    let python = python_or_skip!();
    let mut catalog = Catalog::open_in_memory().unwrap();
    let dir = tempfile::tempdir().unwrap();

    let mut options = options(&python, 3);
    options.thumbnail_dir = Some(dir.path().to_path_buf());

    run_sync(&mut catalog, &options, &Prompts::default())
        .await
        .unwrap();
    let before: Vec<_> = catalog
        .page(0, 10)
        .unwrap()
        .into_iter()
        .map(|p| p.thumb_path)
        .collect();

    run_sync(&mut catalog, &options, &Prompts::default())
        .await
        .unwrap();
    let after: Vec<_> = catalog
        .page(0, 10)
        .unwrap()
        .into_iter()
        .map(|p| p.thumb_path)
        .collect();

    assert_eq!(before, after, "ein Re-Sync darf Vorschauen nicht verlieren");
    assert!(catalog.ids_without_thumbnail(10).unwrap().is_empty());
}

#[tokio::test]
async fn thumbnail_events_reach_the_ui() {
    let python = python_or_skip!();
    let mut catalog = Catalog::open_in_memory().unwrap();
    let dir = tempfile::tempdir().unwrap();

    let mut options = options(&python, 4);
    options.thumbnail_dir = Some(dir.path().to_path_buf());

    let (tx, mut rx) = mpsc::unbounded_channel();
    run_sync(&mut catalog, &options, &Prompts::events_only(tx))
        .await
        .unwrap();

    let mut counts = Vec::new();
    while let Ok(event) = rx.try_recv() {
        if let SyncEvent::Thumbnails { stored } = event {
            counts.push(stored);
        }
    }
    assert_eq!(
        counts,
        [1, 2, 3, 4],
        "je Vorschau eine Meldung, aufsteigend"
    );
}

#[tokio::test]
async fn backfill_sends_ids_only_after_authentication() {
    let python = python_or_skip!();
    let mut catalog = Catalog::open_in_memory().unwrap();
    let dir = tempfile::tempdir().unwrap();

    // Erst einen Sync ohne Vorschauen, damit Katalogzeilen ohne thumb_path da sind.
    run_sync(&mut catalog, &options(&python, 3), &Prompts::default())
        .await
        .unwrap();
    let missing = catalog.ids_without_thumbnail(10).unwrap();
    assert_eq!(missing.len(), 3);

    // Der Fake-Sidecar verlangt hier 2FA. Wuerden die IDs vor dem Code
    // rausgehen, verschluckte der Sidecar die erste ID als 2FA-Code.
    let mut options = options(&python, 3);
    options.thumbnail_dir = Some(dir.path().to_path_buf());
    options.fetch_thumbnails_for = missing;
    options.fake_two_factor = true;

    let (tx, mut rx) = mpsc::unbounded_channel::<SecretRequest>();
    tokio::spawn(async move {
        let request = rx.recv().await.expect("2FA wurde nicht angefragt");
        request.send("123456".to_string()).unwrap();
    });

    let summary = run_sync(
        &mut catalog,
        &options,
        &Prompts {
            two_factor: Some(tx),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(summary.thumbnails, 3);
    assert!(catalog.ids_without_thumbnail(10).unwrap().is_empty());
}

#[tokio::test]
async fn password_is_forwarded_to_stdin() {
    let python = python_or_skip!();
    let mut catalog = Catalog::open_in_memory().unwrap();

    let mut options = options(&python, 3);
    options.fake_password = true;

    let (tx, mut rx) = mpsc::unbounded_channel::<SecretRequest>();
    let responder = tokio::spawn(async move {
        let request = rx.recv().await.expect("Passwort wurde nicht angefragt");
        request.send("geheim".to_string()).unwrap();
    });

    let summary = run_sync(
        &mut catalog,
        &options,
        &Prompts {
            password: Some(tx),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    responder.await.unwrap();

    assert_eq!(summary.stored, 3);
}

#[tokio::test]
async fn wrong_password_is_reported_as_credentials_error() {
    let python = python_or_skip!();
    let mut catalog = Catalog::open_in_memory().unwrap();

    let mut options = options(&python, 3);
    options.fake_password = true;

    let (tx, mut rx) = mpsc::unbounded_channel::<SecretRequest>();
    tokio::spawn(async move {
        let request = rx.recv().await.unwrap();
        request.send("falsch".to_string()).unwrap();
    });

    let error = run_sync(
        &mut catalog,
        &options,
        &Prompts {
            password: Some(tx),
            ..Default::default()
        },
    )
    .await
    .unwrap_err();

    // Eigene Fehlerart, damit die UI gezielt erneut fragen kann.
    assert!(
        matches!(error, SyncError::Credentials { .. }),
        "unerwartet: {error:?}"
    );
}

#[tokio::test]
async fn password_and_two_factor_are_asked_in_order() {
    let python = python_or_skip!();
    let mut catalog = Catalog::open_in_memory().unwrap();

    let mut options = options(&python, 2);
    options.fake_password = true;
    options.fake_two_factor = true;

    let (password_tx, mut password_rx) = mpsc::unbounded_channel::<SecretRequest>();
    let (tfa_tx, mut tfa_rx) = mpsc::unbounded_channel::<SecretRequest>();
    let (event_tx, mut event_rx) = mpsc::unbounded_channel();

    tokio::spawn(async move {
        password_rx
            .recv()
            .await
            .unwrap()
            .send("geheim".into())
            .unwrap();
    });
    tokio::spawn(async move {
        tfa_rx.recv().await.unwrap().send("123456".into()).unwrap();
    });

    let summary = run_sync(
        &mut catalog,
        &options,
        &Prompts {
            events: Some(event_tx),
            password: Some(password_tx),
            two_factor: Some(tfa_tx),
        },
    )
    .await
    .unwrap();

    assert_eq!(summary.stored, 2);

    let mut order = Vec::new();
    while let Ok(event) = event_rx.try_recv() {
        match event {
            SyncEvent::PasswordRequired => order.push("password"),
            SyncEvent::TwoFactorRequired => order.push("2fa"),
            SyncEvent::Authenticated { .. } => order.push("ok"),
            _ => {}
        }
    }
    assert_eq!(order, ["password", "2fa", "ok"]);
}

#[tokio::test]
async fn missing_password_channel_aborts() {
    let python = python_or_skip!();
    let mut catalog = Catalog::open_in_memory().unwrap();

    let mut options = options(&python, 2);
    options.fake_password = true;

    let error = run_sync(&mut catalog, &options, &Prompts::default())
        .await
        .unwrap_err();
    assert!(
        matches!(error, SyncError::PasswordAborted),
        "unerwartet: {error:?}"
    );
}

/// `photos.all` ist nicht nach `added_date` sortiert. Ein Sidecar, der beim
/// ersten "zu alten" Asset abbricht, verschluckt den Rest der Bibliothek.
#[tokio::test]
async fn since_filters_without_ending_the_walk() {
    let python = python_or_skip!();

    // Erst alles holen, um die Datumslage zu kennen.
    let mut full = Catalog::open_in_memory().unwrap();
    run_sync(&mut full, &options(&python, 24), &Prompts::default())
        .await
        .unwrap();

    let mut dates: Vec<_> = full
        .page(0, 100)
        .unwrap()
        .into_iter()
        .map(|p| p.record.added_date)
        .collect();
    dates.sort();
    let cutoff = dates[dates.len() / 2];
    let expected = dates.iter().filter(|d| **d > cutoff).count() as u64;
    assert!(
        expected > 0 && expected < 24,
        "Schnitt muss mittendrin liegen"
    );

    // Jetzt derselbe Lauf mit Cursor. Die Zahl muss exakt stimmen — bei einem
    // vorzeitigen Abbruch käme deutlich weniger an.
    let mut filtered = Catalog::open_in_memory().unwrap();
    let mut options = options(&python, 24);
    options.since = Some(cutoff);

    let summary = run_sync(&mut filtered, &options, &Prompts::default())
        .await
        .unwrap();

    assert_eq!(summary.stored, expected);
    assert_eq!(filtered.count().unwrap(), expected);
    for photo in filtered.page(0, 100).unwrap() {
        assert!(photo.record.added_date > cutoff);
    }
}

/// Ein abgebrochener Sync muss sich durch einen erneuten Lauf vervollstaendigen.
#[tokio::test]
async fn interrupted_sync_is_completed_by_the_next_run() {
    let python = python_or_skip!();
    let dir = tempfile::tempdir().unwrap();
    let mut catalog = Catalog::open_in_memory().unwrap();

    let mut options = options(&python, 12);
    options.thumbnail_dir = Some(dir.path().to_path_buf());

    run_sync(&mut catalog, &options, &Prompts::default())
        .await
        .unwrap();
    assert_eq!(catalog.count().unwrap(), 12);

    // Abbruch nachstellen: ein Teil der Bibliothek kam nie im Katalog an.
    let keep: Vec<String> = catalog
        .page(0, 5)
        .unwrap()
        .iter()
        .map(|p| p.id().to_string())
        .collect();
    let mut fresh = Catalog::open_in_memory().unwrap();
    for photo in catalog.page(0, 100).unwrap() {
        if keep.contains(&photo.id().to_string()) {
            fresh.upsert_record(&photo.record).unwrap();
        }
    }
    assert_eq!(fresh.count().unwrap(), 5);

    // Der naechste Lauf holt den Rest — ohne Cursor, so wie die App ihn startet.
    let summary = run_sync(&mut fresh, &options, &Prompts::default())
        .await
        .unwrap();

    assert_eq!(
        summary.stored, 12,
        "der volle Durchlauf emittiert alles erneut"
    );
    assert_eq!(
        fresh.count().unwrap(),
        12,
        "die fehlenden Fotos sind nachgetragen"
    );
    assert!(fresh.ids_without_thumbnail(20).unwrap().is_empty());
}

// --------------------------------------------------------------------------
// Inkrementeller Abgleich über den CloudKit-Zone-Token
// --------------------------------------------------------------------------

/// Schicht 1: unveränderter Token heißt, es gibt schlicht nichts zu tun.
#[tokio::test]
async fn unchanged_token_skips_the_whole_walk() {
    let python = python_or_skip!();
    let mut catalog = Catalog::open_in_memory().unwrap();

    // Erstaufbau.
    let first = run_sync(&mut catalog, &options(&python, 6), &Prompts::default())
        .await
        .unwrap();
    assert!(first.baseline_complete);
    assert_eq!(catalog.count().unwrap(), 6);

    let state = catalog.sync_state().unwrap();
    assert!(state.baseline_done, "Erstaufbau muss vermerkt sein");
    assert_eq!(state.zone_token.as_deref(), Some("fake-token-6"));

    // Zweiter Lauf mit demselben Token.
    let mut options = options(&python, 6);
    options.since_token = state.zone_token.clone();

    let (tx, mut rx) = mpsc::unbounded_channel();
    let second = run_sync(&mut catalog, &options, &Prompts::events_only(tx))
        .await
        .unwrap();

    assert!(second.unchanged);
    assert_eq!(second.stored, 0, "kein einziger Datensatz wurde geholt");
    assert_eq!(second.deleted, 0);
    assert_eq!(catalog.count().unwrap(), 6);

    let mut events = Vec::new();
    while let Ok(event) = rx.try_recv() {
        events.push(event);
    }
    assert!(events.contains(&SyncEvent::Unchanged));
}

/// Schicht 2: neue Fotos kommen als Delta, nicht als voller Durchlauf.
#[tokio::test]
async fn changed_token_fetches_only_the_difference() {
    let python = python_or_skip!();
    let dir = tempfile::tempdir().unwrap();
    let mut catalog = Catalog::open_in_memory().unwrap();

    let mut base = options(&python, 6);
    base.thumbnail_dir = Some(dir.path().to_path_buf());
    run_sync(&mut catalog, &base, &Prompts::default())
        .await
        .unwrap();

    // Die Bibliothek ist auf 9 gewachsen.
    let mut options = options(&python, 9);
    options.thumbnail_dir = Some(dir.path().to_path_buf());
    options.since_token = catalog.sync_state().unwrap().zone_token;

    let summary = run_sync(&mut catalog, &options, &Prompts::default())
        .await
        .unwrap();

    assert_eq!(summary.stored, 3, "nur die drei neuen, nicht alle neun");
    assert_eq!(summary.thumbnails, 3);
    assert!(!summary.baseline_complete, "kein voller Durchlauf");
    assert_eq!(catalog.count().unwrap(), 9);
    assert_eq!(
        catalog.sync_state().unwrap().zone_token.as_deref(),
        Some("fake-token-9")
    );
}

/// Schicht 2: in iCloud gelöschte Fotos verschwinden samt Cache-Dateien.
#[tokio::test]
async fn deletions_remove_rows_and_cached_files() {
    let python = python_or_skip!();
    let dir = tempfile::tempdir().unwrap();
    let mut catalog = Catalog::open_in_memory().unwrap();

    let mut base = options(&python, 6);
    base.thumbnail_dir = Some(dir.path().to_path_buf());
    run_sync(&mut catalog, &base, &Prompts::default())
        .await
        .unwrap();

    let doomed = catalog.get("fake-00005").unwrap().unwrap();
    let thumb = doomed.thumb_path.expect("Vorschau fehlt");
    assert!(thumb.is_file());

    // Die Bibliothek ist auf 4 geschrumpft.
    let mut options = options(&python, 4);
    options.thumbnail_dir = Some(dir.path().to_path_buf());
    options.since_token = catalog.sync_state().unwrap().zone_token;

    let summary = run_sync(&mut catalog, &options, &Prompts::default())
        .await
        .unwrap();

    assert_eq!(summary.deleted, 2);
    assert_eq!(catalog.count().unwrap(), 4);
    assert_eq!(catalog.get("fake-00005").unwrap(), None);
    assert!(!thumb.is_file(), "die Cache-Datei muss mit verschwinden");
}

/// Ein abgebrochener Erstaufbau darf den Token nicht übernehmen — sonst
/// zieht der nächste Lauf ein Delta und sieht den Rest der Bibliothek nie.
#[tokio::test]
async fn incomplete_baseline_does_not_claim_the_token() {
    let python = python_or_skip!();
    let mut catalog = Catalog::open_in_memory().unwrap();

    let mut options = options(&python, 8);
    options.limit = Some(3);

    let summary = run_sync(&mut catalog, &options, &Prompts::default())
        .await
        .unwrap();

    assert!(!summary.baseline_complete);
    assert_eq!(
        catalog.sync_state().unwrap().zone_token,
        None,
        "ein halber Durchlauf verdient den Token nicht"
    );
    assert!(!catalog.sync_state().unwrap().baseline_done);
}

/// Der Erstaufbau setzt an der gemerkten Position fort.
#[tokio::test]
async fn baseline_resumes_from_the_stored_offset() {
    let python = python_or_skip!();
    let mut catalog = Catalog::open_in_memory().unwrap();

    // Ersten Teil holen und so tun, als wäre dort abgebrochen worden.
    let mut first = options(&python, 10);
    first.limit = Some(4);
    run_sync(&mut catalog, &first, &Prompts::default())
        .await
        .unwrap();
    assert_eq!(catalog.count().unwrap(), 4);
    catalog
        .set_baseline_progress(4, false, chrono::Utc::now())
        .unwrap();

    // Fortsetzung ab Offset 4.
    let mut second = options(&python, 10);
    second.offset = catalog.sync_state().unwrap().baseline_offset;

    let summary = run_sync(&mut catalog, &second, &Prompts::default())
        .await
        .unwrap();

    assert_eq!(
        summary.stored, 6,
        "nur der Rest, nicht die ganze Bibliothek"
    );
    assert!(summary.baseline_complete);
    assert_eq!(catalog.count().unwrap(), 10);
    assert_eq!(
        catalog.sync_state().unwrap().zone_token.as_deref(),
        Some("fake-token-10")
    );
}
