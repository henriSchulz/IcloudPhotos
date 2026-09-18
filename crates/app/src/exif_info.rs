//! Aufnahmedaten für das Info-Panel des Betrachters — ohne GTK.
//!
//! EXIF steht nur im Original: iCloud liefert mit den Metadaten weder Kamera
//! noch Belichtung, und die Vorschau (`resJPEGThumb`) kommt ohne EXIF. Gelesen
//! wird deshalb die Datei im Full-Res-Cache, über `kamadak-exif` — reines Rust,
//! und es kennt neben JPEG auch den HEIF-Container, in dem drei von vier
//! iPhone-Fotos liegen.
//!
//! Alles hier liefert fertig formatierte Texte, damit die Oberfläche nur noch
//! Zeilen ein- und ausblendet.

use std::fs::File;
use std::io::BufReader;
use std::path::Path;

use exif::{Exif, In, Reader, Tag, Value};

/// Die Aufnahmedaten eines Fotos, soweit die Kamera sie hinterlegt hat.
///
/// Jedes Feld ist für sich optional: ein Scan hat keine Belichtung, ein
/// Screenshot gar nichts. Die Oberfläche blendet fehlende Zeilen aus.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExifInfo {
    /// Hersteller und Modell, z.B. „Apple iPhone 15 Pro".
    pub camera: Option<String>,
    pub lens: Option<String>,
    /// Brennweite, z.B. „6.8 mm (24 mm equiv.)".
    pub focal_length: Option<String>,
    /// Blende, z.B. „ƒ/1.8".
    pub aperture: Option<String>,
    /// Belichtungszeit, z.B. „1/120 s".
    pub shutter: Option<String>,
    /// ISO-Wert ohne Einheit, z.B. „100" — die Zeile heißt schon „ISO".
    pub iso: Option<String>,
    /// „Fired", wenn der Blitz ausgelöst hat. Ein nicht ausgelöster Blitz ist
    /// der Normalfall und keine Zeile wert.
    pub flash: Option<String>,
}

/// Liest die Aufnahmedaten aus einer Bilddatei.
///
/// Blockiert — gehört in einen Hilfsthread. Eine Datei ganz ohne EXIF ist ein
/// `Err(NotFound)`, kein Sonderfall für den Aufrufer: in beiden Fällen gibt es
/// nichts zu zeigen.
pub fn read(path: &Path) -> Result<ExifInfo, exif::Error> {
    let file = File::open(path)?;
    let mut reader = Reader::new();
    // Ein einzelnes kaputtes Feld soll nicht alle anderen mitreißen. Hersteller
    // schreiben gern Eigenwilliges in Felder, die hier niemand braucht.
    reader.continue_on_error(true);
    let exif = reader
        .read_from_container(&mut BufReader::new(file))
        .or_else(|error| {
            error.distill_partial_result(|errors| {
                for error in errors {
                    tracing::debug!(path = %path.display(), %error, "EXIF-Feld übersprungen");
                }
            })
        })?;
    Ok(from_exif(&exif))
}

/// Macht aus den rohen Feldern die Texte fürs Panel.
pub fn from_exif(exif: &Exif) -> ExifInfo {
    let focal = rational(exif, Tag::FocalLength);
    let equivalent = uint(exif, Tag::FocalLengthIn35mmFilm);
    // `PhotographicSensitivity` ist das Feld, das Kameras tatsächlich
    // schreiben; `ISOSpeed` gibt es nur zusammen mit einem SensitivityType
    // und kommt in der Praxis kaum vor.
    let iso = uint(exif, Tag::PhotographicSensitivity)
        .or_else(|| uint(exif, Tag::ISOSpeed))
        .filter(|&iso| iso > 0);
    ExifInfo {
        camera: camera_name(
            ascii(exif, Tag::Make).as_deref(),
            ascii(exif, Tag::Model).as_deref(),
        ),
        lens: ascii(exif, Tag::LensModel),
        focal_length: format_focal_length(focal, equivalent),
        aperture: rational(exif, Tag::FNumber).and_then(format_aperture),
        shutter: rational(exif, Tag::ExposureTime).and_then(format_shutter),
        iso: iso.map(|iso| iso.to_string()),
        flash: uint(exif, Tag::Flash)
            .filter(|&flash| flash_fired(flash))
            .map(|_| "Fired".to_string()),
    }
}

