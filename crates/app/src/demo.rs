//! `--demo`: füllt den Katalog mit erzeugten Bildern.
//!
//! Damit lässt sich die gesamte UI — Home, Raster, Alben, Viewer, LRU-Zugriffe,
//! Eviction — ohne Apple-Konto und ohne Netz ausprobieren. Die Bibliothek ist
//! einer echten nachgebaut: Tage mit mehreren Fotos an einem Ort, eine Reise,
//! derselbe Tag in früheren Jahren („On This Day"), Videos, Favoriten, eigene
//! Alben samt einem Ordner und ein paar Bildschirmfotos. Die Bilder sind
//! ruhige Landschaften aus Verläufen (Himmel, Sonne, Hügel oder Meer) statt
//! einfarbiger Flächen — sonst ließe sich an einem Screenshot nicht ablesen,
//! ob eine Kachel richtig beschnitten ist.
//!
//! Alle Orte bekommen gleich einen Namen im Katalog. So schlägt der Viewer
//! nichts bei Nominatim nach: eine Demo schickt nichts ins Netz.

use std::path::Path;

use chrono::{Local, NaiveTime, TimeDelta, TimeZone, Utc};
use gtk::gdk_pixbuf::{Colorspace, Pixbuf};
use icloud_photos_cache::{AlbumContent, Catalog, place_cell};
use icloud_photos_core::Config;
use icloud_photos_core::model::{AlbumKind, AlbumRecord, PhotoRecord};

/// Längere Kante einer Vorschau — so groß wie die echten von iCloud.
const THUMB_EDGE: i32 = 384;
/// Längere Kante eines „Originals".
const ORIGINAL_EDGE: i32 = 1600;
/// So viele Demo-Fotos bekommen auch ein „Original" im Full-Res-Cache.
const ORIGINALS: usize = 12;

/// Orte der Demo: Name wie ihn der Geocoder speichern würde, und Koordinate.
const PLACES: [(&str, f64, f64); 9] = [
    ("Markt, Leipzig", 51.3397, 12.3731),
    ("Clara-Zetkin-Park, Leipzig", 51.3299, 12.3529),
    ("Altstadt, Dresden", 51.0504, 13.7373),
    ("Belém, Lisbon", 38.6916, -9.2160),
    ("Alfama, Lisbon", 38.7118, -9.1300),
    ("Bastei, Saxon Switzerland", 50.9620, 14.0734),
    ("Mitte, Berlin", 52.5200, 13.4050),
    ("Binz, Rügen", 54.4000, 13.6100),
    ("Ribeira, Porto", 41.1406, -8.6110),
];

/// Ein Tag der Demo: wie viele Tage vor heute, Ort (Index in `PLACES`, `None`
/// ohne Ort), Fotos, davon Videos.
const DAYS: [(i64, Option<usize>, usize, usize); 18] = [
    (0, Some(0), 6, 0),
    (2, Some(1), 9, 1),
    (4, Some(2), 10, 1),
    (11, Some(0), 5, 0),
    (17, Some(4), 14, 2),
    (18, Some(4), 11, 1),
    (19, Some(3), 10, 0),
    (38, Some(5), 12, 1),
    (53, Some(6), 7, 0),
    (76, Some(7), 13, 2),
    (77, Some(7), 9, 0),
    (102, Some(8), 10, 1),
    (149, None, 6, 0),
    (309, Some(0), 8, 1),
    (365, Some(0), 7, 0),
    (452, Some(6), 8, 0),
    (731, Some(8), 9, 1),
    (905, Some(6), 6, 0),
];

/// Seitenverhältnisse (Breite / Höhe) im Wechsel, wie aus einem iPhone.
const ASPECTS: [(u32, u32); 6] = [(4, 3), (3, 4), (3, 2), (4, 3), (16, 9), (1, 1)];

/// Ein Demo-Foto, bevor es in den Katalog geht.
struct Shot {
    id: String,
    day: usize,
    video: bool,
    screenshot: bool,
}

/// Ob die App als Demo läuft (`--demo`). Dann synchronisiert sie nicht von
/// selbst: die Demo-Bibliothek gehört zu keinem Konto.
pub fn active() -> bool {
    std::env::args().any(|arg| arg == "--demo")
}

