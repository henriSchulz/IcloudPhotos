//! Integrationstests gegen den echten Sidecar-Prozess im Fake-Modus.
//!
//! Der Fake-Modus importiert `pyicloud` nicht, ein blankes `python3` genügt.
//! Damit deckt der Test Prozessstart, NDJSON-Parsing, Batching und den
//! 2FA-Handshake ab, ohne Zugangsdaten oder Netz.

use std::path::PathBuf;

use icloud_photos_cache::Catalog;
use icloud_photos_sync::sidecar::TwoFactorRequest;
use icloud_photos_sync::{SidecarOptions, SyncError, SyncEvent, run_sync};
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
    let summary = run_sync(&mut catalog, &options(&python, 300), None, None)
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

    run_sync(&mut catalog, &options(&python, 300), Some(tx), None)
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
    assert_eq!(progress, [256, 300], "ein Zwischenstand und der Abschluss");
}

#[tokio::test]
async fn two_factor_code_is_forwarded_to_stdin() {
    let python = python_or_skip!();
    let mut catalog = Catalog::open_in_memory().unwrap();

    let mut options = options(&python, 2);
    options.fake_two_factor = true;

    let (tx, mut rx) = mpsc::unbounded_channel::<TwoFactorRequest>();
    let responder = tokio::spawn(async move {
        let request = rx.recv().await.expect("2FA wurde nicht angefragt");
        request.send("123456".to_string()).unwrap();
    });

    let summary = run_sync(&mut catalog, &options, None, Some(tx))
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

    let (tx, mut rx) = mpsc::unbounded_channel::<TwoFactorRequest>();
    tokio::spawn(async move {
        let request = rx.recv().await.unwrap();
        request.send("000000".to_string()).unwrap();
    });

    let error = run_sync(&mut catalog, &options, None, Some(tx))
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

    let error = run_sync(&mut catalog, &options, None, None)
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

    run_sync(&mut catalog, &options(&python, 10), None, None)
        .await
        .unwrap();
    run_sync(&mut catalog, &options(&python, 10), None, None)
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
        None,
        None,
    )
    .await
    .unwrap_err();
    assert!(
        matches!(error, SyncError::Spawn { .. }),
        "unerwartet: {error:?}"
    );
}
