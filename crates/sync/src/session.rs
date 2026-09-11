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
    },
    PasswordRequired {
        #[serde(default)]
        username: String,
    },
    AuthRequired {
        #[serde(default)]
        method: String,
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
                SessionMessage::Error { kind, message } if kind == "credentials" => {
                    return Err(SyncError::Credentials { message });
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
    pub async fn fetch_original(&mut self, id: &str, dest: &Path) -> Result<Original, SyncError> {
        let command = serde_json::json!({
            "cmd": "original",
            "id": id,
            "dest": dest.to_string_lossy(),
        });
        write_line(&mut self.stdin, &command.to_string()).await?;

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

            match message {
                SessionMessage::Original {
                    id: got,
                    path,
                    bytes,
                } if got == id => return Ok(Original { path, bytes }),
                SessionMessage::OriginalFailed { id: got, message } if got == id => {
                    return Err(SyncError::Sidecar {
                        kind: "download".to_string(),
                        message,
                    });
                }
                SessionMessage::Error { kind, message } => {
                    return Err(SyncError::Sidecar { kind, message });
                }
                // Antwort auf einen anderen Befehl oder eine Statuszeile:
                // weiterlesen, statt den Aufrufer mit Fremdem zu bedienen.
                other => tracing::debug!(?other, "Zeile gehoert nicht zu dieser Anfrage"),
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
