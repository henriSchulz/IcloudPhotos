//! Geteilte Basistypen: Konfiguration, Foto-Modell, Fehlertypen.

pub mod config;
pub mod model;

pub use config::Config;
pub use model::Photo;

/// Fehler beim Laden oder Validieren der Konfiguration.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("Could not read the configuration: {path}")]
    Read {
        path: std::path::PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("The configuration is not valid TOML: {0}")]
    Parse(#[from] toml::de::Error),
    #[error("Invalid configuration value: {0}")]
    Invalid(String),
    #[error("Could not determine the home directory")]
    NoHome,
}
