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

/// So lange gilt ein vollstaendiger Durchlauf als frisch.
///
/// Der laufende Abgleich geht ueber den Zone-Token und kostet bei
/// unveraenderter Bibliothek einen Request. Der vollstaendige Durchlauf ist nur
/// das Sicherheitsnetz gegen langsames Abdriften — bei 19.646 Assets sind das
/// rund 170 Seitenabrufe, das gehoert nicht in den Tagesrhythmus.
pub const RECONCILE_AFTER: chrono::TimeDelta = chrono::TimeDelta::days(7);

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
    /// Zone-Token des letzten erfolgreichen Laufs. Nur setzen, wenn der
    /// Erstaufbau durch ist — sonst ueberspringt der Delta-Lauf den Rest der
    /// Bibliothek.
    pub since_token: Option<String>,
    /// Position, an der ein abgebrochener Erstaufbau weitermacht.
    pub offset: u64,
    /// Hoechstens so viele Assets pro Lauf. Ein so abgeschnittener Durchlauf
    /// gilt nicht als vollstaendiger Abgleich.
    pub limit: Option<u64>,
    /// Delta ueberspringen und vollstaendig durchlaufen (periodischer Abgleich).
    pub force_baseline: bool,
    /// Wohin Vorschaubilder geladen werden. `None` heisst: nur Metadaten.
    pub thumbnail_dir: Option<PathBuf>,
    /// Statt eines Syncs nur Vorschauen fuer diese IDs nachladen.
    pub fetch_thumbnails_for: Vec<String>,
    /// Testmodus: so viele synthetische Datensätze statt echter iCloud-Zugriffe.
    pub fake: Option<u64>,
    /// Testmodus: den 2FA-Dialog durchspielen.
    pub fake_two_factor: bool,
    /// Testmodus: die Passwortabfrage durchspielen.
    pub fake_password: bool,
    /// Testmodus: je Asset so lange warten, damit das Einlaufen sichtbar wird.
    pub fake_delay_ms: u64,
    /// Testmodus: so viele Assets mehr zaehlen als liefern. Stellt die
    /// Bestandsluecke nach, die es in echten Bibliotheken gibt.
    pub fake_missing: u64,
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
            since_token: None,
            offset: 0,
            limit: None,
            force_baseline: false,
            thumbnail_dir: None,
            fetch_thumbnails_for: Vec::new(),
            fake: None,
            fake_two_factor: false,
            fake_password: false,
            fake_delay_ms: 0,
            fake_missing: 0,
        }
    }

    fn argv(&self) -> Vec<String> {
        let mut args = vec![self.script.to_string_lossy().into_owned()];
        if let Some(dir) = &self.thumbnail_dir {
            args.push("--thumbs".into());
            args.push(dir.to_string_lossy().into_owned());
        }
        // Vor dem Fake-Zweig: der Fake-Sidecar muss dieselben Schalter
        // auswerten, sonst testet er ein anderes Verhalten als der echte.
        if let Some(since) = self.since {
            args.push("--since".into());
            args.push(since.to_rfc3339());
        }
        if let Some(token) = &self.since_token {
            args.push("--since-token".into());
            args.push(token.clone());
        }
        if self.offset > 0 {
            args.push("--offset".into());
            args.push(self.offset.to_string());
        }
        if self.force_baseline {
            args.push("--force-baseline".into());
        }
        if let Some(limit) = self.limit {
            args.push("--limit".into());
            args.push(limit.to_string());
        }
        if let Some(count) = self.fake {
            args.push("--fake".into());
            args.push(count.to_string());
            if self.fake_two_factor {
                args.push("--fake-2fa".into());
            }
            if self.fake_password {
                args.push("--fake-password".into());
            }
            if self.fake_delay_ms > 0 {
                args.push("--fake-delay-ms".into());
                args.push(self.fake_delay_ms.to_string());
            }
            if self.fake_missing > 0 {
                args.push("--fake-missing".into());
                args.push(self.fake_missing.to_string());
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
        args
    }

    /// Argumente fuer den dauerhaft laufenden Serve-Modus.
    pub fn serve_argv(&self) -> Vec<String> {
        let mut args = vec![self.script.to_string_lossy().into_owned(), "--serve".into()];
        if let Some(count) = self.fake {
            args.push("--fake".into());
            args.push(count.to_string());
            if self.fake_password {
                args.push("--fake-password".into());
            }
            return args;
        }
        args.push("--username".into());
        args.push(self.username.clone());
        if let Some(dir) = &self.cookie_dir {
            args.push("--cookie-dir".into());
            args.push(dir.to_string_lossy().into_owned());
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
    /// Die Bibliothek war unveraendert — dieser Lauf kostete einen Request.
    pub unchanged: bool,
    /// So viele in iCloud geloeschte Fotos wurden ausgetragen.
    pub deleted: u64,
    /// Neuer Zone-Token fuer den naechsten Lauf.
    pub new_token: Option<String>,
    /// Ob in diesem Lauf ein vollstaendiger Durchlauf zu Ende kam.
    pub baseline_complete: bool,
    /// Der Delta-Abgleich war nicht moeglich.
    pub token_expired: bool,
    /// Geaenderte Records ohne zugehoeriges Asset.
    pub unresolved: u64,
    /// Bestand laut iCloud, sofern der Sidecar ihn gemeldet hat.
    pub library_count: Option<u64>,
    /// Neuer Cursor für den nächsten Delta-Sync.
    pub cursor: Option<DateTime<Utc>>,
}

/// Kanal, über den die UI nach einem Geheimnis gefragt wird — Passwort oder
/// 2FA-Code. Die Antwort geht über den enthaltenen `oneshot`-Sender zurück.
pub type SecretRequest = oneshot::Sender<String>;

/// Rückkanäle zur UI. Fehlt einer, bricht der Sync ab, sobald der Sidecar
/// danach fragt.
#[derive(Default)]
pub struct Prompts {
    /// Fortschrittsmeldungen.
    pub events: Option<mpsc::UnboundedSender<SyncEvent>>,
    /// Abfrage des Apple-ID-Passworts.
    pub password: Option<mpsc::UnboundedSender<SecretRequest>>,
    /// Abfrage des 2FA-Codes.
    pub two_factor: Option<mpsc::UnboundedSender<SecretRequest>>,
}

impl Prompts {
    /// Nur Fortschritt, keine interaktiven Abfragen — für Tests und
    /// Hintergrundläufe mit gültiger Sitzung.
    pub fn events_only(events: mpsc::UnboundedSender<SyncEvent>) -> Self {
        Self {
            events: Some(events),
            ..Default::default()
        }
    }

    /// Fragt nach dem Apple-ID-Passwort.
    pub async fn ask_password(&self) -> Option<String> {
        Self::ask(&self.password).await
    }

    /// Fragt nach dem 2FA-Code.
    pub async fn ask_two_factor(&self) -> Option<String> {
        Self::ask(&self.two_factor).await
    }

    /// Fragt über `channel` nach einem Geheimnis und wartet auf die Antwort.
    async fn ask(channel: &Option<mpsc::UnboundedSender<SecretRequest>>) -> Option<String> {
        let channel = channel.as_ref()?;
        let (tx, rx) = oneshot::channel();
        if channel.send(tx).is_err() {
            return None;
        }
        rx.await.ok()
    }
}

/// Führt einen Sync aus: startet den Sidecar, schreibt alle Metadaten in den
/// Katalog und gibt Fortschritt über `events` aus.
///
/// Fragt der Sidecar nach Passwort oder 2FA-Code, geht eine [`SecretRequest`]
/// über den passenden Kanal in `prompts`; der Sync wartet auf die Antwort.
pub async fn run_sync(
    catalog: &mut Catalog,
    options: &SidecarOptions,
    prompts: &Prompts,
) -> Result<SyncSummary, SyncError> {
    let events = &prompts.events;
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
                        events,
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
                    events,
                    SyncEvent::Thumbnails {
                        stored: summary.thumbnails,
                    },
                );
            }
            SidecarMessage::Cursor { token } => {
                summary.new_token = token;
            }
            SidecarMessage::Unchanged => {
                tracing::info!("Bibliothek unveraendert, kein Durchlauf noetig");
                summary.unchanged = true;
                emit(events, SyncEvent::Unchanged);
            }
            SidecarMessage::Deleted { id } => {
                // Erst ausstehende Metadaten schreiben, damit ein in derselben
                // Runde angelegtes Foto auch wirklich wieder verschwindet.
                summary.stored += flush(catalog, &mut batch)?;
                for path in catalog.delete_photo(&id)? {
                    if let Err(error) = std::fs::remove_file(&path)
                        && error.kind() != std::io::ErrorKind::NotFound
                    {
                        tracing::warn!(
                            path = %path.display(),
                            %error,
                            "Cache-Datei eines geloeschten Fotos blieb liegen"
                        );
                    }
                }
                summary.deleted += 1;
                emit(
                    events,
                    SyncEvent::Deleted {
                        removed: summary.deleted,
                    },
                );
            }
            SidecarMessage::Offset { value } => {
                // Fortschritt sofort festhalten: ein Abbruch soll nicht den
                // ganzen Erstaufbau kosten.
                summary.stored += flush(catalog, &mut batch)?;
                catalog.set_baseline_progress(value, false, Utc::now())?;
                emit(
                    events,
                    SyncEvent::Progress {
                        stored: summary.stored,
                    },
                );
            }
            SidecarMessage::LibraryCount { value } => {
                summary.library_count = Some(value);
                emit(events, SyncEvent::LibraryCount { total: value });
            }
            SidecarMessage::BaselineComplete { count } => {
                tracing::info!(count, "vollstaendiger Durchlauf beendet");
                summary.baseline_complete = true;
            }
            SidecarMessage::TokenExpired { message } => {
                tracing::warn!(%message, "Delta nicht moeglich, Sidecar faellt zurueck");
                summary.token_expired = true;
            }
            SidecarMessage::ChangeUnresolved {
                id,
                record_type,
                message,
            } => {
                tracing::debug!(%id, ?record_type, %message, "geaenderter Record ohne Asset");
                summary.unresolved += 1;
            }
            SidecarMessage::ThumbFailed { id, message } => {
                tracing::warn!(%id, %message, "Vorschau konnte nicht geladen werden");
                summary.thumbnails_failed += 1;
            }
            SidecarMessage::PasswordRequired { username } => {
                tracing::info!(%username, "Sidecar fordert Passwort an");
                emit(events, SyncEvent::PasswordRequired);

                let Some(password) = Prompts::ask(&prompts.password).await else {
                    failure = Some(SyncError::PasswordAborted);
                    break;
                };
                stdin.write_all(format!("{password}\n").as_bytes()).await?;
                stdin.flush().await?;
            }
            SidecarMessage::AuthRequired { method } => {
                tracing::info!(%method, "Sidecar fordert 2FA-Code an");
                emit(events, SyncEvent::TwoFactorRequired);

                let Some(code) = Prompts::ask(&prompts.two_factor).await else {
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
                emit(events, SyncEvent::Authenticated { trusted });

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
                failure = Some(if kind == "credentials" {
                    SyncError::Credentials { message }
                } else {
                    SyncError::Sidecar { kind, message }
                });
                break;
            }
        }
    }

    // Restliche Datensätze auch bei Abbruch sichern — ein Teil-Sync ist besser
    // als ein verworfener, der Cursor kommt dann eben erst beim nächsten Lauf.
    summary.stored += flush(catalog, &mut batch)?;
    emit(
        events,
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

    // Ein reiner Nachlade-Lauf fuer Vorschauen ist kein Abgleich und sagt
    // ueber den Stand der Bibliothek nichts aus.
    if options.is_thumbnail_backfill() {
        return Ok(summary);
    }

    // Ab hier gilt der Lauf als durchgelaufen — auch ein "unveraendert" ist
    // ein Abgleich. Die Anzeige "zuletzt aktualisiert" haengt daran.
    catalog.set_last_sync(Utc::now())?;

    if summary.baseline_complete {
        catalog.set_baseline_progress(0, true, Utc::now())?;
    }

    // Bestandsabgleich. `photos.all` zaehlt Dinge mit, die nie als Asset
    // ankommen — an einer Bibliothek mit 19.646 gezaehlten Assets fehlten
    // dauerhaft 77. Diese Luecke ist strukturell; ein erneuter Durchlauf
    // findet sie auch nicht.
    //
    // Frueher loeste jede Abweichung einen neuen Vollabgleich aus. Das ergab
    // eine Schleife: Durchlauf fertig -> Zahlen passen nicht -> Merker zurueck
    // -> naechster Lauf faengt von vorn an, und das bei jeder Synchronisation.
    // Fuer zehn neue Fotos lief die App durch die ganze Bibliothek.
    //
    // Gemerkt wird deshalb, wie gross die Luecke nach einem vollstaendigen
    // Durchlauf war. Nur was darueber hinausgeht, ist eine verpasste Aenderung
    // — und nur die erzwingt einen neuen Durchlauf.
    if let Some(expected) = summary.library_count {
        let local = catalog.count()?;
        // Mehr lokal als gemeldet heisst: hier stehen noch Fotos, die in
        // iCloud geloescht wurden. Die raeumt der Delta-Lauf ueber Tombstones
        // weg, kein Vollabgleich.
        let gap = expected.saturating_sub(local);

        if summary.baseline_complete {
            if gap > 0 {
                tracing::info!(
                    local,
                    expected,
                    gap,
                    "Bestandsluecke nach vollstaendigem Durchlauf — wird als strukturell vermerkt"
                );
            }
            catalog.set_baseline_gap(gap)?;
        } else if options.since_token.is_some() {
            // Nur ein Delta-Lauf kann eine Aenderung verpasst haben. Ein
            // abgeschnittener Durchlauf hat schlicht noch nicht alles gesehen —
            // wuerde er hier zuruecksetzen, verwuerfe er seinen eigenen
            // Fortschritt.
            let known = catalog.sync_state()?.baseline_gap;
            if gap > known {
                tracing::warn!(
                    local,
                    expected,
                    gap,
                    known,
                    "Bestand weicht staerker ab als bekannt — naechster Lauf gleicht vollstaendig ab"
                );
                catalog.set_baseline_progress(0, false, Utc::now())?;
            }
        }
    }

    // Den Token nur uebernehmen, wenn der lokale Stand ihn auch verdient.
    // Nach einem abgebrochenen Erstaufbau waere er eine Luege: der naechste
    // Lauf wuerde ein Delta ziehen und den Rest der Bibliothek nie sehen.
    let token_is_earned = summary.unchanged
        || summary.baseline_complete
        || (options.since_token.is_some() && !summary.token_expired);
    if token_is_earned && let Some(token) = &summary.new_token {
        catalog.set_zone_token(Some(token))?;
    } else if !token_is_earned {
        tracing::info!("Durchlauf unvollstaendig — Zone-Token wird nicht uebernommen");
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
