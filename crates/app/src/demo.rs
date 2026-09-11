//! `--demo`: füllt den Katalog mit erzeugten Bildern.
//!
//! Damit lässt sich die gesamte UI — Grid, Viewer, LRU-Zugriffe, Eviction —
//! ohne Apple-Konto und ohne Netz ausprobieren.

use std::path::Path;

use chrono::{TimeDelta, Utc};
use gtk::gdk_pixbuf::{Colorspace, Pixbuf};
use icloud_photos_cache::Catalog;
use icloud_photos_core::Config;
use icloud_photos_core::model::PhotoRecord;

const DEMO_PHOTOS: usize = 240;
const THUMB_PX: i32 = 256;
const ORIGINAL_PX: i32 = 1400;
/// So viele Demo-Fotos bekommen auch ein „Original" im Full-Res-Cache.
const ORIGINALS: usize = 12;

pub fn seed(catalog: &Catalog, config: &Config) -> anyhow::Result<()> {
    if catalog.count()? > 0 {
        tracing::info!("Katalog ist nicht leer, Demo-Daten werden übersprungen");
        return Ok(());
    }
    tracing::info!("erzeuge {DEMO_PHOTOS} Demo-Fotos");

    let base = Utc::now() - TimeDelta::days(DEMO_PHOTOS as i64);

    for index in 0..DEMO_PHOTOS {
        let id = format!("demo-{index:04}");
        let added = base + TimeDelta::days(index as i64);

        catalog.upsert_record(&PhotoRecord {
            id: id.clone(),
            master_id: None,
            filename: format!("IMG_{index:04}.png"),
            added_date: added,
            asset_date: Some(added),
            checksum: None,
            width: Some(ORIGINAL_PX as u32),
            height: Some(ORIGINAL_PX as u32),
        })?;

        let thumb = config.paths.thumbnail_dir.join(format!("{id}.png"));
        let bytes = write_swatch(&thumb, THUMB_PX, index)?;
        catalog.set_thumbnail(&id, &thumb, bytes)?;

        if index < ORIGINALS {
            let original = config.paths.full_res_dir.join(format!("{id}.png"));
            let bytes = write_swatch(&original, ORIGINAL_PX, index)?;
            // Gestaffelte Zugriffszeiten, damit die LRU-Reihenfolge sichtbar ist.
            catalog.set_original_cached(&id, &original, bytes, added)?;
        }
    }

    tracing::info!(total = catalog.count()?, "Demo-Daten angelegt");
    Ok(())
}

/// Schreibt ein einfarbiges PNG und gibt seine Größe in Bytes zurück.
fn write_swatch(path: &Path, size: i32, index: usize) -> anyhow::Result<u64> {
    let pixbuf = Pixbuf::new(Colorspace::Rgb, false, 8, size, size)
        .ok_or_else(|| anyhow::anyhow!("Pixbuf konnte nicht angelegt werden"))?;
    pixbuf.fill(color_for(index));
    pixbuf.savev(path, "png", &[])?;
    Ok(std::fs::metadata(path)?.len())
}

/// Gut unterscheidbare Farben über den Farbkreis verteilt.
fn color_for(index: usize) -> u32 {
    let hue = (index as f64 * 137.508) % 360.0;
    let (r, g, b) = hsv_to_rgb(hue, 0.55, 0.85);
    u32::from_be_bytes([r, g, b, 0xff])
}

fn hsv_to_rgb(hue: f64, saturation: f64, value: f64) -> (u8, u8, u8) {
    let sector = hue / 60.0;
    let chroma = value * saturation;
    let secondary = chroma * (1.0 - (sector % 2.0 - 1.0).abs());
    let offset = value - chroma;

    let (r, g, b) = match sector as u32 {
        0 => (chroma, secondary, 0.0),
        1 => (secondary, chroma, 0.0),
        2 => (0.0, chroma, secondary),
        3 => (0.0, secondary, chroma),
        4 => (secondary, 0.0, chroma),
        _ => (chroma, 0.0, secondary),
    };
    let to_byte = |v: f64| (((v + offset) * 255.0).round().clamp(0.0, 255.0)) as u8;
    (to_byte(r), to_byte(g), to_byte(b))
}
