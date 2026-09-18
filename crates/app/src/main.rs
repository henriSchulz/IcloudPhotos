//! iCloud Photos — nativer Client für Omarchy.

mod collections;
mod demo;
mod downloader;
mod exif_info;
mod export;
mod geocode;
mod info_panel;
mod photo_object;
mod sidebar;
mod sync_manager;
mod sync_task;
mod viewer;
mod window;
mod zoom_view;

use std::rc::Rc;

use adw::prelude::*;
use icloud_photos_cache::{Catalog, enforce_limit, housekeeping};
use icloud_photos_core::Config;

const APP_ID: &str = "de.henri.IcloudPhotos";

/// Sicherheitsnetz gegen verwaiste Cache-Dateien nach Abstürzen.
const HOUSEKEEPING_INTERVAL: std::time::Duration = std::time::Duration::from_secs(10 * 60);

fn main() -> anyhow::Result<()> {
    prefer_gl_renderer();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let config_path = Config::default_path()?;
    let config = Config::load(&config_path)?;
    tracing::info!(path = %config_path.display(), "Konfiguration geladen");

    std::fs::create_dir_all(&config.paths.thumbnail_dir)?;
    std::fs::create_dir_all(&config.paths.full_res_dir)?;

    let catalog = Catalog::open(&config.paths.db_path)?;

    // Einmal beim Start aufräumen, damit die Cache-Summe stimmt, bevor die
    // erste Eviction darauf rechnet.
    match housekeeping::sweep(&catalog, &config.paths.full_res_dir) {
        Ok(report) if !report.is_clean() => {
            tracing::warn!(
                orphans = report.orphan_files_removed,
                stale = report.stale_rows_cleared,
                "Cache war nach dem letzten Lauf inkonsistent"
            );
        }
        Ok(_) => {}
        Err(error) => tracing::error!(%error, "Housekeeping fehlgeschlagen"),
    }

    if std::env::args().any(|arg| arg == "--demo") {
        demo::seed(&catalog, &config)?;
    }

    let state = window::AppState::new(config, config_path, catalog);

    // GApplication ist per D-Bus Single-Instance. Ein Testlauf mit derselben
    // ID reicht seine Aktivierung an eine bereits laufende Instanz weiter und
    // beendet sich sofort — deshalb bekommt der Testmodus eine eigene ID.
    // Erlaubt eine zweite Instanz neben einer laufenden — zum Nachstellen von
    // Problemen mit echten Daten, ohne die produktive App zu beenden.
    let dev_instance = std::env::var_os("ICLOUD_PHOTOS_DEV_INSTANCE").is_some();
    let app = if sync_task::fake_count().is_some() || dev_instance {
        adw::Application::builder()
            .application_id(format!("{APP_ID}.Fake"))
            .flags(gtk::gio::ApplicationFlags::NON_UNIQUE)
            .build()
    } else {
        adw::Application::builder().application_id(APP_ID).build()
    };
    app.connect_activate({
        let state = state.clone();
        move |app| {
            window::build(app, state.clone());
            schedule_housekeeping(state.clone());
        }
    });

    // GTK soll `--demo` nicht als eigenes Argument deuten.
    app.run_with_args::<&str>(&[]);
    Ok(())
}

/// Wählt den GL-Renderer, sofern der Nutzer nichts anderes vorgibt.
///
/// iPhones filmen in 4K-HDR, der Hardware-Decoder liefert dafür P010-DMABufs.
/// Der Vulkan-Renderer (Vorgabe seit GTK 4.16) kann die hier nicht direkt
/// importieren und nimmt einen Umweg, der an einem Tiger-Lake-Rechner nur
/// ~6 statt 30 Bilder pro Sekunde auf den Schirm bringt. Der GL-Renderer
/// importiert sie ohne Kopie. 1080p-Videos (NV12) laufen mit beiden flüssig.
fn prefer_gl_renderer() {
    if std::env::var_os("GSK_RENDERER").is_none() {
        // SAFETY: erster Schritt in `main`, es läuft noch kein zweiter Thread.
        unsafe { std::env::set_var("GSK_RENDERER", "gl") };
    }
}

fn schedule_housekeeping(state: Rc<window::AppState>) {
    gtk::glib::timeout_add_local(HOUSEKEEPING_INTERVAL, move || {
        let catalog = state.catalog.borrow();
        let full_res_dir = state.config.borrow().paths.full_res_dir.clone();
        match housekeeping::sweep(&catalog, &full_res_dir) {
            Ok(report) if !report.is_clean() => tracing::warn!(
                orphans = report.orphan_files_removed,
                stale = report.stale_rows_cleared,
                "Housekeeping hat aufgeräumt"
            ),
            Ok(_) => tracing::debug!("Housekeeping: nichts zu tun"),
            Err(error) => tracing::error!(%error, "Housekeeping fehlgeschlagen"),
        }

        // Das Limit auch ohne frischen Download durchsetzen: eine gesenkte
        // Konfiguration oder ein Absturz mitten in der Eviction soll nicht bis
        // zum naechsten Full-Res-Download unbemerkt bleiben.
        let cache_config = state.config.borrow().cache.clone();
        match enforce_limit(&catalog, &cache_config) {
            Ok(report) if !report.is_noop() => tracing::info!(
                evicted = report.evicted,
                freed_bytes = report.freed_bytes,
                "Full-Res-Cache heruntergeraeumt"
            ),
            Ok(_) => {}
            Err(error) => tracing::error!(%error, "Eviction fehlgeschlagen"),
        }

        gtk::glib::ControlFlow::Continue
    });
}
