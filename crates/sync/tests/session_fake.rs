//! Der dauerhaft laufende Sidecar für Originale auf Abruf.

use std::path::PathBuf;

use icloud_photos_cache::{Catalog, enforce_limit};
use icloud_photos_core::config::Cache as CacheConfig;
use icloud_photos_sync::{
    Done, Job, Prompts, SecretRequest, SidecarOptions, SidecarSession, SyncError,
};
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
async fn serves_several_originals_from_one_process() {
    let python = python_or_skip!();
    let dir = tempfile::tempdir().unwrap();

    let mut session = SidecarSession::start(&options(&python, 10), &Prompts::default())
        .await
        .unwrap();

    // Der Sinn des Serve-Modus: mehrere Downloads, ein Prozess, eine Anmeldung.
    for index in 0..3 {
        let id = format!("fake-{index:05}");
        let dest = dir.path().join(format!("{id}.png"));
        let original = session.fetch_original(&id, None, &dest).await.unwrap();

        assert_eq!(original.path, dest);
        assert!(original.bytes > 0);
        assert_eq!(dest.metadata().unwrap().len(), original.bytes);
    }

    session.shutdown().await;
}

#[tokio::test]
async fn unknown_asset_is_an_error_not_a_hang() {
    let python = python_or_skip!();
    let dir = tempfile::tempdir().unwrap();

    let mut session = SidecarSession::start(&options(&python, 4), &Prompts::default())
        .await
        .unwrap();

    let error = session
        .fetch_original("fake-99999", None, &dir.path().join("x.png"))
        .await
        .unwrap_err();
    // "missing" heisst: ein zweiter Anlauf braucht gar nicht erst zu starten.
    // Die App unterscheidet daran, ob sie die Sitzung erneuert.
    assert!(
        matches!(&error, SyncError::Sidecar { kind, .. } if kind == "missing"),
        "unerwartet: {error:?}"
    );

    // Die Sitzung muss danach weiter benutzbar sein.
    let ok = session
        .fetch_original("fake-00001", None, &dir.path().join("y.png"))
        .await
        .unwrap();
    assert!(ok.bytes > 0);

    session.shutdown().await;
}

#[tokio::test]
async fn session_asks_for_the_password_once() {
    let python = python_or_skip!();
    let dir = tempfile::tempdir().unwrap();

    let mut options = options(&python, 4);
    options.fake_password = true;

    let (tx, mut rx) = mpsc::unbounded_channel::<SecretRequest>();
    let asked = tokio::spawn(async move {
        let request = rx.recv().await.expect("Passwort wurde nicht angefragt");
        request.send("geheim".to_string()).unwrap();
        // Ein zweiter Download darf nicht erneut fragen.
        rx.recv().await.is_none()
    });

    let mut session = SidecarSession::start(
        &options,
        &Prompts {
            password: Some(tx),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    session
        .fetch_original("fake-00000", None, &dir.path().join("a.png"))
        .await
        .unwrap();
    session
        .fetch_original("fake-00001", None, &dir.path().join("b.png"))
        .await
        .unwrap();
    session.shutdown().await;

    assert!(asked.await.unwrap(), "nur eine einzige Passwortabfrage");
}

/// Der eigentliche Zweck des Ganzen: erst durch Full-Res-Downloads bekommt die
/// LRU-Eviction überhaupt etwas zu tun.
#[tokio::test]
async fn downloads_fill_the_cache_until_eviction_kicks_in() {
    let python = python_or_skip!();
    let dir = tempfile::tempdir().unwrap();
    let mut catalog = Catalog::open_in_memory().unwrap();

    // Erst den Katalog füllen.
    let mut sync_options = options(&python, 6);
    sync_options.thumbnail_dir = Some(dir.path().join("thumbs"));
    icloud_photos_sync::run_sync(&mut catalog, &sync_options, &Prompts::default())
        .await
        .unwrap();

    let mut session = SidecarSession::start(&options(&python, 6), &Prompts::default())
        .await
        .unwrap();

    let mut paths = Vec::new();
    for index in 0..6 {
        let id = format!("fake-{index:05}");
        let dest = dir.path().join("fullres").join(format!("{id}.png"));
        let original = session.fetch_original(&id, None, &dest).await.unwrap();
        let now = chrono::Utc::now();
        catalog
            .set_original_cached(&id, &original.path, original.bytes, now)
            .unwrap();
        paths.push(original.path);
    }
    session.shutdown().await;

    let total = catalog.cached_bytes().unwrap();
    assert!(total > 0, "ohne Downloads bliebe der Cache leer");
    assert_eq!(paths.iter().filter(|p| p.is_file()).count(), 6);

    // Limit knapp unter den Bestand legen, damit die Eviction greift.
    let config = CacheConfig {
        full_res_cache_limit_gb: (total as f64 * 0.6) / 1e9,
        eviction_target_ratio: 0.9,
    };
    let report = enforce_limit(&catalog, &config).unwrap();

    assert!(report.evicted > 0, "die Eviction haette laufen muessen");
    assert!(catalog.cached_bytes().unwrap() <= config.target_bytes());
    // Die zuerst geladenen Originale fliegen zuerst — echtes LRU.
    assert!(!paths[0].is_file());
    assert!(paths[5].is_file(), "das zuletzt benutzte bleibt");
}

#[tokio::test]
async fn deletes_and_uploads_through_the_same_process() {
    let python = python_or_skip!();
    let dir = tempfile::tempdir().unwrap();
    let mut session = SidecarSession::start(&options(&python, 10), &Prompts::default())
        .await
        .unwrap();

    let done = session
        .run(&Job::Delete {
            id: "fake-00001".into(),
            master: None,
        })
        .await
        .unwrap();
    assert_eq!(done, Done::Deleted);

    // Unbekanntes Foto: endgültig, kein zweiter Anlauf.
    match session
        .run(&Job::Delete {
            id: "nope".into(),
            master: None,
        })
        .await
    {
        Err(SyncError::Sidecar { kind, .. }) => assert_eq!(kind, "missing"),
        other => panic!("erwartet missing, bekam {other:?}"),
    }

    let file = dir.path().join("IMG_0001.JPG");
    std::fs::write(&file, b"jpeg").unwrap();
    let done = session.run(&Job::Upload { path: file }).await.unwrap();
    assert_eq!(done, Done::Uploaded { id: None });

    // Eine fehlende Datei ist ein Fehlschlag, kein Hänger.
    assert!(
        session
            .run(&Job::Upload {
                path: dir.path().join("missing.jpg"),
            })
            .await
            .is_err()
    );

    // Und danach liefert derselbe Prozess weiter Originale.
    let dest = dir.path().join("fake-00002.png");
    assert!(
        session
            .fetch_original("fake-00002", None, &dest)
            .await
            .is_ok()
    );
    session.shutdown().await;
}

#[tokio::test]
async fn sets_and_clears_favorites() {
    let python = python_or_skip!();
    let mut session = SidecarSession::start(&options(&python, 10), &Prompts::default())
        .await
        .unwrap();
    for value in [true, false] {
        let done = session
            .run(&Job::Favorite {
                id: "fake-00003".into(),
                master: None,
                value,
            })
            .await
            .unwrap();
        assert_eq!(done, Done::Favorited);
    }
    // Ein unbekanntes Foto ist endgültig verloren, kein zweiter Anlauf.
    match session
        .run(&Job::Favorite {
            id: "nope".into(),
            master: None,
            value: true,
        })
        .await
    {
        Err(SyncError::Sidecar { kind, .. }) => assert_eq!(kind, "missing"),
        other => panic!("erwartet missing, bekam {other:?}"),
    }
}
