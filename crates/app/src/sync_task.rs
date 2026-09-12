//! Sync im Hintergrund-Thread, damit die UI nie blockiert.
//!
//! Der Sync bekommt eine eigene SQLite-Verbindung. Im WAL-Modus stört das den
//! lesenden UI-Thread nicht, und `Connection` muss nicht zwischen Threads
//! geteilt werden.

use std::path::PathBuf;

use chrono::Utc;
use icloud_photos_cache::Catalog;
use icloud_photos_core::Config;
use icloud_photos_sync::{
    Prompts, RECONCILE_AFTER, SecretRequest, SidecarOptions, SyncEvent, SyncSummary, run_sync,
};
use tokio::sync::mpsc;

/// Was der Sync-Thread der UI meldet.
pub enum SyncMessage {
    /// Der Lauf beginnt. `resuming` steht, wenn ein abgebrochener Erstaufbau
    /// fortgesetzt wird — das gehoert in die Anzeige, sonst sieht es aus, als
    /// finge der Sync wieder bei null an. `full_pass` sagt, ob die ganze
    /// Bibliothek durchlaufen wird; nur dann ist ein Balkenstand ueberhaupt
    /// sinnvoll, sonst zaehlt der Lauf Aenderungen gegen nichts.
    Started {
        resuming: Option<u64>,
        full_pass: bool,
    },
    /// Nach dem Abgleich werden fehlende Vorschauen nachgeholt. Eigene Phase,
    /// weil ihre Zaehlung nichts mit dem Bibliotheksbestand zu tun hat.
    BackfillStarted {
        missing: u64,
    },
    Progress {
        stored: u64,
    },
    Thumbnails {
        stored: u64,
    },
    /// Die Bibliothek ist unveraendert — dieser Lauf kostete einen Request.
    Unchanged,
    LibraryCount {
        total: u64,
    },
    Deleted {
        removed: u64,
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
    let cookie_dir = Some(config.paths.cookie_dir.clone());
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
            // Kein `added_date`-Cursor: `photos.all` ist danach nicht sortiert.
            // Der Abgleich laeuft stattdessen ueber den CloudKit-Zone-Token.
            options.since = None;

            let state = match catalog.sync_state() {
                Ok(state) => state,
                Err(error) => {
                    let _ = tx.send(SyncMessage::Finished(Err(format!(
                        "Sync-Zustand nicht lesbar: {error}"
                    ))));
                    return;
                }
            };
            apply_sync_state(&mut options, &state);
            let _ = tx.send(SyncMessage::Started {
                resuming: (!state.baseline_done && state.baseline_offset > 0)
                    .then_some(state.baseline_offset),
                // Ohne Token geht der Lauf durch die ganze Bibliothek.
                full_pass: options.since_token.is_none(),
            });
            options.thumbnail_dir = Some(thumbnail_dir);
            options.fake = fake_count();
            // Erlaubt, den kompletten Anmeldeweg ohne Apple-Konto durchzuspielen.
            options.fake_password = std::env::var_os("ICLOUD_PHOTOS_FAKE_PASSWORD").is_some();
            options.fake_delay_ms = std::env::var("ICLOUD_PHOTOS_FAKE_DELAY_MS")
                .ok()
                .and_then(|value| value.parse().ok())
                .unwrap_or(0);

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
                                Some(SyncEvent::Unchanged) => {
                                    let _ = tx.send(SyncMessage::Unchanged);
                                }
                                Some(SyncEvent::LibraryCount { total }) => {
                                    let _ = tx.send(SyncMessage::LibraryCount { total });
                                }
                                Some(SyncEvent::Deleted { removed }) => {
                                    let _ = tx.send(SyncMessage::Deleted { removed });
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
                match backfill_thumbnails(&mut catalog, &options, &prompts, &tx).await {
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

/// Uebersetzt den gespeicherten Zustand in Sidecar-Schalter.
///
/// Der Token geht nur mit, wenn der Erstaufbau wirklich durch ist — sonst
/// zoege der Sidecar ein Delta und der Rest der Bibliothek bliebe ungesehen.
fn apply_sync_state(options: &mut SidecarOptions, state: &icloud_photos_cache::SyncState) {
    if !state.baseline_done {
        options.since_token = None;
        options.offset = state.baseline_offset;
        if options.offset > 0 {
            tracing::info!(offset = options.offset, "setze Erstaufbau fort");
        }
        return;
    }

    options.offset = 0;
    let stale = state
        .last_baseline_at
        .is_none_or(|at| Utc::now() - at > RECONCILE_AFTER);
    if stale {
        tracing::info!("periodischer Vollabgleich faellig");
        options.force_baseline = true;
        options.since_token = None;
    } else {
        options.since_token = state.zone_token.clone();
    }
}

/// Holt fehlende Vorschauen nach. Gibt `Ok(default)` zurueck, wenn nichts fehlt.
async fn backfill_thumbnails(
    catalog: &mut Catalog,
    base: &SidecarOptions,
    prompts: &Prompts,
    tx: &mpsc::UnboundedSender<SyncMessage>,
) -> Result<SyncSummary, icloud_photos_sync::SyncError> {
    let missing = catalog.ids_without_thumbnail(BACKFILL_LIMIT)?;
    if missing.is_empty() {
        return Ok(SyncSummary::default());
    }
    tracing::info!(count = missing.len(), "lade fehlende Vorschauen nach");
    // Erst melden, wenn wirklich etwas nachzuholen ist — sonst stuende die
    // Phase kurz in der Anzeige, ohne dass etwas passiert.
    let _ = tx.send(SyncMessage::BackfillStarted {
        missing: missing.len() as u64,
    });

    let mut options = base.clone();
    options.fetch_thumbnails_for = missing;
    // Der Nachlade-Lauf ist kein Abgleich: er darf weder den Token uebernehmen
    // noch einen Erstaufbau als abgeschlossen melden.
    options.since_token = None;
    options.force_baseline = false;
    options.offset = 0;
    run_sync(catalog, &options, prompts).await
}

/// Testmodus: `ICLOUD_PHOTOS_FAKE=500` synchronisiert synthetische Daten.
pub fn fake_count() -> Option<u64> {
    std::env::var("ICLOUD_PHOTOS_FAKE").ok()?.parse().ok()
}

/// Python-Interpreter: `ICLOUD_PHOTOS_PYTHON`, sonst das venv im Projekt,
/// sonst das System-Python.
pub fn python_path() -> PathBuf {
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
pub fn script_path() -> PathBuf {
    if let Ok(path) = std::env::var("ICLOUD_PHOTOS_SIDECAR") {
        return PathBuf::from(path);
    }
    project_root().join("sidecar/pyicloud_sync.py")
}

fn project_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}