fn field(exif: &Exif, tag: Tag) -> Option<&Value> {
    exif.get_field(tag, In::PRIMARY).map(|field| &field.value)
}

/// Ein Textfeld, ohne Nullbytes und Leerraum drumherum. Leere Felder zählen
/// als fehlend — manche Kameras schreiben lieber "" als gar nichts.
fn ascii(exif: &Exif, tag: Tag) -> Option<String> {
    let Value::Ascii(parts) = field(exif, tag)? else {
        return None;
    };
    parts
        .iter()
        .map(|part| {
            String::from_utf8_lossy(part)
                .trim_matches(|c: char| c == '\0' || c.is_whitespace())
                .to_string()
        })
        .find(|text| !text.is_empty())
}

fn rational(exif: &Exif, tag: Tag) -> Option<f64> {
    let value = match field(exif, tag)? {
        Value::Rational(values) => values.first().filter(|r| r.denom != 0).map(|r| r.to_f64()),
        Value::SRational(values) => values.first().filter(|r| r.denom != 0).map(|r| r.to_f64()),
        other => other.get_uint(0).map(f64::from),
    }?;
    value.is_finite().then_some(value)
}

fn uint(exif: &Exif, tag: Tag) -> Option<u32> {
    field(exif, tag)?.get_uint(0)
}

/// Hersteller und Modell zusammen, ohne den Hersteller doppelt zu nennen.
///
/// Canon schreibt „Canon" als Hersteller und „Canon EOS R5" als Modell,
/// Apple dagegen „Apple" und „iPhone 15 Pro". Beides soll als ein Name
/// erscheinen, nicht als „Canon Canon EOS R5".
pub fn camera_name(make: Option<&str>, model: Option<&str>) -> Option<String> {
    let make = make.map(str::trim).filter(|s| !s.is_empty());
    let model = model.map(str::trim).filter(|s| !s.is_empty());
    match (make, model) {
        (Some(make), Some(model)) => {
            if model.to_lowercase().starts_with(&make.to_lowercase()) {
                Some(model.to_string())
            } else {
                Some(format!("{make} {model}"))
            }
        }
        (Some(only), None) | (None, Some(only)) => Some(only.to_string()),
        (None, None) => None,
    }
}

/// Bit 0 des Flash-Felds: ausgelöst oder nicht. Die übrigen Bits sagen, ob
/// überhaupt ein Blitz da war und in welchem Modus — für die Anzeige egal.
pub fn flash_fired(flash: u32) -> bool {
    flash & 1 == 1
}

/// Eine Zahl mit höchstens `places` Nachkommastellen, ohne Nullen am Ende:
/// „50" statt „50.0", aber „6.8".
pub fn decimal(value: f64, places: usize) -> String {
    let text = format!("{value:.places$}");
    if text.contains('.') {
        text.trim_end_matches('0').trim_end_matches('.').to_string()
    } else {
        text
    }
}

/// Belichtungszeit wie auf der Kamera: Bruchteile als „1/120 s", lange
/// Zeiten als „2.5 s".
///
/// Kameras schreiben die Zeit nicht immer als glatten Bruch — 1/120 kommt
/// auch als 8333/1000000 an. Liegt der Kehrwert nah an einer ganzen Zahl, wird
/// er als Bruch gezeigt. Sonst, etwa bei 0.4 s (Kehrwert 2.5), wäre „1/3 s"
/// schlicht falsch, deshalb dann die Dezimalzahl.
pub fn format_shutter(seconds: f64) -> Option<String> {
    if !seconds.is_finite() || seconds <= 0.0 {
        return None;
    }
    if seconds >= 1.0 {
        return Some(format!("{} s", decimal(seconds, 1)));
    }
    let denominator = 1.0 / seconds;
    let rounded = denominator.round();
    if rounded >= 1.0 && (denominator - rounded).abs() / rounded <= 0.05 {
        Some(format!("1/{rounded:.0} s"))
    } else {
        Some(format!("{} s", decimal(seconds, 2)))
    }
}

/// Blende als „ƒ/1.8" — eine Nachkommastelle, wie sie auf Objektiven steht.
/// Das iPhone schreibt 1.78; auf 1.8 gerundet ist das die übliche Angabe.
pub fn format_aperture(f_number: f64) -> Option<String> {
    (f_number.is_finite() && f_number > 0.0).then(|| format!("ƒ/{}", decimal(f_number, 1)))
}

