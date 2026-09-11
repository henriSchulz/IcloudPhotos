//! Laden und Validieren von `~/.config/icloud-photos/config.toml`.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::ConfigError;

/// Gesamte Anwendungskonfiguration.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub icloud: ICloud,
    #[serde(default)]
    pub cache: Cache,
    #[serde(default)]
    pub paths: Paths,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ICloud {
    /// Apple-ID. Das Passwort liegt im Keyring, niemals hier.
    #[serde(default)]
    pub username: String,
    #[serde(default = "default_sync_interval")]
    pub sync_interval_minutes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Cache {
    /// Obergrenze des Full-Res-Caches in Gigabyte.
    #[serde(default = "default_limit_gb")]
    pub full_res_cache_limit_gb: f64,
    /// Auf welchen Anteil des Limits nach einer Eviction heruntergeräumt wird
    /// (Hysterese gegen Eviction-Thrashing).
    #[serde(default = "default_target_ratio")]
    pub eviction_target_ratio: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Paths {
    #[serde(default = "default_thumbnail_dir")]
    pub thumbnail_dir: PathBuf,
    #[serde(default = "default_full_res_dir")]
    pub full_res_dir: PathBuf,
    #[serde(default = "default_db_path")]
    pub db_path: PathBuf,
}

fn default_sync_interval() -> u64 {
    15
}
fn default_limit_gb() -> f64 {
    5.0
}
fn default_target_ratio() -> f64 {
    0.9
}
fn default_thumbnail_dir() -> PathBuf {
    PathBuf::from("~/.cache/icloud-photos/thumbnails")
}
fn default_full_res_dir() -> PathBuf {
    PathBuf::from("~/.cache/icloud-photos/fullres")
}
fn default_db_path() -> PathBuf {
    PathBuf::from("~/.local/share/icloud-photos/catalog.sqlite")
}

impl Default for ICloud {
    fn default() -> Self {
        Self {
            username: String::new(),
            sync_interval_minutes: default_sync_interval(),
        }
    }
}

impl Default for Cache {
    fn default() -> Self {
        Self {
            full_res_cache_limit_gb: default_limit_gb(),
            eviction_target_ratio: default_target_ratio(),
        }
    }
}

impl Default for Paths {
    fn default() -> Self {
        Self {
            thumbnail_dir: default_thumbnail_dir(),
            full_res_dir: default_full_res_dir(),
            db_path: default_db_path(),
        }
    }
}

impl Cache {
    /// Hartes Limit in Bytes.
    pub fn limit_bytes(&self) -> u64 {
        (self.full_res_cache_limit_gb * 1e9) as u64
    }

    /// Zielgröße in Bytes, auf die eine Eviction herunterräumt.
    pub fn target_bytes(&self) -> u64 {
        (self.full_res_cache_limit_gb * 1e9 * self.eviction_target_ratio) as u64
    }
}

impl Config {
    /// Standardpfad der Konfigurationsdatei.
    pub fn default_path() -> Result<PathBuf, ConfigError> {
        let dirs =
            directories::ProjectDirs::from("", "", "icloud-photos").ok_or(ConfigError::NoHome)?;
        Ok(dirs.config_dir().join("config.toml"))
    }

    /// Lädt die Konfiguration. Fehlt die Datei, gelten die Defaults.
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                let mut config = Self::default();
                config.expand_paths()?;
                return Ok(config);
            }
            Err(source) => {
                return Err(ConfigError::Read {
                    path: path.to_path_buf(),
                    source,
                });
            }
        };

        let mut config: Self = toml::from_str(&text)?;
        config.validate()?;
        config.expand_paths()?;
        Ok(config)
    }

    /// Schreibt die Konfiguration zurück.
    ///
    /// Enthält die Apple-ID, aber nie das Passwort — das liegt im Keyring.
    /// Die Datei bekommt trotzdem 0600, weil sie eine Kontokennung preisgibt.
    pub fn save(&self, path: &Path) -> Result<(), ConfigError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|source| ConfigError::Read {
                path: parent.to_path_buf(),
                source,
            })?;
        }
        let text = toml::to_string_pretty(self).map_err(|error| {
            ConfigError::Invalid(format!("Konfiguration nicht serialisierbar: {error}"))
        })?;
        std::fs::write(path, text).map_err(|source| ConfigError::Read {
            path: path.to_path_buf(),
            source,
        })?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let permissions = std::fs::Permissions::from_mode(0o600);
            std::fs::set_permissions(path, permissions).map_err(|source| ConfigError::Read {
                path: path.to_path_buf(),
                source,
            })?;
        }
        Ok(())
    }

    fn validate(&self) -> Result<(), ConfigError> {
        let limit = self.cache.full_res_cache_limit_gb;
        if !limit.is_finite() || limit <= 0.0 {
            return Err(ConfigError::Invalid(format!(
                "full_res_cache_limit_gb muss > 0 sein, ist {limit}"
            )));
        }
        let ratio = self.cache.eviction_target_ratio;
        if !ratio.is_finite() || ratio <= 0.0 || ratio > 1.0 {
            return Err(ConfigError::Invalid(format!(
                "eviction_target_ratio muss in (0, 1] liegen, ist {ratio}"
            )));
        }
        if self.icloud.sync_interval_minutes == 0 {
            return Err(ConfigError::Invalid(
                "sync_interval_minutes muss > 0 sein".into(),
            ));
        }
        Ok(())
    }

    /// Ersetzt ein führendes `~` in allen Pfaden durch das Home-Verzeichnis.
    fn expand_paths(&mut self) -> Result<(), ConfigError> {
        let home = directories::BaseDirs::new().ok_or(ConfigError::NoHome)?;
        let home = home.home_dir();
        for path in [
            &mut self.paths.thumbnail_dir,
            &mut self.paths.full_res_dir,
            &mut self.paths.db_path,
        ] {
            *path = expand_tilde(path, home);
        }
        Ok(())
    }
}

