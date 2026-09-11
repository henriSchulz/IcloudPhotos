//! Start und Steuerung des Python-Sidecars.

use std::path::PathBuf;
use std::process::Stdio;

use chrono::{DateTime, Utc};
use icloud_photos_cache::Catalog;
use icloud_photos_core::model::PhotoRecord;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::{mpsc, oneshot};

use crate::SyncError;
use crate::protocol::{SidecarMessage, SyncEvent};

/// So viele Datensätze werden gesammelt, bevor eine Transaktion geschrieben wird.
const BATCH_SIZE: usize = 256;

/// Wie der Sidecar gestartet wird.
#[derive(Debug, Clone)]
pub struct SidecarOptions {
    /// Python-Interpreter, üblicherweise der aus dem venv des Projekts.
    pub python: PathBuf,
    pub script: PathBuf,
    pub username: String,
    pub cookie_dir: Option<PathBuf>,
    /// Nur Assets neuer als dieser Zeitpunkt abfragen (Delta-Sync).
    pub since: Option<DateTime<Utc>>,
    /// Wohin Vorschaubilder geladen werden. `None` heisst: nur Metadaten.
    pub thumbnail_dir: Option<PathBuf>,
    /// Statt eines Syncs nur Vorschauen fuer diese IDs nachladen.
    pub fetch_thumbnails_for: Vec<String>,
    /// Testmodus: so viele synthetische Datensätze statt echter iCloud-Zugriffe.
    pub fake: Option<u64>,
    /// Testmodus: den 2FA-Dialog durchspielen.
    pub fake_two_factor: bool,
}

impl SidecarOptions {
    pub fn new(
        python: impl Into<PathBuf>,
        script: impl Into<PathBuf>,
        username: impl Into<String>,
    ) -> Self {
        Self {
            python: python.into(),
            script: script.into(),
            username: username.into(),
            cookie_dir: None,
            since: None,
            thumbnail_dir: None,
            fetch_thumbnails_for: Vec::new(),
            fake: None,
            fake_two_factor: false,
        }
    }

    fn argv(&self) -> Vec<String> {
        let mut args = vec![self.script.to_string_lossy().into_owned()];
        if let Some(dir) = &self.thumbnail_dir {
            args.push("--thumbs".into());
            args.push(dir.to_string_lossy().into_owned());
        }
        if let Some(count) = self.fake {
            args.push("--fake".into());
            args.push(count.to_string());
            if self.fake_two_factor {
                args.push("--fake-2fa".into());
            }
            return args;
        }
        args.push("--username".into());
        args.push(self.username.clone());
        if let Some(dir) = &self.cookie_dir {
            args.push("--cookie-dir".into());
            args.push(dir.to_string_lossy().into_owned());
        }
        if self.is_thumbnail_backfill() {
            args.push("--fetch-thumbs".into());
            return args;
        }
        if let Some(since) = self.since {
            args.push("--since".into());
            args.push(since.to_rfc3339());
        }
        args
    }

    /// Im Nachlade-Modus kommen IDs ueber stdin statt eines vollen Syncs.
    fn is_thumbnail_backfill(&self) -> bool {
        !self.fetch_thumbnails_for.is_empty()
    }
}

/// Ergebnis eines Sync-Laufs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SyncSummary {
    /// Wie viele Datensätze in den Katalog geschrieben wurden.
    pub stored: u64,
    /// Wie viele der Sidecar gemeldet hat.
    pub reported: u64,
    /// Wie viele Vorschauen im Katalog vermerkt wurden.
    pub thumbnails: u64,
    /// Wie viele Vorschauen der Sidecar nicht laden konnte.
    pub thumbnails_failed: u64,
    /// Neuer Cursor für den nächsten Delta-Sync.
    pub cursor: Option<DateTime<Utc>>,
}

/// Kanal, über den die UI nach einem 2FA-Code gefragt wird.
pub type TwoFactorRequest = oneshot::Sender<String>;