/// Brennweite, dazu das Kleinbild-Äquivalent, sofern die Kamera es nennt.
///
/// Beim iPhone sagt die echte Brennweite (6.8 mm) wenig; erst das
/// Äquivalent (24 mm) ordnet sie ein. Stimmen beide überein — Vollformat —,
/// fällt das Äquivalent weg.
pub fn format_focal_length(millimetres: Option<f64>, equivalent: Option<u32>) -> Option<String> {
    let millimetres = millimetres.filter(|mm| mm.is_finite() && *mm > 0.0);
    let equivalent = equivalent.filter(|&mm| mm > 0);
    match (millimetres, equivalent) {
        (Some(mm), Some(eq)) if (mm - f64::from(eq)).abs() >= 0.5 => {
            Some(format!("{} mm ({eq} mm equiv.)", decimal(mm, 1)))
        }
        (Some(mm), _) => Some(format!("{} mm", decimal(mm, 1))),
        (None, Some(eq)) => Some(format!("{eq} mm equiv.")),
        (None, None) => None,
    }
}

/// Pixelmaße, z.B. „4032 × 3024".
pub fn format_dimensions(width: u32, height: u32) -> Option<String> {
    (width > 0 && height > 0).then(|| format!("{width} × {height}"))
}

/// Auflösung in Megapixeln, z.B. „12.2 MP". Eine Stelle genügt — mehr
/// Genauigkeit liest niemand aus dieser Zahl.
pub fn format_megapixels(width: u32, height: u32) -> Option<String> {
    let pixels = u64::from(width) * u64::from(height);
    (pixels > 0).then(|| format!("{} MP", decimal(pixels as f64 / 1_000_000.0, 1)))
}

