//! Sync im Hintergrund-Thread, damit die UI nie blockiert.
//!
//! Der Sync bekommt eine eigene SQLite-Verbindung. Im WAL-Modus stört das den
//! lesenden UI-Thread nicht, und `Connection` muss nicht zwischen Threads
//! geteilt werden.

use std::path::PathBuf;

use icloud_photos_cache::Catalog;
use icloud_photos_core::Config;
use icloud_photos_sync::{
    Prompts, SecretRequest, SidecarOptions, SyncEvent, SyncSummary, run_sync,
};
use tokio::sync::mpsc;

/// Was der Sync-Thread der UI meldet.
pub enum SyncMessage {
    Progress {
        stored: u64,
    },
    Thumbnails {
        stored: u64,
    },
    /// Die Anmeldung ist vollstaendig durch — inklusive 2FA. Erst ab hier darf
    /// die UI die Anmeldemaske verlassen.
    Authenticated {
        trusted: bool,
    },
    /// Der Keyring hat kein Passwort und die Anmeldemaske hat keins geliefert.
    PasswordRequired {
        responder: SecretRequest,
    },
    TwoFactorRequired {
        responder: SecretRequest,
    },
    Finished(Result<SyncSummary, String>),
    /// Die Anmeldung wurde abgelehnt — die UI soll erneut fragen.
    CredentialsRejected {
        message: String,
    },
}

/// Startet einen Sync und liefert den Meldungskanal.
/// `pending_password` stammt aus der Anmeldemaske und beantwortet die erste
/// Passwortabfrage, ohne den Nutzer noch einmal zu behelligen.
pub fn spawn(
    config: &Config,
    pending_password: Option<String>,
) -> mpsc::UnboundedReceiver<SyncMessage> {
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
            // Bewusst kein Cursor: `photos.all` ist nicht nach `added_date`
            // sortiert, ein Cursor wuerde einen abgebrochenen Sync nicht
            // fortsetzen, sondern den Rest der Bibliothek ueberspringen. Der
            // volle Durchlauf ist idempotent — vorhandene Vorschauen werden
            // uebersprungen, Metadaten nur aktualisiert.
            options.since = None;
            options.thumbnail_dir = Some(thumbnail_dir);
            options.fake = fake_count();
            // Erlaubt, den kompletten Anmeldeweg ohne Apple-Konto durchzuspielen.
            options.fake_password = std::env::var_os("ICLOUD_PHOTOS_FAKE_PASSWORD").is_some();

            let (event_tx, mut event_rx) = mpsc::unbounded_channel::<SyncEvent>();
            let (tfa_tx, mut tfa_rx) = mpsc::unbounded_channel::<SecretRequest>();
            let (password_tx, mut password_rx) = mpsc::unbounded_channel::<SecretRequest>();

            let forwarder = {
                let tx = tx.clone();
                let mut pending = pending_password;
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
                                Some(SyncEvent::Authenticated { trusted }) => {
                                    let _ = tx.send(SyncMessage::Authenticated { trusted });
                                }
                                Some(_) => {}
                                None => break,
                            },
                            request = password_rx.recv() => match request {
                                Some(responder) => match pending.take() {
                                    // Aus der Anmeldemaske: direkt beantworten.
                                    Some(password) => {
                                        let _ = responder.send(password);
                                    }
                                    // Sonst muss die UI fragen.
                                    None => {
                                        let _ = tx.send(
                                            SyncMessage::PasswordRequired { responder },
                                        );
                                    }
                                },
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

            let prompts = Prompts {
                events: Some(event_tx),
                password: Some(password_tx),
                two_factor: Some(tfa_tx),
            };

            let mut result = run_sync(&mut catalog, &options, &prompts).await;

            // Der Delta-Sync sieht aeltere Assets nicht erneut. Fehlt dort eine
            // Vorschau — erster Lauf, Netzfehler, abgebrochener Sync — wird sie
            // in einem zweiten Durchgang gezielt nachgeholt.
            if let Ok(summary) = &mut result {
                match backfill_thumbnails(&mut catalog, &options, &prompts).await {
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

            // Abgelehnte Zugangsdaten sind kein technischer Fehler, sondern ein
            // Fall fuer die Anmeldemaske.
            if let Err(icloud_photos_sync::SyncError::Credentials { message }) = &result {
                let _ = tx.send(SyncMessage::CredentialsRejected {
                    message: message.clone(),
                });
                return;
            }
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
    prompts: &Prompts,
) -> Result<SyncSummary, icloud_photos_sync::SyncError> {
    let missing = catalog.ids_without_thumbnail(BACKFILL_LIMIT)?;
    if missing.is_empty() {
        return Ok(SyncSummary::default());
    }
    tracing::info!(count = missing.len(), "lade fehlende Vorschauen nach");

    let mut options = base.clone();
    options.fetch_thumbnails_for = missing;
    run_sync(catalog, &options, prompts).await
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