fn expand_tilde(path: &Path, home: &Path) -> PathBuf {
    match path.strip_prefix("~") {
        Ok(rest) => home.join(rest),
        Err(_) => path.to_path_buf(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_when_file_missing() {
        let config = Config::load(Path::new("/nonexistent/config.toml")).unwrap();
        assert_eq!(config.cache.full_res_cache_limit_gb, 5.0);
        assert_eq!(config.icloud.sync_interval_minutes, 15);
    }

    #[test]
    fn tilde_is_expanded() {
        let home = Path::new("/home/tester");
        assert_eq!(
            expand_tilde(Path::new("~/.cache/x"), home),
            PathBuf::from("/home/tester/.cache/x")
        );
        assert_eq!(
            expand_tilde(Path::new("/abs/x"), home),
            PathBuf::from("/abs/x")
        );
    }

    #[test]
    fn target_bytes_applies_ratio() {
        let cache = Cache {
            full_res_cache_limit_gb: 5.0,
            eviction_target_ratio: 0.9,
        };
        assert_eq!(cache.limit_bytes(), 5_000_000_000);
        assert_eq!(cache.target_bytes(), 4_500_000_000);
    }

    #[test]
    fn rejects_bad_ratio() {
        let toml = r#"
            [cache]
            eviction_target_ratio = 1.5
        "#;
        let config: Config = toml::from_str(toml).unwrap();
        assert!(config.validate().is_err());
    }

    #[test]
    fn rejects_zero_limit() {
        let toml = r#"
            [cache]
            full_res_cache_limit_gb = 0
        "#;
        let config: Config = toml::from_str(toml).unwrap();
        assert!(config.validate().is_err());
    }

    #[test]
    fn save_then_load_roundtrips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");

        let mut config = Config::default();
        config.icloud.username = "a@b.c".into();
        config.cache.full_res_cache_limit_gb = 3.5;
        config.save(&path).unwrap();

        let loaded = Config::load(&path).unwrap();
        assert_eq!(loaded.icloud.username, "a@b.c");
        assert_eq!(loaded.cache.full_res_cache_limit_gb, 3.5);

        // Die Datei nennt das Konto, also nicht fuer andere lesbar.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o077, 0, "Gruppe/Andere duerfen nicht lesen");
        }
    }

    #[test]
    fn saved_config_never_contains_a_password() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let mut config = Config::default();
        config.icloud.username = "a@b.c".into();
        config.save(&path).unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.to_lowercase().contains("password"));
        assert!(!text.to_lowercase().contains("passwort"));
    }

    #[test]
    fn parses_documented_example() {
        let toml = r#"
            [icloud]
            username = "a@b.c"
            sync_interval_minutes = 15

            [cache]
            full_res_cache_limit_gb = 5
            eviction_target_ratio = 0.9

            [paths]
            thumbnail_dir = "~/.cache/icloud-photos/thumbnails"
            full_res_dir = "~/.cache/icloud-photos/fullres"
            db_path = "~/.local/share/icloud-photos/catalog.sqlite"
        "#;
        let config: Config = toml::from_str(toml).unwrap();
        config.validate().unwrap();
        assert_eq!(config.icloud.username, "a@b.c");
    }
}
