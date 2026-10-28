//! Full-Res-Downloads auf Abruf, in einem eigenen Thread.
//!
//! Der Sidecar bleibt dafür stehen (Serve-Modus): Prozessstart plus Anmeldung
//! kosten Sekunden und dürfen nicht bei jedem geöffneten Foto anfallen.

use std::path::{Path, PathBuf};

use chrono::Utc;
use icloud_photos_cache::{Catalog, enforce_limit};
use icloud_photos_core::Config;
use icloud_photos_sync::{
    Done, Job, Original, Prompts, SecretRequest, SidecarOptions, SidecarSession, SyncError,
    TermsRequest,
};
use tokio::sync::mpsc;

/// Was der Download-Thread der UI meldet.
pub enum DownloadMessage {
    /// Das Original liegt jetzt im Cache.
    Ready {
        id: String,
        path: PathBuf,
    },
    Failed {
        id: String,
        message: String,
    },
    PasswordRequired {
        responder: SecretRequest,
    },
    TwoFactorRequired {
        responder: SecretRequest,
    },
    TermsRequired(TermsRequest),
    /// Die Eviction hat aufgeräumt; die Cache-Anzeige stimmt nicht mehr.
    CacheChanged,
    /// In iCloud gelöscht und aus dem Katalog genommen.
    Deleted {
        id: String,
    },
    DeleteFailed {
        id: String,
        message: String,
    },
    /// Eine Datei ist in iCloud angekommen; der nächste Sync bringt sie samt
    /// Vorschau in den Katalog.
    Uploaded {
        path: PathBuf,
    },
    UploadFailed {
        path: PathBuf,
        message: String,
    },
}

/// Wozu ein Original geholt wird.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Purpose {
    /// Jemand sieht es sich an — zählt als Zugriff.
    View,
    /// Vorausschauend für einen Nachbarn im Viewer geholt. Zählt nicht als
    /// Zugriff, sonst stünde ein nie betrachtetes Foto in der LRU-Reihe vor
    /// einem betrachteten.
    Prefetch,
}

enum Request {
    Original { id: String, purpose: Purpose },
    Delete { id: String },
    Upload { path: PathBuf },
}

/// Griff auf den Download-Thread.
pub struct Downloader {
    requests: mpsc::UnboundedSender<Request>,
}

impl Downloader {
    /// Fordert das Original eines Fotos an.
    pub fn request(&self, id: &str) {
        self.send(id, Purpose::View);
    }

    /// Holt ein Original im Voraus, ohne es als benutzt zu zählen.
    pub fn prefetch(&self, id: &str) {
        self.send(id, Purpose::Prefetch);
    }

    fn send(&self, id: &str, purpose: Purpose) {
        let _ = self.requests.send(Request::Original {
            id: id.to_string(),
            purpose,
        });
    }

    /// Ein zweiter Griff auf denselben Thread (die Warteschlange ist geteilt).
    pub fn clone_handle(&self) -> Self {
        Self {
            requests: self.requests.clone(),
        }
    }

    /// Löscht ein Foto in iCloud (dort 30 Tage unter "Zuletzt gelöscht").
    pub fn delete(&self, id: &str) {
        let _ = self.requests.send(Request::Delete { id: id.to_string() });
    }

    /// Lädt eine Datei in die Mediathek.
    pub fn upload(&self, path: &Path) {
        let _ = self.requests.send(Request::Upload {
            path: path.to_path_buf(),
        });
    }
}

