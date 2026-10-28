//! Sync-Orchestrator: startet den Python-Sidecar, liest sein NDJSON-Protokoll
//! und schreibt die Metadaten in den Katalog.

pub mod protocol;
pub mod session;
pub mod sidecar;

pub use protocol::{SidecarMessage, SyncEvent};
pub use session::{Done, Job, Original, SidecarSession};
pub use sidecar::{
    Prompts, RECONCILE_AFTER, SecretRequest, SidecarOptions, SyncSummary, TermsRequest, run_sync,
};

/// Fehler eines Sync-Laufs.
#[derive(Debug, thiserror::Error)]
pub enum SyncError {
    #[error("Could not start the sidecar: {command}")]
    Spawn {
        command: String,
        #[source]
        source: std::io::Error,
    },
    #[error("Communication with the sidecar failed")]
    Io(#[from] std::io::Error),
    #[error("The sidecar sent an unreadable line: {line}")]
    Protocol {
        line: String,
        #[source]
        source: serde_json::Error,
    },
    #[error("The sidecar reported an error ({kind}): {message}")]
    Sidecar { kind: String, message: String },
    #[error("Sign-in rejected: {message}")]
    Credentials { message: String },
    #[error("The sidecar exited with status {0} without reporting 'done'")]
    UnexpectedExit(String),
    #[error("No two-factor code was provided")]
    TwoFactorAborted,
    #[error("No password was provided")]
    PasswordAborted,
    #[error("The updated iCloud Terms and Conditions were not accepted")]
    TermsDeclined,
    #[error("Could not write to the library database")]
    Catalog(#[from] icloud_photos_cache::CacheError),
}