/// Dateigröße in Dezimaleinheiten, wie Dateimanager sie zeigen.
///
/// Dieselbe Rechnung wie `human_bytes` im Hauptfenster; die liegt dort
/// privat, und das Panel soll ohne Umbau am Fenster auskommen.
pub fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1000.0 && unit < UNITS.len() - 1 {
        value /= 1000.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// Koordinaten mit Himmelsrichtung, z.B. „48.1372° N, 11.5756° E". Vier
/// Stellen sind gut zehn Meter — genauer ist ein Handy-GPS ohnehin nicht.
pub fn format_coordinates(lat: f64, lon: f64) -> String {
    let north_south = if lat < 0.0 { 'S' } else { 'N' };
    let east_west = if lon < 0.0 { 'W' } else { 'E' };
    format!(
        "{:.4}° {north_south}, {:.4}° {east_west}",
        lat.abs(),
        lon.abs()
    )
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use exif::experimental::Writer;
    use exif::{Field, Rational};

    use super::*;

    #[test]
    fn shutter_speeds_read_like_on_the_camera() {
        assert_eq!(format_shutter(1.0 / 120.0).as_deref(), Some("1/120 s"));
        assert_eq!(format_shutter(1.0 / 8000.0).as_deref(), Some("1/8000 s"));
        // So schreiben es manche Kameras: kein glatter Bruch.
        assert_eq!(format_shutter(0.008333).as_deref(), Some("1/120 s"));
        assert_eq!(format_shutter(0.5).as_deref(), Some("1/2 s"));
        assert_eq!(format_shutter(0.25).as_deref(), Some("1/4 s"));
        // Kehrwert 2.5 — als Bruch wäre das gelogen.
        assert_eq!(format_shutter(0.4).as_deref(), Some("0.4 s"));
        assert_eq!(format_shutter(0.3).as_deref(), Some("0.3 s"));
        assert_eq!(format_shutter(1.0).as_deref(), Some("1 s"));
        assert_eq!(format_shutter(2.5).as_deref(), Some("2.5 s"));
        assert_eq!(format_shutter(30.0).as_deref(), Some("30 s"));
    }

    #[test]
    fn nonsense_shutter_speeds_are_dropped() {
        assert_eq!(format_shutter(0.0), None);
        assert_eq!(format_shutter(-1.0), None);
        assert_eq!(format_shutter(f64::NAN), None);
        assert_eq!(format_shutter(f64::INFINITY), None);
    }

    #[test]
    fn apertures_get_one_decimal() {
        assert_eq!(format_aperture(1.78).as_deref(), Some("ƒ/1.8"));
        assert_eq!(format_aperture(2.2).as_deref(), Some("ƒ/2.2"));
        assert_eq!(format_aperture(8.0).as_deref(), Some("ƒ/8"));
        assert_eq!(format_aperture(16.0).as_deref(), Some("ƒ/16"));
        assert_eq!(format_aperture(0.0), None);
        assert_eq!(format_aperture(f64::NAN), None);
    }

    #[test]
    fn focal_length_names_the_equivalent_when_it_differs() {
        assert_eq!(
            format_focal_length(Some(6.765), Some(24)).as_deref(),
            Some("6.8 mm (24 mm equiv.)")
        );
        // Vollformat: das Äquivalent sagt nichts Neues.
        assert_eq!(
            format_focal_length(Some(50.0), Some(50)).as_deref(),
            Some("50 mm")
        );
        assert_eq!(
            format_focal_length(Some(35.0), None).as_deref(),
            Some("35 mm")
        );
        assert_eq!(
            format_focal_length(None, Some(26)).as_deref(),
            Some("26 mm equiv.")
        );
        assert_eq!(format_focal_length(Some(0.0), Some(0)).as_deref(), None);
        assert_eq!(format_focal_length(None, None), None);
    }

    #[test]
    fn camera_name_does_not_repeat_the_make() {
        assert_eq!(
            camera_name(Some("Apple"), Some("iPhone 15 Pro")).as_deref(),
            Some("Apple iPhone 15 Pro")
        );
        assert_eq!(
            camera_name(Some("Canon"), Some("Canon EOS R5")).as_deref(),
            Some("Canon EOS R5")
        );
        assert_eq!(
            camera_name(Some("SONY"), Some("Sony ILCE-7M3")).as_deref(),
            Some("Sony ILCE-7M3")
        );
        assert_eq!(camera_name(None, Some("X100V")).as_deref(), Some("X100V"));
        assert_eq!(
            camera_name(Some("Apple"), Some("  ")).as_deref(),
            Some("Apple")
        );
        assert_eq!(camera_name(Some(""), None), None);
    }

    #[test]
    fn only_bit_zero_means_the_flash_fired() {
        assert!(flash_fired(0x01));
        assert!(flash_fired(0x19)); // Automatik, ausgelöst
        assert!(!flash_fired(0x00));
        assert!(!flash_fired(0x10)); // Aus
        assert!(!flash_fired(0x18)); // Automatik, nicht ausgelöst
        assert!(!flash_fired(0x20)); // kein Blitz vorhanden
    }

    #[test]
    fn sizes_and_places_are_readable() {
        assert_eq!(
            format_dimensions(4032, 3024).as_deref(),
            Some("4032 × 3024")
        );
        assert_eq!(format_dimensions(0, 3024), None);
        assert_eq!(format_megapixels(4032, 3024).as_deref(), Some("12.2 MP"));
        assert_eq!(format_megapixels(4000, 3000).as_deref(), Some("12 MP"));
        assert_eq!(format_megapixels(8064, 6048).as_deref(), Some("48.8 MP"));
        assert_eq!(format_megapixels(0, 0), None);
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(2_345_678), "2.3 MB");
        assert_eq!(format_bytes(1_000), "1.0 KB");
        assert_eq!(
            format_coordinates(48.137154, 11.576124),
            "48.1372° N, 11.5761° E"
        );
        assert_eq!(
            format_coordinates(-33.8688, -151.2093),
            "33.8688° S, 151.2093° W"
        );
    }

    #[test]
    fn decimals_lose_trailing_zeros() {
        assert_eq!(decimal(50.0, 1), "50");
        assert_eq!(decimal(6.765, 1), "6.8");
        assert_eq!(decimal(0.40, 2), "0.4");
        assert_eq!(decimal(100.0, 0), "100");
    }

    /// Die Felder, die ein iPhone 15 Pro typischerweise schreibt.
    fn iphone_fields() -> Vec<Field> {
        let ascii = |tag, text: &str| Field {
            tag,
            ifd_num: In::PRIMARY,
            value: Value::Ascii(vec![text.as_bytes().to_vec()]),
        };
        let rational = |tag, num, denom| Field {
            tag,
            ifd_num: In::PRIMARY,
            value: Value::Rational(vec![Rational { num, denom }]),
        };
        let short = |tag, value| Field {
            tag,
            ifd_num: In::PRIMARY,
            value: Value::Short(vec![value]),
        };
        vec![
            ascii(Tag::Make, "Apple"),
            ascii(Tag::Model, "iPhone 15 Pro"),
            ascii(
                Tag::LensModel,
                "iPhone 15 Pro back triple camera 6.765mm f/1.78",
            ),
            rational(Tag::FocalLength, 6765, 1000),
            short(Tag::FocalLengthIn35mmFilm, 24),
            rational(Tag::FNumber, 178, 100),
            rational(Tag::ExposureTime, 1, 120),
            short(Tag::PhotographicSensitivity, 64),
            short(Tag::Flash, 0x10),
        ]
    }

    /// Ein echtes JPEG mit EXIF-Block: das Fixture (16×16, ohne Metadaten,
    /// mit ImageMagick erzeugt) bekommt direkt hinter SOI ein APP1-Segment.
    fn jpeg_with_exif(fields: &[Field]) -> Vec<u8> {
        let mut writer = Writer::new();
        for field in fields {
            writer.push_field(field);
        }
        let mut tiff = Cursor::new(Vec::new());
        writer.write(&mut tiff, false).unwrap();
        let tiff = tiff.into_inner();

        let jpeg = include_bytes!("../fixtures/tiny.jpg");
        assert_eq!(&jpeg[..2], &[0xFF, 0xD8], "Fixture ist kein JPEG");
        let length = u16::try_from(2 + 6 + tiff.len()).unwrap();
        let mut out = vec![0xFF, 0xD8, 0xFF, 0xE1];
        out.extend_from_slice(&length.to_be_bytes());
        out.extend_from_slice(b"Exif\0\0");
        out.extend_from_slice(&tiff);
        out.extend_from_slice(&jpeg[2..]);
        out
    }

    fn expected_iphone() -> ExifInfo {
        ExifInfo {
            camera: Some("Apple iPhone 15 Pro".into()),
            lens: Some("iPhone 15 Pro back triple camera 6.765mm f/1.78".into()),
            focal_length: Some("6.8 mm (24 mm equiv.)".into()),
            aperture: Some("ƒ/1.8".into()),
            shutter: Some("1/120 s".into()),
            iso: Some("64".into()),
            // 0x10 heißt „Blitz aus" — keine Zeile.
            flash: None,
        }
    }

    #[test]
    fn reads_exif_from_a_jpeg_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("IMG_0001.JPG");
        std::fs::write(&path, jpeg_with_exif(&iphone_fields())).unwrap();

        assert_eq!(read(&path).unwrap(), expected_iphone());
    }

    #[test]
    fn a_fired_flash_gets_a_row() {
        let mut fields = iphone_fields();
        fields.retain(|field| field.tag != Tag::Flash);
        fields.push(Field {
            tag: Tag::Flash,
            ifd_num: In::PRIMARY,
            value: Value::Short(vec![0x19]),
        });
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("flash.jpg");
        std::fs::write(&path, jpeg_with_exif(&fields)).unwrap();

        assert_eq!(read(&path).unwrap().flash.as_deref(), Some("Fired"));
    }

    #[test]
    fn a_file_without_exif_is_an_error_not_a_panic() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("plain.jpg");
        std::fs::write(&path, include_bytes!("../fixtures/tiny.jpg")).unwrap();
        assert!(read(&path).is_err());

        let garbage = dir.path().join("garbage.heic");
        std::fs::write(&garbage, b"not an image").unwrap();
        assert!(read(&garbage).is_err());

        assert!(read(&dir.path().join("missing.jpg")).is_err());
    }

    /// HEIC ist der eigentliche Fall — drei von vier iPhone-Fotos. Gebaut
    /// wird die Datei mit `heif-enc` aus libheif, das die App ohnehin braucht
    /// und das den EXIF-Block des JPEGs in den HEIF-Container übernimmt. Fehlt
    /// das Werkzeug, wird der Test übersprungen statt rot.
    #[test]
    fn reads_exif_from_a_heic_file() {
        let dir = tempfile::tempdir().unwrap();
        let jpeg = dir.path().join("source.jpg");
        let heic = dir.path().join("IMG_0001.HEIC");
        std::fs::write(&jpeg, jpeg_with_exif(&iphone_fields())).unwrap();

        let status = std::process::Command::new("heif-enc")
            .arg("-o")
            .arg(&heic)
            .arg(&jpeg)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        match status {
            Ok(status) if status.success() => {}
            other => {
                eprintln!("heif-enc nicht verfügbar ({other:?}) — HEIC-Test übersprungen");
                return;
            }
        }

        assert_eq!(read(&heic).unwrap(), expected_iphone());
    }
}
