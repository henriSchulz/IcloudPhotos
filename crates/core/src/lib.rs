//! Geteilte Basistypen: Konfiguration, Foto-Modell, Fehlertypen.

pub mod config;
pub mod model;

pub use config::Config;
pub use model::Photo;

/// Fehler beim Laden oder Validieren der Konfiguration.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("Konfiguration konnte nicht gelesen werden: {path}")]
    Read {
        path: std::path::PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("Konfiguration ist kein gültiges TOML: {0}")]
    Parse(#[from] toml::de::Error),
    #[error("Konfigurationswert ungültig: {0}")]
    Invalid(String),
    #[error("Home-Verzeichnis konnte nicht ermittelt werden")]
    NoHome,
}