/// Startet den Download-Thread. Der Sidecar wird erst beim ersten Auftrag
/// gestartet — wer nie ein Foto öffnet, zahlt auch keine Anmeldung.
pub fn spawn(
    config: &Config,
    fake: Option<u64>,
) -> (Downloader, mpsc::UnboundedReceiver<DownloadMessage>) {
    let (request_tx, mut request_rx) = mpsc::unbounded_channel::<Request>();
    let (message_tx, message_rx) = mpsc::unbounded_channel::<DownloadMessage>();

    let db_path = config.paths.db_path.clone();
    let full_res_dir = config.paths.full_res_dir.clone();
    let cache_config = config.cache.clone();
    let cookie_dir = Some(config.paths.cookie_dir.clone());
    let username = config.icloud.username.clone();

    std::thread::spawn(move || {
        let runtime = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(runtime) => runtime,
            Err(error) => {
                tracing::error!(%error, "Download-Thread konnte nicht starten");
                return;
            }
        };

        runtime.block_on(async move {
            let catalog = match Catalog::open(&db_path) {
                Ok(catalog) => catalog,
                Err(error) => {
                    tracing::error!(%error, "Katalog fuer Downloads nicht zu oeffnen");
                    return;
                }
            };

            let mut options = SidecarOptions::new(
                crate::sync_task::python_path(),
                crate::sync_task::script_path(),
                username,
            );
            options.cookie_dir = cookie_dir;
            options.fake = fake;

            let (password_tx, mut password_rx) = mpsc::unbounded_channel::<SecretRequest>();
            let (tfa_tx, mut tfa_rx) = mpsc::unbounded_channel::<SecretRequest>();
            let (terms_tx, mut terms_rx) = mpsc::unbounded_channel::<TermsRequest>();
            let prompts = Prompts {
                events: None,
                password: Some(password_tx),
                two_factor: Some(tfa_tx),
                terms: Some(terms_tx),
            };

            // Abfragen an die UI weiterreichen, solange der Thread lebt.
            let forwarder = {
                let message_tx = message_tx.clone();
                tokio::spawn(async move {
                    loop {
                        tokio::select! {
                            request = password_rx.recv() => match request {
                                Some(responder) => {
                                    let _ = message_tx
                                        .send(DownloadMessage::PasswordRequired { responder });
                                }
                                None => break,
                            },
                            request = tfa_rx.recv() => match request {
                                Some(responder) => {
                                    let _ = message_tx
                                        .send(DownloadMessage::TwoFactorRequired { responder });
                                }
                                None => break,
                            },
                            request = terms_rx.recv() => match request {
                                Some(request) => {
                                    let _ = message_tx.send(DownloadMessage::TermsRequired(request));
                                }
                                None => break,
                            },
                        }
                    }
                })
            };

            let mut session: Option<SidecarSession> = None;

            while let Some(request) = request_rx.recv().await {
                let (id, purpose) = match request {
                    Request::Original { id, purpose } => (id, purpose),
                    Request::Delete { id } => {
                        let master = catalog
                            .get(&id)
                            .ok()
                            .flatten()
                            .and_then(|photo| photo.record.master_id);
                        let job = Job::Delete {
                            id: id.clone(),
                            master,
                        };
                        let message = match run_job(&mut session, &options, &prompts, &job).await {
                            Ok(_) => {
                                // Gleich aus dem Katalog: der nächste Sync sähe
                                // die Löschung zwar auch, bis dahin stünde das
                                // Foto aber noch im Raster.
                                match catalog.delete_photo(&id) {
                                    Ok(files) => {
                                        for file in files {
                                            let _ = std::fs::remove_file(file);
                                        }
                                    }
                                    Err(error) => tracing::error!(%id, %error, "Geloeschtes Foto bleibt im Katalog"),
                                }
                                DownloadMessage::Deleted { id }
                            }
                            Err(error) => {
                                tracing::warn!(%id, %error, "Foto nicht loeschbar");
                                DownloadMessage::DeleteFailed {
                                    id,
                                    message: error.to_string(),
                                }
                            }
                        };
                        let _ = message_tx.send(message);
                        continue;
                    }
                    Request::Upload { path } => {
                        let job = Job::Upload { path: path.clone() };
                        let message = match run_job(&mut session, &options, &prompts, &job).await {
                            Ok(_) => DownloadMessage::Uploaded { path },
                            Err(error) => {
                                tracing::warn!(path = %path.display(), %error, "Upload gescheitert");
                                DownloadMessage::UploadFailed {
                                    path,
                                    message: error.to_string(),
                                }
                            }
                        };
                        let _ = message_tx.send(message);
                        continue;
                    }
                };
                let dest = destination(&catalog, &full_res_dir, &id);
                // Der Master-Record aus dem Katalog spart einen CloudKit-Lookup.
                let master = catalog
                    .get(&id)
                    .ok()
                    .flatten()
                    .and_then(|photo| photo.record.master_id);

                match fetch(
                    &mut session,
                    &options,
                    &prompts,
                    &id,
                    master.as_deref(),
                    &dest,
                )
                .await
                {
                    Ok(original) => {
                        let stored = match purpose {
                            Purpose::View => catalog.set_original_cached(
                                &id,
                                &original.path,
                                original.bytes,
                                Utc::now(),
                            ),
                            Purpose::Prefetch => {
                                catalog.set_original_prefetched(&id, &original.path, original.bytes)
                            }
                        };
                        if let Err(error) = stored {
                            tracing::error!(%error, "Original nicht im Katalog vermerkt");
                        }
                        let _ = message_tx.send(DownloadMessage::Ready {
                            id: id.clone(),
                            path: original.path,
                        });

                        // Nach jedem Download aufraeumen, wie in CLAUDE.md
                        // festgelegt.
                        match enforce_limit(&catalog, &cache_config) {
                            Ok(report) if !report.is_noop() => {
                                tracing::info!(
                                    evicted = report.evicted,
                                    freed_bytes = report.freed_bytes,
                                    "Full-Res-Cache heruntergeraeumt"
                                );
                                let _ = message_tx.send(DownloadMessage::CacheChanged);
                            }
                            Ok(_) => {
                                let _ = message_tx.send(DownloadMessage::CacheChanged);
                            }
                            Err(error) => tracing::error!(%error, "Eviction fehlgeschlagen"),
                        }
                    }
                    Err(error) => {
                        tracing::warn!(%id, %error, "Original nicht ladbar");
                        let _ = message_tx.send(DownloadMessage::Failed {
                            id,
                            message: error.to_string(),
                        });
                    }
                }
            }

            forwarder.abort();
            if let Some(active) = session {
                active.shutdown().await;
            }
        });
    });

    (
        Downloader {
            requests: request_tx,
        },
        message_rx,
    )
}

