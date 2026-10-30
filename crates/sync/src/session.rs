//! Dauerhaft laufender Sidecar für Originale auf Abruf.
//!
//! Ein Prozessstart samt Anmeldung kostet Sekunden. Das darf nicht bei jedem
//! geöffneten Foto anfallen, deshalb bleibt für Full-Res ein Sidecar stehen
//! und beantwortet Befehle über stdin.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use serde::Deserialize;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

use crate::sidecar::{Prompts, SidecarOptions};
use crate::{SidecarMessage, SyncError, SyncEvent};

/// Antwort des Sidecars auf einen Download-Befehl.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum SessionMessage {
    Ready,
    Original {
        id: String,
        path: PathBuf,
        bytes: u64,
    },
    OriginalFailed {
        id: String,
        message: String,
        /// `missing` heisst: das Asset gibt es nicht. Alles andere kann ein
        /// zweiter Anlauf beheben. Aeltere Sidecars kennen das Feld nicht.
        #[serde(default = "retryable")]
        kind: String,
    },
    PasswordRequired {
        #[serde(default)]
        username: String,
    },
    AuthRequired {
        #[serde(default)]
        method: String,
    },
    TermsRequired {
        #[serde(default)]
        url: String,
    },
    AuthOk {
        #[serde(default)]
        trusted: bool,
    },
    Error {
        #[serde(default)]
        kind: String,
        message: String,
    },
    /// Das Foto liegt in iCloud jetzt unter "Zuletzt gelöscht".
    PhotoDeleted {
        id: String,
    },
    DeleteFailed {
        id: String,
        message: String,
        #[serde(default = "retryable")]
        kind: String,
    },
    /// `isFavorite` ist gesetzt.
    FavoriteSet {
        id: String,
    },
    FavoriteFailed {
        id: String,
        message: String,
        #[serde(default = "retryable")]
        kind: String,
    },
    /// Die Datei ist in iCloud. `id` fehlt, solange iCloud sie noch nicht
    /// indiziert hat (oder sie schon kannte) — der nächste Sync bringt sie.
    Uploaded {
        path: PathBuf,
        #[serde(default)]
        id: Option<String>,
    },
    UploadFailed {
        path: PathBuf,
        message: String,
    },
}

/// Ein Auftrag an den Sidecar im Serve-Modus.
#[derive(Debug, Clone)]
pub enum Job {
    /// Original holen; `master` spart einen CloudKit-Lookup.
    Original {
        id: String,
        master: Option<String>,
        dest: PathBuf,
    },
    /// In iCloud löschen (nach "Zuletzt gelöscht", 30 Tage zurückholbar).
    Delete { id: String, master: Option<String> },
    /// Eine Datei in die Mediathek laden.
    Upload { path: PathBuf },
    /// Favorit setzen (`true`) oder lösen.
    Favorite {
        id: String,
        master: Option<String>,
        value: bool,
    },
}

/// Was ein erledigter Auftrag ergibt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Done {
    Original(Original),
    Deleted,
    Favorited,
    Uploaded { id: Option<String> },
}

/// Ein geladenes Original.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Original {
    pub path: PathBuf,
    pub bytes: u64,
}

/// Ein laufender Sidecar im Serve-Modus.
pub struct SidecarSession {
    child: Child,
    stdin: ChildStdin,
    lines: Lines<BufReader<ChildStdout>>,
}

/// Vorgabe fuer `original_failed` ohne `kind`: im Zweifel noch einmal
/// versuchen. Ein Sidecar aelteren Standes meldet das Feld nicht.
fn retryable() -> String {
    "error".to_string()
}

impl SidecarSession {
    /// Startet den Sidecar und wartet, bis er Befehle annimmt.
    ///
    /// Fragt er dabei nach Passwort oder 2FA, laufen die Abfragen über
    /// `prompts` — genauso wie beim Sync.
    pub async fn start(options: &SidecarOptions, prompts: &Prompts) -> Result<Self, SyncError> {
        let mut args = options.serve_argv();
        let command_line = format!("{} {}", options.python.display(), args.join(" "));
        tracing::info!(command = %command_line, "starte Sidecar im Serve-Modus");

        let mut child = Command::new(&options.python)
            .args(std::mem::take(&mut args))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .map_err(|source| SyncError::Spawn {
                command: command_line,
                source,
            })?;

        let stdout = child.stdout.take().expect("stdout als Pipe angefordert");
        let mut stdin = child.stdin.take().expect("stdin als Pipe angefordert");
        let mut lines = BufReader::new(stdout).lines();

        // Bis `ready` durchlaufen und dabei die Anmeldung erledigen.
        loop {
            let Some(line) = lines.next_line().await? else {
                return Err(SyncError::UnexpectedExit(
                    "Sidecar endete vor 'ready'".to_string(),
                ));
            };
            if line.trim().is_empty() {
                continue;
            }
            let message: SessionMessage =
                serde_json::from_str(&line).map_err(|source| SyncError::Protocol {
                    line: line.clone(),
                    source,
                })?;

            match message {
                SessionMessage::Ready => break,
                SessionMessage::AuthOk { trusted } => {
                    emit(prompts, SyncEvent::Authenticated { trusted });
                }
                SessionMessage::PasswordRequired { .. } => {
                    emit(prompts, SyncEvent::PasswordRequired);
                    let Some(secret) = prompts.ask_password().await else {
                        return Err(SyncError::PasswordAborted);
                    };
                    write_line(&mut stdin, &secret).await?;
                }
                SessionMessage::AuthRequired { .. } => {
                    emit(prompts, SyncEvent::TwoFactorRequired);
                    let Some(code) = prompts.ask_two_factor().await else {
                        return Err(SyncError::TwoFactorAborted);
                    };
                    write_line(&mut stdin, &code).await?;
                }
                SessionMessage::TermsRequired { url } => {
                    if !prompts.ask_terms(&url).await {
                        return Err(SyncError::TermsDeclined);
                    }
                    write_line(&mut stdin, "accept").await?;
                }
                SessionMessage::Error { kind, message } if kind == "credentials" => {
                    return Err(SyncError::Credentials { message });
                }
                SessionMessage::Error { kind, .. } if kind == "terms" => {
                    return Err(SyncError::TermsDeclined);
                }
                SessionMessage::Error { kind, message } => {
                    return Err(SyncError::Sidecar { kind, message });
                }
                other => {
                    tracing::debug!(?other, "unerwartete Zeile vor 'ready'");
                }
            }
        }

        Ok(Self {
            child,
            stdin,
            lines,
        })
    }