pub fn seed(catalog: &mut Catalog, config: &Config) -> anyhow::Result<()> {
    if catalog.count()? > 0 {
        tracing::info!("Katalog ist nicht leer, Demo-Daten werden übersprungen");
        return Ok(());
    }

    let today = Local::now().date_naive();
    let mut shots = Vec::new();
    let mut index = 0usize;
    for (day_index, &(ago, place, count, videos)) in DAYS.iter().enumerate() {
        let day = today - TimeDelta::days(ago);
        for k in 0..count {
            let id = format!("demo-{index:04}");
            let video = videos > 0 && (k * 7 + day_index) % count < videos;
            // Über den Tag verteilt, das jüngste zuerst in der Bibliothek.
            let minutes = 9 * 60 + 12 + (k as u32) * 41 + (day_index as u32) * 3;
            let time =
                NaiveTime::from_hms_opt((minutes / 60) % 24, minutes % 60, 0).unwrap_or_default();
            let local = Local
                .from_local_datetime(&day.and_time(time))
                .single()
                .unwrap_or_else(Local::now);
            // Heute nichts aus der Zukunft.
            let taken = local
                .with_timezone(&Utc)
                .min(Utc::now() - TimeDelta::minutes(k as i64));
            let (aw, ah) = ASPECTS[(index * 5 + 1) % ASPECTS.len()];
            let (width, height) = scale_to(ORIGINAL_EDGE as u32, aw, ah);
            let coordinates = place.map(|p| jitter(PLACES[p].1, PLACES[p].2, index));
            let extension = if video { "MOV" } else { "HEIC" };
            catalog.upsert_record(&PhotoRecord {
                id: id.clone(),
                master_id: None,
                filename: format!("IMG_{:04}.{extension}", 4800 + index),
                added_date: taken + TimeDelta::hours(2),
                asset_date: Some(taken),
                checksum: None,
                width: Some(width),
                height: Some(height),
                lat: coordinates.map(|c| c.0),
                lon: coordinates.map(|c| c.1),
                item_type: Some(if video { "movie" } else { "image" }.into()),
            })?;
            write_photo(catalog, config, &id, index, aw, ah, index < ORIGINALS)?;
            shots.push(Shot {
                id,
                day: day_index,
                video,
                screenshot: false,
            });
            index += 1;
        }
    }

    // Drei Bildschirmfotos: hochkant, ohne Ort, als PNG.
    for k in 0..3 {
        let id = format!("demo-shot-{k}");
        let taken = Utc::now() - TimeDelta::days(30 + k as i64 * 12);
        catalog.upsert_record(&PhotoRecord {
            id: id.clone(),
            master_id: None,
            filename: format!("Screenshot {}.png", taken.format("%Y-%m-%d")),
            added_date: taken,
            asset_date: Some(taken),
            checksum: None,
            width: Some(1179),
            height: Some(2556),
            lat: None,
            lon: None,
            item_type: Some("image".into()),
        })?;
        write_photo(catalog, config, &id, 900 + k, 9, 19, false)?;
        shots.push(Shot {
            id,
            day: usize::MAX,
            video: false,
            screenshot: true,
        });
    }

    // Jeder Ort hat schon einen Namen — der Viewer fragt dann nicht nach.
    for (day_index, &(_, place, ..)) in DAYS.iter().enumerate() {
        let Some(place) = place else { continue };
        for (index, shot) in shots.iter().enumerate() {
            if shot.day == day_index {
                let (lat, lon) = jitter(PLACES[place].1, PLACES[place].2, index);
                catalog.remember_place(&place_cell(lat, lon), Some(PLACES[place].0), Utc::now())?;
            }
        }
    }

    seed_albums(catalog, &shots)?;
    catalog.set_last_sync(Utc::now() - TimeDelta::minutes(4))?;
    tracing::info!(total = catalog.count()?, "Demo-Daten angelegt");
    Ok(())
}