/// So oft wird ein Original versucht, bevor der Auftrag als gescheitert gilt.
/// Zwei reichen: der zweite Anlauf läuft über eine frische Anmeldung.
const ATTEMPTS: u32 = 2;

/// So lange wird vor dem zweiten Anlauf gewartet — gegen den kurzen
/// Netzaussetzer, der sonst aus einem Foto eine Fehlermeldung macht.
const RETRY_PAUSE: std::time::Duration = std::time::Duration::from_secs(1);

/// Holt ein Original und setzt bei einem Fehlschlag einmal neu auf.
///
/// Die Sitzung im Sidecar läuft nach einigen Stunden ab, und Apple meldet das
/// als gewöhnlichen Fehlschlag, nicht als Verbindungsabbruch. Ohne diesen
/// zweiten Anlauf mit frischer Anmeldung scheitert danach *jedes* geöffnete
/// Foto mit derselben Meldung, bis die App neu gestartet wird. Ein
/// Netzaussetzer sieht von hier aus genauso aus und ist damit gleich mit
/// erledigt.
///
/// Der Preis: ein vorgeblich behebbarer Fehlschlag kostet eine neue Anmeldung,
/// auch wenn die alte Sitzung in Ordnung war. Das ist einmal ein paar Sekunden
/// und allemal besser als eine App, die bis zum Neustart keine Originale mehr
/// liefert.
async fn fetch(
    session: &mut Option<SidecarSession>,
    options: &SidecarOptions,
    prompts: &Prompts,
    id: &str,
    master: Option<&str>,
    dest: &Path,
) -> Result<Original, SyncError> {
    let job = Job::Original {
        id: id.to_string(),
        master: master.map(str::to_string),
        dest: dest.to_path_buf(),
    };
    match run_job(session, options, prompts, &job).await? {
        Done::Original(original) => Ok(original),
        _ => Err(SyncError::UnexpectedExit("falsche Antwort".into())),
    }
}

