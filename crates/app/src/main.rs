//! iCloud Photos — nativer Client für Omarchy.

mod demo;
mod photo_object;
mod sync_task;
mod window;

use std::rc::Rc;

use adw::prelude::*;
use icloud_photos_cache::{Catalog, enforce_limit, housekeeping};
use icloud_photos_core::Config;

const APP_ID: &str = "de.henri.IcloudPhotos";

/// Sicherheitsnetz gegen verwaiste Cache-Dateien nach Abstürzen.
const HOUSEKEEPING_INTERVAL: std::time::Duration = std::time::Duration::from_secs(10 * 60);

fn main() -> anyhow::Result<()> {
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

    let state = window::AppState::new(config, catalog);

    let app = adw::Application::builder().application_id(APP_ID).build();
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

fn schedule_housekeeping(state: Rc<window::AppState>) {
    gtk::glib::timeout_add_local(HOUSEKEEPING_INTERVAL, move || {
        let catalog = state.catalog.borrow();
        match housekeeping::sweep(&catalog, &state.config.paths.full_res_dir) {
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
        match enforce_limit(&catalog, &state.config.cache) {
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
