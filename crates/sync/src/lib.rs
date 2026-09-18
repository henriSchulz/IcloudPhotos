//! Sync-Orchestrator: startet den Python-Sidecar, liest sein NDJSON-Protokoll
//! und schreibt die Metadaten in den Katalog.

pub mod protocol;
pub mod session;
pub mod sidecar;

pub use protocol::{SidecarMessage, SyncEvent};
pub use session::{Original, SidecarSession};
pub use sidecar::{
    Prompts, RECONCILE_AFTER, SecretRequest, SidecarOptions, SyncSummary, TermsRequest, run_sync,
};

/// Fehler eines Sync-Laufs.
#[derive(Debug, thiserror::Error)]
pub enum SyncError {
    #[error("Sidecar konnte nicht gestartet werden: {command}")]
    Spawn {
        command: String,
        #[source]
        source: std::io::Error,
    },
    #[error("Sidecar-Kommunikation fehlgeschlagen")]
    Io(#[from] std::io::Error),
    #[error("Sidecar gab eine unlesbare Zeile aus: {line}")]
    Protocol {
        line: String,
        #[source]
        source: serde_json::Error,
    },
    #[error("Sidecar meldet Fehler ({kind}): {message}")]
    Sidecar { kind: String, message: String },
    #[error("Anmeldung abgelehnt: {message}")]
    Credentials { message: String },
    #[error("Sidecar endete mit Status {0}, ohne 'done' zu melden")]
    UnexpectedExit(String),
    #[error("2FA-Code wurde nicht bereitgestellt")]
    TwoFactorAborted,
    #[error("Passwort wurde nicht bereitgestellt")]
    PasswordAborted,
    #[error("Die geänderten iCloud-Nutzungsbedingungen wurden nicht akzeptiert")]
    TermsDeclined,
    #[error("Katalog konnte nicht geschrieben werden")]
    Catalog(#[from] icloud_photos_cache::CacheError),
}