/// Führt einen Auftrag aus und setzt bei einem Fehlschlag einmal mit frischer
/// Anmeldung neu auf (siehe `fetch`). Gilt auch fürs Löschen und Hochladen:
/// beide sind wiederholbar — ein zweites Löschen desselben Fotos ändert
/// nichts, und eine Datei, die iCloud schon hat, kommt als Duplikat zurück.
async fn run_job(
    session: &mut Option<SidecarSession>,
    options: &SidecarOptions,
    prompts: &Prompts,
    job: &Job,
) -> Result<Done, SyncError> {
    let mut last: Option<SyncError> = None;

    for attempt in 1..=ATTEMPTS {
        if attempt > 1 {
            tokio::time::sleep(RETRY_PAUSE).await;
        }
        if session.is_none() {
            match SidecarSession::start(options, prompts).await {
                Ok(started) => *session = Some(started),
                Err(error) => {
                    if is_final(&error) {
                        return Err(error);
                    }
                    last = Some(error);
                    continue;
                }
            }
        }

        let Some(active) = session.as_mut() else {
            continue;
        };
        match active.run(job).await {
            Ok(done) => return Ok(done),
            Err(error) => {
                if is_final(&error) {
                    return Err(error);
                }
                tracing::warn!(
                    ?job,
                    %error,
                    attempt,
                    "Auftrag gescheitert — Sitzung wird erneuert"
                );
                // Die Sitzung kann abgelaufen oder tot sein; sie wegzuwerfen
                // ist der einzige Weg, das von hier aus herauszufinden.
                // `kill_on_drop` beendet den Sidecar dabei mit.
                *session = None;
                last = Some(error);
            }
        }
    }

    Err(last.unwrap_or_else(|| SyncError::UnexpectedExit("kein Versuch".into())))
}

/// Ob ein zweiter Anlauf sinnlos wäre.
fn is_final(error: &SyncError) -> bool {
    match error {
        // Der Sidecar sagt selbst, ob das Asset schlicht nicht da ist.
        SyncError::Sidecar { kind, .. } => kind == "missing",
        // Ein abgelehntes Passwort wird beim zweiten Mal nicht richtig.
        SyncError::Credentials { .. } | SyncError::PasswordAborted => true,
        SyncError::TwoFactorAborted | SyncError::TermsDeclined => true,
        _ => false,
    }
}

