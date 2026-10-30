//! „Edit in Retouch" — gibt Fotos an Henris Bildbearbeitung Retouch
//! (`~/Projects/foto-app`, Python-Server + Web-Oberfläche) weiter.
//!
//! Retouch kennt iCloud Photos schon: `POST /api/icloud/import` mit der
//! Katalog-ID legt dort eine Bearbeitung an und nimmt das Original aus dem
//! Full-Res-Cache dieser App. Deshalb lädt die App die Originale vorher selbst
//! (derselbe Auftragsweg wie Sichern/Öffnen) — Retouch muss dann nichts aus
//! iCloud holen.
//!
//! Reihenfolge: Server sicherstellen (startet ihn wie `bin/foto`, ohne
//! Fenster), Fotos übergeben, erst dann das Fenster öffnen (`bin/foto`). Die
//! Seite liest ihre Liste nur beim Laden; ein Fenster vor dem Import zeigte
//! die neuen Bearbeitungen nicht.
//!
//! `ICLOUD_PHOTOS_RETOUCH=<Programm>` ersetzt den ganzen Weg: das Programm
//! bekommt die IDs als Argumente (Prüfstand: ein Stub, der nur mitschreibt).
//! `RETOUCH_DIR` und `FOTO_PORT` folgen Retouchs eigenen Namen.

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn retouch_dir() -> Option<PathBuf> {
    std::env::var_os("RETOUCH_DIR")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|home| PathBuf::from(home).join("Projects/foto-app"))
        })
}

fn base_url() -> String {
    let port = std::env::var("FOTO_PORT").unwrap_or_else(|_| "8790".to_string());
    format!("http://localhost:{port}")
}

/// Ob es Retouch auf diesem Rechner gibt — sonst bietet die App es nicht an.
pub fn available() -> bool {
    std::env::var_os("ICLOUD_PHOTOS_RETOUCH").is_some()
        || retouch_dir().is_some_and(|dir| dir.join("bin/foto").is_file())
}

/// Übergibt die Fotos. Läuft blockierend — aus einem Hilfsthread rufen.
pub fn hand_off(ids: &[String]) -> Result<(), String> {
    if ids.is_empty() {
        return Ok(());
    }
    if let Some(program) = std::env::var_os("ICLOUD_PHOTOS_RETOUCH") {
        return Command::new(program)
            .args(ids)
            .status()
            .map_err(|error| format!("Retouch couldn’t be started: {error}"))
            .and_then(|status| {
                status
                    .success()
                    .then_some(())
                    .ok_or_else(|| "Retouch didn’t take the photos.".to_string())
            });
    }
    let dir = retouch_dir().ok_or_else(|| "Retouch isn’t installed.".to_string())?;
    let launcher = dir.join("bin/foto");
    if !launcher.is_file() {
        return Err("Retouch isn’t installed.".to_string());
    }

    let agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(120)))
        .build()
        .new_agent();
    let url = base_url();
    let up = |agent: &ureq::Agent| {
        agent
            .get(&format!("{url}/api/jobs"))
            .config()
            .timeout_global(Some(Duration::from_millis(400)))
            .build()
            .call()
            .is_ok()
    };
    if !up(&agent) {
        start_server(&dir)?;
        let deadline = Instant::now() + Duration::from_secs(6);
        while !up(&agent) {
            if Instant::now() > deadline {
                return Err("Retouch didn’t start.".to_string());
            }
            std::thread::sleep(Duration::from_millis(150));
        }
    }
    for id in ids {
        let body = serde_json::json!({ "photo": id });
        let result = agent
            .post(&format!("{url}/api/icloud/import"))
            .send_json(&body);
        if let Err(error) = result {
            tracing::warn!(%id, %error, "Retouch hat das Foto nicht angenommen");
            return Err("Retouch couldn’t open the photo.".to_string());
        }
    }
    Command::new(&launcher)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| format!("Retouch couldn’t be opened: {error}"))?;
    Ok(())
}

/// Startet den Server wie `bin/foto` es tut, nur ohne Fenster: eigene
/// Sitzung, Ausgabe in Retouchs Log.
fn start_server(dir: &std::path::Path) -> Result<(), String> {
    let log_dir = std::env::var_os("HOME")
        .map(|home| PathBuf::from(home).join(".cache/foto-app"))
        .ok_or_else(|| "No home directory.".to_string())?;
    std::fs::create_dir_all(&log_dir).map_err(|error| error.to_string())?;
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_dir.join("server.log"))
        .map_err(|error| error.to_string())?;
    let err = log.try_clone().map_err(|error| error.to_string())?;
    Command::new("setsid")
        .arg("python3")
        .arg(dir.join("server.py"))
        .stdin(Stdio::null())
        .stdout(log)
        .stderr(err)
        .spawn()
        .map(|_| ())
        .map_err(|error| format!("Retouch couldn’t be started: {error}"))
}