/// Führt einen Sync aus: startet den Sidecar, schreibt alle Metadaten in den
/// Katalog und gibt Fortschritt über `events` aus.
///
/// Meldet der Sidecar `auth_required`, geht eine [`TwoFactorRequest`] an
/// `two_factor`; der Sync wartet dann auf den Code aus der UI.
pub async fn run_sync(
    catalog: &mut Catalog,
    options: &SidecarOptions,
    events: Option<mpsc::UnboundedSender<SyncEvent>>,
    two_factor: Option<mpsc::UnboundedSender<TwoFactorRequest>>,
) -> Result<SyncSummary, SyncError> {
    let args = options.argv();
    let command_line = format!("{} {}", options.python.display(), args.join(" "));
    tracing::info!(command = %command_line, "starte Sidecar");

    let mut child = Command::new(&options.python)
        .args(&args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .map_err(|source| SyncError::Spawn {
            command: command_line,
            source,
        })?;

    let stdout = child
        .stdout
        .take()
        .expect("stdout wurde als Pipe angefordert");
    let mut stdin = child
        .stdin
        .take()
        .expect("stdin wurde als Pipe angefordert");
    let mut lines = BufReader::new(stdout).lines();

    let mut summary = SyncSummary::default();
    let mut batch: Vec<PhotoRecord> = Vec::with_capacity(BATCH_SIZE);
    let mut saw_done = false;
    let mut failure: Option<SyncError> = None;

    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let message: SidecarMessage =
            serde_json::from_str(&line).map_err(|source| SyncError::Protocol {
                line: line.clone(),
                source,
            })?;

        match message {
            SidecarMessage::Photo(record) => {
                batch.push(record);
                if batch.len() >= BATCH_SIZE {
                    summary.stored += flush(catalog, &mut batch)?;
                    emit(
                        &events,
                        SyncEvent::Progress {
                            stored: summary.stored,
                        },
                    );
                }
            }
            SidecarMessage::Thumb { id, path, bytes } => {
                // Der Katalogeintrag muss existieren, sonst laeuft das Update
                // ins Leere — deshalb erst den Batch schreiben.
                summary.stored += flush(catalog, &mut batch)?;
                catalog.set_thumbnail(&id, &path, bytes)?;
                summary.thumbnails += 1;
                emit(
                    &events,
                    SyncEvent::Thumbnails {
                        stored: summary.thumbnails,
                    },
                );
            }
            SidecarMessage::ThumbFailed { id, message } => {
                tracing::warn!(%id, %message, "Vorschau konnte nicht geladen werden");
                summary.thumbnails_failed += 1;
            }
            SidecarMessage::AuthRequired { method } => {
                tracing::info!(%method, "Sidecar fordert 2FA-Code an");
                emit(&events, SyncEvent::TwoFactorRequired);

                let Some(channel) = &two_factor else {
                    failure = Some(SyncError::TwoFactorAborted);
                    break;
                };
                let (tx, rx) = oneshot::channel();
                if channel.send(tx).is_err() {
                    failure = Some(SyncError::TwoFactorAborted);
                    break;
                }
                let Ok(code) = rx.await else {
                    failure = Some(SyncError::TwoFactorAborted);
                    break;
                };
                stdin.write_all(format!("{code}\n").as_bytes()).await?;
                stdin.flush().await?;
            }
            SidecarMessage::AuthOk { trusted } => {
                if !trusted {
                    tracing::warn!(
                        "Sitzung ist nicht vertrauenswürdig — der nächste Sync fragt erneut nach 2FA"
                    );
                }
                emit(&events, SyncEvent::Authenticated { trusted });

                // Erst jetzt die IDs schicken. Vorher waere die erste Zeile als
                // 2FA-Code verschluckt worden, denn der Sidecar liest beides
                // von stdin.
                if options.is_thumbnail_backfill() {
                    let payload = options.fetch_thumbnails_for.join("\n");
                    stdin.write_all(payload.as_bytes()).await?;
                    stdin.write_all(b"\n").await?;
                    stdin.flush().await?;
                    // EOF beendet die Leseschleife des Sidecars.
                    stdin.shutdown().await?;
                }
            }
            SidecarMessage::Done { count, cursor } => {
                summary.reported = count;
                summary.cursor = cursor;
                saw_done = true;
            }
            SidecarMessage::Error { kind, message } => {
                failure = Some(SyncError::Sidecar { kind, message });
                break;
            }
        }
    }

    // Restliche Datensätze auch bei Abbruch sichern — ein Teil-Sync ist besser
    // als ein verworfener, der Cursor kommt dann eben erst beim nächsten Lauf.
    summary.stored += flush(catalog, &mut batch)?;
    emit(
        &events,
        SyncEvent::Progress {
            stored: summary.stored,
        },
    );

    // stdin schliessen, damit ein noch auf einen 2FA-Code wartender Sidecar EOF
    // sieht und sich beendet, statt den Prozess ewig haengen zu lassen.
    drop(stdin);

    // Nach einem Fehler nicht auf ein geordnetes Ende warten: der Sidecar kann
    // in einem Netz-Timeout oder auf einer vollen stdout-Pipe stehen.
    if failure.is_some() {
        let _ = child.kill().await;
    }
    let status = child.wait().await?;

    if let Some(error) = failure {
        return Err(error);
    }
    if !saw_done {
        return Err(SyncError::UnexpectedExit(status.to_string()));
    }

    // Fällt der Cursor aus, hilft der Katalog selbst weiter.
    if summary.cursor.is_none() {
        summary.cursor = catalog.sync_cursor()?;
    }

    tracing::info!(
        stored = summary.stored,
        reported = summary.reported,
        thumbnails = summary.thumbnails,
        thumbnails_failed = summary.thumbnails_failed,
        "Sync abgeschlossen"
    );
    Ok(summary)
}

fn flush(catalog: &mut Catalog, batch: &mut Vec<PhotoRecord>) -> Result<u64, SyncError> {
    if batch.is_empty() {
        return Ok(0);
    }
    catalog.upsert_batch(batch)?;
    let written = batch.len() as u64;
    batch.clear();
    Ok(written)
}

fn emit(events: &Option<mpsc::UnboundedSender<SyncEvent>>, event: SyncEvent) {
    if let Some(sender) = events {
        // Die UI kann weg sein; ein Sync ohne Zuhörer läuft trotzdem zu Ende.
        let _ = sender.send(event);
    }
}