/// Zielpfad für ein Original. Die Endung stammt aus dem Dateinamen in iCloud,
/// damit GDK das Bildformat erkennt.
fn destination(catalog: &Catalog, dir: &std::path::Path, id: &str) -> PathBuf {
    let extension = catalog
        .get(id)
        .ok()
        .flatten()
        .and_then(|photo| {
            std::path::Path::new(&photo.record.filename)
                .extension()
                .map(|ext| ext.to_string_lossy().to_lowercase())
        })
        .unwrap_or_else(|| "bin".to_string());

    // IDs koennen "/" enthalten und taugen nicht direkt als Dateiname.
    let safe: String = id
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    dir.join(format!("{safe}.{extension}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use icloud_photos_core::model::PhotoRecord;

    fn config(dir: &std::path::Path) -> Config {
        let mut config = Config::default();
        config.icloud.username = "tester@example.com".into();
        config.paths.db_path = dir.join("catalog.sqlite");
        config.paths.full_res_dir = dir.join("fullres");
        config.paths.thumbnail_dir = dir.join("thumbs");
        config
    }

    fn seed(catalog: &Catalog, id: &str) {
        catalog
            .upsert_record(&PhotoRecord {
                id: id.to_string(),
                master_id: None,
                filename: format!("{id}.HEIC"),
                added_date: Utc::now(),
                asset_date: None,
                checksum: None,
                width: None,
                height: None,
                lat: None,
                lon: None,
                item_type: None,
            })
            .unwrap();
    }

    #[test]
    fn destination_uses_the_icloud_extension() {
        let catalog = Catalog::open_in_memory().unwrap();
        seed(&catalog, "abc");
        let path = destination(&catalog, std::path::Path::new("/cache"), "abc");
        assert_eq!(path, PathBuf::from("/cache/abc.heic"));
    }

    #[test]
    fn destination_sanitises_ids_and_falls_back() {
        let catalog = Catalog::open_in_memory().unwrap();
        // Unbekannte ID: keine Endung bekannt, und "/" darf nicht in den Pfad.
        let path = destination(&catalog, std::path::Path::new("/cache"), "a/b+c");
        assert_eq!(path, PathBuf::from("/cache/a_b_c.bin"));
    }

    #[test]
    fn a_dead_session_is_replaced_instead_of_reused() {
        // Der Fake-Sidecar laesst `fake-flaky` im ersten Prozess scheitern und
        // liefert erst im zweiten. Ohne den zweiten Anlauf mit frischer
        // Anmeldung bliebe es bei der Fehlermeldung — so verhaelt sich eine
        // abgelaufene iCloud-Sitzung im Echtbetrieb.
        let dir = tempfile::tempdir().unwrap();
        let config = config(dir.path());

        let catalog = Catalog::open(&config.paths.db_path).unwrap();
        seed(&catalog, "fake-flaky");
        drop(catalog);

        let (downloader, mut messages) = spawn(&config, Some(5));
        downloader.request("fake-flaky");

        loop {
            match messages.blocking_recv() {
                Some(DownloadMessage::Ready { id, path }) => {
                    assert_eq!(id, "fake-flaky");
                    assert!(path.is_file(), "{} fehlt", path.display());
                    break;
                }
                Some(DownloadMessage::Failed { id, message }) => {
                    panic!("{id} haette beim zweiten Anlauf ankommen muessen: {message}");
                }
                Some(_) => continue,
                None => panic!("Download-Thread beendet"),
            }
        }
    }

    #[test]
    fn download_stores_the_original_and_enforces_the_limit() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = config(dir.path());
        // Ein einziges Original passt, das zweite loest die Eviction aus.
        config.cache.full_res_cache_limit_gb = 1.5e-6;

        let catalog = Catalog::open(&config.paths.db_path).unwrap();
        for index in 0..3 {
            seed(&catalog, &format!("fake-{index:05}"));
        }
        drop(catalog);

        // Fake-Modus explizit, nicht ueber die Umgebung: sonst liefe der Test
        // in einem normalen `cargo test` still ins Leere.
        let (downloader, mut messages) = spawn(&config, Some(5));
        let mut ready = Vec::new();
        for index in 0..3 {
            downloader.request(&format!("fake-{index:05}"));
            // Auf die Ready-Meldung dieses Auftrags warten.
            loop {
                match messages.blocking_recv() {
                    Some(DownloadMessage::Ready { id, path }) => {
                        assert!(path.is_file(), "{} fehlt", path.display());
                        ready.push(id);
                        break;
                    }
                    Some(DownloadMessage::Failed { id, message }) => {
                        panic!("{id} fehlgeschlagen: {message}");
                    }
                    Some(_) => continue,
                    None => panic!("Download-Thread beendet"),
                }
            }
        }
        assert_eq!(ready.len(), 3);

        let catalog = Catalog::open(&config.paths.db_path).unwrap();
        assert!(
            catalog.cached_bytes().unwrap() <= config.cache.limit_bytes(),
            "die Eviction haette nach dem Download laufen muessen"
        );
        // Das zuerst geholte Original ist als erstes gewichen.
        assert!(!catalog.get("fake-00000").unwrap().unwrap().is_orig_cached);
        assert!(catalog.get("fake-00002").unwrap().unwrap().is_orig_cached);
    }
}