    /// Fordert ein Original an und wartet auf die Antwort.
    /// `master` ist der CPLMaster-Record aus dem Katalog. Ist er bekannt,
    /// braucht der Sidecar nur einen einzigen CloudKit-Lookup statt zwei.
    pub async fn fetch_original(
        &mut self,
        id: &str,
        master: Option<&str>,
        dest: &Path,
    ) -> Result<Original, SyncError> {
        let job = Job::Original {
            id: id.to_string(),
            master: master.map(str::to_string),
            dest: dest.to_path_buf(),
        };
        match self.run(&job).await? {
            Done::Original(original) => Ok(original),
            _ => Err(SyncError::UnexpectedExit("falsche Antwort".into())),
        }
    }

    /// Schickt einen Auftrag und wartet auf genau seine Antwort.
    pub async fn run(&mut self, command: &Job) -> Result<Done, SyncError> {
        let line = match command {
            Job::Original { id, master, dest } => serde_json::json!({
                "cmd": "original",
                "id": id,
                "master": master,
                "dest": dest.to_string_lossy(),
            }),
            Job::Delete { id, master } => serde_json::json!({
                "cmd": "delete",
                "id": id,
                "master": master,
            }),
            Job::Upload { path } => serde_json::json!({
                "cmd": "upload",
                "path": path.to_string_lossy(),
            }),
            Job::Favorite { id, master, value } => serde_json::json!({
                "cmd": "favorite",
                "id": id,
                "master": master,
                "value": value,
            }),
        };
        write_line(&mut self.stdin, &line.to_string()).await?;

        loop {
            let Some(line) = self.lines.next_line().await? else {
                return Err(SyncError::UnexpectedExit(
                    "Sidecar endete waehrend eines Downloads".to_string(),
                ));
            };
            if line.trim().is_empty() {
                continue;
            }
            let message: SessionMessage =
                serde_json::from_str(&line).map_err(|source| SyncError::Protocol {
                    line: line.clone(),
                    source,
                })?;

            match (command, message) {
                (
                    Job::Original { id, .. },
                    SessionMessage::Original {
                        id: got,
                        path,
                        bytes,
                    },
                ) if &got == id => return Ok(Done::Original(Original { path, bytes })),
                (
                    Job::Original { id, .. },
                    SessionMessage::OriginalFailed {
                        id: got,
                        message,
                        kind,
                    },
                ) if &got == id => {
                    return Err(SyncError::Sidecar { kind, message });
                }
                (Job::Delete { id, .. }, SessionMessage::PhotoDeleted { id: got })
                    if &got == id =>
                {
                    return Ok(Done::Deleted);
                }
                (
                    Job::Delete { id, .. },
                    SessionMessage::DeleteFailed {
                        id: got,
                        message,
                        kind,
                    },
                ) if &got == id => {
                    return Err(SyncError::Sidecar { kind, message });
                }
                (Job::Favorite { id, .. }, SessionMessage::FavoriteSet { id: got })
                    if &got == id =>
                {
                    return Ok(Done::Favorited);
                }
                (
                    Job::Favorite { id, .. },
                    SessionMessage::FavoriteFailed {
                        id: got,
                        message,
                        kind,
                    },
                ) if &got == id => {
                    return Err(SyncError::Sidecar { kind, message });
                }
                (Job::Upload { path }, SessionMessage::Uploaded { path: got, id })
                    if &got == path =>
                {
                    return Ok(Done::Uploaded { id });
                }
                (Job::Upload { path }, SessionMessage::UploadFailed { path: got, message })
                    if &got == path =>
                {
                    return Err(SyncError::Sidecar {
                        kind: "error".into(),
                        message,
                    });
                }
                (_, SessionMessage::Error { kind, message }) => {
                    return Err(SyncError::Sidecar { kind, message });
                }
                // Antwort auf einen anderen Befehl oder eine Statuszeile:
                // weiterlesen, statt den Aufrufer mit Fremdem zu bedienen.
                (_, other) => tracing::debug!(?other, "Zeile gehoert nicht zu dieser Anfrage"),
            }
        }
    }

    /// Beendet den Sidecar geordnet.
    pub async fn shutdown(mut self) {
        let _ = write_line(&mut self.stdin, r#"{"cmd":"quit"}"#).await;
        drop(self.stdin);
        let _ = self.child.wait().await;
    }
}

async fn write_line(stdin: &mut ChildStdin, payload: &str) -> Result<(), SyncError> {
    stdin.write_all(payload.as_bytes()).await?;
    stdin.write_all(b"\n").await?;
    stdin.flush().await?;
    Ok(())
}

fn emit(prompts: &Prompts, event: SyncEvent) {
    if let Some(sender) = &prompts.events {
        let _ = sender.send(event);
    }
}

// `SidecarMessage` wird hier nicht gebraucht, der Import haelt aber die
// Protokolldefinition an einem Ort zusammen.
const _: Option<SidecarMessage> = None;