/// Alben wie in einer echten Bibliothek: Favorites, Videos und Screenshots
/// als intelligente, dazu eigene Alben und ein Ordner mit drei Reisen.
fn seed_albums(catalog: &mut Catalog, shots: &[Shot]) -> anyhow::Result<()> {
    let ids = |pick: &dyn Fn(usize, &Shot) -> bool| -> Vec<String> {
        shots
            .iter()
            .enumerate()
            .filter(|(i, s)| pick(*i, s))
            .map(|(_, s)| s.id.clone())
            .collect()
    };
    let on_days = |days: &'static [usize]| move |_: usize, s: &Shot| days.contains(&s.day);
    let album =
        |id: &str, name: &str, kind: AlbumKind, parent: Option<&str>, position: u64| AlbumRecord {
            id: id.into(),
            name: name.into(),
            kind,
            parent: parent.map(str::to_string),
            position,
        };
    let albums = vec![
        album("Favorites", "Favorites", AlbumKind::Smart, None, 0),
        album("Videos", "Videos", AlbumKind::Smart, None, 1),
        album("Screenshots", "Screenshots", AlbumKind::Smart, None, 2),
        album("Live", "Live", AlbumKind::Smart, None, 3),
        album("demo-lisbon", "Lisbon 2026", AlbumKind::Album, None, 10),
        album("demo-ruegen", "Rügen Summer", AlbumKind::Album, None, 11),
        album("demo-family", "Family", AlbumKind::Album, None, 12),
        album("demo-uni", "Uni", AlbumKind::Album, None, 13),
        album("demo-trips", "Trips", AlbumKind::Folder, None, 14),
        album(
            "demo-porto",
            "Porto",
            AlbumKind::Album,
            Some("demo-trips"),
            15,
        ),
        album(
            "demo-dresden",
            "Dresden",
            AlbumKind::Album,
            Some("demo-trips"),
            16,
        ),
        album(
            "demo-saxon",
            "Saxon Switzerland",
            AlbumKind::Album,
            Some("demo-trips"),
            17,
        ),
    ];
    let content = |id: &str, members: Vec<String>| AlbumContent {
        id: id.into(),
        size: Some(members.len() as u64),
        members,
    };
    let contents = vec![
        content(
            "Favorites",
            ids(&|i, s: &Shot| !s.screenshot && (i * 7919) % 100 < 15),
        ),
        content("Videos", ids(&|_, s: &Shot| s.video)),
        content("Screenshots", ids(&|_, s: &Shot| s.screenshot)),
        content(
            "Live",
            ids(&|i, s: &Shot| !s.video && !s.screenshot && i % 3 == 0),
        ),
        content("demo-lisbon", ids(&on_days(&[4, 5, 6]))),
        content("demo-ruegen", ids(&on_days(&[9, 10]))),
        content(
            "demo-family",
            ids(&|i, s: &Shot| !s.screenshot && i % 4 == 1),
        ),
        content("demo-uni", ids(&on_days(&[0, 3, 13]))),
        content("demo-porto", ids(&on_days(&[11, 16]))),
        content("demo-dresden", ids(&on_days(&[2]))),
        content("demo-saxon", ids(&on_days(&[7]))),
    ];
    catalog.replace_albums(&albums, &contents)?;
    Ok(())
}

/// Breite und Höhe für eine längere Kante und ein Seitenverhältnis.
fn scale_to(edge: u32, aw: u32, ah: u32) -> (u32, u32) {
    if aw >= ah {
        (edge, edge * ah / aw)
    } else {
        (edge * aw / ah, edge)
    }
}

/// Ein Ort, leicht gestreut — sonst stünden alle Fotos eines Tages auf
/// einem Punkt.
fn jitter(lat: f64, lon: f64, index: usize) -> (f64, f64) {
    let j = |salt: usize| ((index * 7919 + salt) % 100) as f64 / 100.0 - 0.5;
    (lat + j(13) * 0.01, lon + j(71) * 0.01)
}

fn write_photo(
    catalog: &Catalog,
    config: &Config,
    id: &str,
    seed: usize,
    aw: u32,
    ah: u32,
    original: bool,
) -> anyhow::Result<()> {
    let (tw, th) = scale_to(THUMB_EDGE as u32, aw, ah);
    let thumb = config.paths.thumbnail_dir.join(format!("{id}.jpg"));
    let bytes = write_scene(&thumb, tw as i32, th as i32, seed)?;
    catalog.set_thumbnail(id, &thumb, bytes)?;
    if original {
        let (ow, oh) = scale_to(ORIGINAL_EDGE as u32, aw, ah);
        let path = config.paths.full_res_dir.join(format!("{id}.jpg"));
        let bytes = write_scene(&path, ow as i32, oh as i32, seed)?;
        // Gestaffelte Zugriffszeiten, damit die LRU-Reihenfolge sichtbar ist.
        let when = Utc::now() - TimeDelta::hours(seed as i64);
        catalog.set_original_cached(id, &path, bytes, when)?;
    }
    Ok(())
}

