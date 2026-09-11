//! Sync im Hintergrund-Thread, damit die UI nie blockiert.
//!
//! Der Sync bekommt eine eigene SQLite-Verbindung. Im WAL-Modus stört das den
//! lesenden UI-Thread nicht, und `Connection` muss nicht zwischen Threads
//! geteilt werden.

use std::path::PathBuf;

use icloud_photos_cache::Catalog;
use icloud_photos_core::Config;
use icloud_photos_sync::{SidecarOptions, SyncEvent, SyncSummary, run_sync};
use tokio::sync::{mpsc, oneshot};

/// Was der Sync-Thread der UI meldet.
pub enum SyncMessage {
    Progress { stored: u64 },
    Thumbnails { stored: u64 },
    TwoFactorRequired { responder: oneshot::Sender<String> },
    Finished(Result<SyncSummary, String>),
}

/// Startet einen Sync und liefert den Meldungskanal.
pub fn spawn(config: &Config) -> mpsc::UnboundedReceiver<SyncMessage> {
    let (tx, rx) = mpsc::unbounded_channel();

    let db_path = config.paths.db_path.clone();
    let cookie_dir = config.paths.db_path.parent().map(|p| p.join("cookies"));
    let username = config.icloud.username.clone();
    let thumbnail_dir = config.paths.thumbnail_dir.clone();

    std::thread::spawn(move || {
        let runtime = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(runtime) => runtime,
            Err(error) => {
                let _ = tx.send(SyncMessage::Finished(Err(format!(
                    "Tokio-Runtime konnte nicht starten: {error}"
                ))));
                return;
            }
        };

        runtime.block_on(async move {
            let mut catalog = match Catalog::open(&db_path) {
                Ok(catalog) => catalog,
                Err(error) => {
                    let _ = tx.send(SyncMessage::Finished(Err(format!(
                        "Katalog konnte nicht geöffnet werden: {error}"
                    ))));
                    return;
                }
            };

            let mut options = SidecarOptions::new(python_path(), script_path(), username);
            options.cookie_dir = cookie_dir;
            options.since = catalog.sync_cursor().ok().flatten();
            options.thumbnail_dir = Some(thumbnail_dir);
            options.fake = fake_count();

            let (event_tx, mut event_rx) = mpsc::unbounded_channel::<SyncEvent>();
            let (tfa_tx, mut tfa_rx) = mpsc::unbounded_channel::<oneshot::Sender<String>>();

            let forwarder = {
                let tx = tx.clone();
                tokio::spawn(async move {
                    loop {
                        tokio::select! {
                            event = event_rx.recv() => match event {
                                Some(SyncEvent::Progress { stored }) => {
                                    let _ = tx.send(SyncMessage::Progress { stored });
                                }
                                Some(SyncEvent::Thumbnails { stored }) => {
                                    let _ = tx.send(SyncMessage::Thumbnails { stored });
                                }
                                Some(_) => {}
                                None => break,
                            },
                            request = tfa_rx.recv() => match request {
                                Some(responder) => {
                                    let _ = tx.send(SyncMessage::TwoFactorRequired { responder });
                                }
                                None => break,
                            },
                        }
                    }
                })
            };

            let mut result = run_sync(
                &mut catalog,
                &options,
                Some(event_tx.clone()),
                Some(tfa_tx.clone()),
            )
            .await;

            // Der Delta-Sync sieht aeltere Assets nicht erneut. Fehlt dort eine
            // Vorschau — erster Lauf, Netzfehler, abgebrochener Sync — wird sie
            // in einem zweiten Durchgang gezielt nachgeholt.
            if let Ok(summary) = &mut result {
                match backfill_thumbnails(&mut catalog, &options, event_tx, tfa_tx).await {
                    Ok(extra) => {
                        summary.thumbnails += extra.thumbnails;
                        summary.thumbnails_failed += extra.thumbnails_failed;
                    }
                    // Die Metadaten stehen schon im Katalog; fehlende Vorschauen
                    // holt der naechste Lauf. Das ist kein gescheiterter Sync.
                    Err(error) => {
                        tracing::warn!(%error, "Vorschauen konnten nicht nachgeladen werden");
                    }
                }
            }

            forwarder.abort();

            let _ = tx.send(SyncMessage::Finished(result.map_err(|e| e.to_string())));
        });
    });

    rx
}

/// Hoechstens so viele Vorschauen pro Lauf nachladen, damit ein erster Sync
/// einer grossen Bibliothek nicht endlos blockiert.
const BACKFILL_LIMIT: u64 = 500;

/// Holt fehlende Vorschauen nach. Gibt `Ok(default)` zurueck, wenn nichts fehlt.
async fn backfill_thumbnails(
    catalog: &mut Catalog,
    base: &SidecarOptions,
    events: mpsc::UnboundedSender<SyncEvent>,
    two_factor: mpsc::UnboundedSender<oneshot::Sender<String>>,
) -> Result<SyncSummary, icloud_photos_sync::SyncError> {
    let missing = catalog.ids_without_thumbnail(BACKFILL_LIMIT)?;
    if missing.is_empty() {
        return Ok(SyncSummary::default());
    }
    tracing::info!(count = missing.len(), "lade fehlende Vorschauen nach");

    let mut options = base.clone();
    options.fetch_thumbnails_for = missing;
    run_sync(catalog, &options, Some(events), Some(two_factor)).await
}

/// Testmodus: `ICLOUD_PHOTOS_FAKE=500` synchronisiert synthetische Daten.
pub fn fake_count() -> Option<u64> {
    std::env::var("ICLOUD_PHOTOS_FAKE").ok()?.parse().ok()
}

/// Python-Interpreter: `ICLOUD_PHOTOS_PYTHON`, sonst das venv im Projekt,
/// sonst das System-Python.
fn python_path() -> PathBuf {
    if let Ok(path) = std::env::var("ICLOUD_PHOTOS_PYTHON") {
        return PathBuf::from(path);
    }
    let venv = project_root().join(".venv/bin/python");
    if venv.is_file() {
        return venv;
    }
    PathBuf::from("python3")
}

/// Sidecar-Skript: `ICLOUD_PHOTOS_SIDECAR`, sonst relativ zum Projekt.
fn script_path() -> PathBuf {
    if let Ok(path) = std::env::var("ICLOUD_PHOTOS_SIDECAR") {
        return PathBuf::from(path);
    }
    project_root().join("sidecar/pyicloud_sync.py")
}

fn project_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}