/// Paletten: Himmel oben, Himmel unten, Sonne, Hügel/Meer vorn, dahinter.
const PALETTES: [[u32; 5]; 6] = [
    [0xf6d6c8, 0xe9a7a0, 0xfff4e6, 0x8e4f6a, 0xb86a7c],
    [0xbfdcef, 0xe8f1f2, 0xffffff, 0x2f5d50, 0x6f9c7f],
    [0xf7c58f, 0xf5e0b8, 0xfff2d6, 0x5b6b3a, 0x93a75a],
    [0x2a2d4e, 0x6b4a72, 0xffd59a, 0x1d2238, 0x3a3f66],
    [0xcfe6e4, 0xf1efe6, 0xffffff, 0x3f7f86, 0x8cc1b8],
    [0xe7dccb, 0xf3ece1, 0xfffaf0, 0x7a5c44, 0xb69a7c],
];

/// Eine ruhige Landschaft als JPEG: Himmel als Verlauf, Sonne mit Hof, zwei
/// Hügel- oder Wellenlinien. Gibt die Dateigröße zurück.
fn write_scene(path: &Path, width: i32, height: i32, seed: usize) -> anyhow::Result<u64> {
    let pixbuf = Pixbuf::new(Colorspace::Rgb, false, 8, width, height)
        .ok_or_else(|| anyhow::anyhow!("Could not create a pixbuf"))?;
    let palette = PALETTES[seed % PALETTES.len()];
    let rgb = |c: u32| {
        [
            (c >> 16) as f64,
            ((c >> 8) & 0xff) as f64,
            (c & 0xff) as f64,
        ]
    };
    let mix = |a: [f64; 3], b: [f64; 3], t: f64| {
        [
            a[0] + (b[0] - a[0]) * t,
            a[1] + (b[1] - a[1]) * t,
            a[2] + (b[2] - a[2]) * t,
        ]
    };
    let (top, bottom, sun, near, far) = (
        rgb(palette[0]),
        rgb(palette[1]),
        rgb(palette[2]),
        rgb(palette[3]),
        rgb(palette[4]),
    );
    let r = |salt: usize| ((seed * 2654435761 + salt * 40503) % 1000) as f64 / 1000.0;
    let (sun_x, sun_y, sun_r) = (0.2 + r(1) * 0.6, 0.18 + r(2) * 0.2, 0.05 + r(3) * 0.04);
    let (far_y, far_amp, far_phase) = (
        0.55 + r(4) * 0.1,
        0.04 + r(5) * 0.05,
        r(6) * std::f64::consts::TAU,
    );
    let (near_y, near_amp, near_phase) = (
        0.7 + r(7) * 0.1,
        0.05 + r(8) * 0.06,
        r(9) * std::f64::consts::TAU,
    );
    let rowstride = pixbuf.rowstride() as usize;
    let channels = pixbuf.n_channels() as usize;
    // SAFETY: kein anderer Zugriff auf den frisch erzeugten Puffer.
    let pixels = unsafe { pixbuf.pixels() };
    let aspect = width as f64 / height as f64;
    for y in 0..height {
        let fy = y as f64 / height as f64;
        let sky = mix(top, bottom, fy);
        for x in 0..width {
            let fx = x as f64 / width as f64;
            let dx = (fx - sun_x) * aspect;
            let dy = fy - sun_y;
            let d = (dx * dx + dy * dy).sqrt();
            let mut c = if d < sun_r {
                sun
            } else {
                // Hof um die Sonne.
                let glow = (1.0 - (d - sun_r) / (sun_r * 4.0)).clamp(0.0, 1.0);
                mix(sky, sun, glow * glow * 0.55)
            };
            let far_line = far_y + far_amp * (fx * 4.0 + far_phase).sin();
            let near_line = near_y + near_amp * (fx * 2.3 + near_phase).sin();
            if fy > far_line {
                c = mix(far, sky, ((far_line + 0.25 - fy) * 0.8).clamp(0.0, 0.25));
            }
            if fy > near_line {
                c = mix(near, far, ((1.0 - fy) * 0.5).clamp(0.0, 0.3));
            }
            let at = y as usize * rowstride + x as usize * channels;
            pixels[at] = c[0].round().clamp(0.0, 255.0) as u8;
            pixels[at + 1] = c[1].round().clamp(0.0, 255.0) as u8;
            pixels[at + 2] = c[2].round().clamp(0.0, 255.0) as u8;
        }
    }
    pixbuf.savev(path, "jpeg", &[("quality", "88")])?;
    Ok(std::fs::metadata(path)?.len())
}
