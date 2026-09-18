//! Das Info-Panel des Betrachters — das ⓘ aus Photos.
//!
//! Zwei Quellen: der Katalog (Datum, Maße, Ort) steht sofort bereit, die
//! Aufnahmedaten der Kamera nur im Original (siehe `exif_info`). Solange das
//! Original fehlt, zeigt das Panel, was der Katalog weiß, und einen Hinweis
//! statt leerer Kamerazeilen.

use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use adw::prelude::*;
use gtk::{gio, glib};
use icloud_photos_core::model::Photo;

use crate::exif_info::{self, ExifInfo};

pub struct InfoPanel {
    /// Das Panel als Ganzes, für die Seitenleiste des Betrachters.
    pub widget: gtk::ScrolledWindow,
    title: gtk::Label,
    captured: gtk::Label,

    details: adw::PreferencesGroup,
    filename_row: adw::ActionRow,
    kind_row: adw::ActionRow,
    dimensions_row: adw::ActionRow,
    size_row: adw::ActionRow,

    camera: adw::PreferencesGroup,
    camera_row: adw::ActionRow,
    lens_row: adw::ActionRow,
    focal_row: adw::ActionRow,
    aperture_row: adw::ActionRow,
    shutter_row: adw::ActionRow,
    iso_row: adw::ActionRow,
    flash_row: adw::ActionRow,
    /// Hinweis, solange das Original — und damit EXIF — noch fehlt.
    pending_row: adw::ActionRow,

    location: adw::PreferencesGroup,
    place_row: adw::ActionRow,
    coordinates_row: adw::ActionRow,

    /// Das gezeigte Foto: ID und ob es ein Video ist.
    current: RefCell<Option<Shown>>,
    /// Zählt jede Anzeige hoch. Ein EXIF-Ergebnis, das nach einem Blättern
    /// eintrifft, trägt eine alte Nummer und verfällt.
    generation: Cell<u64>,
}

struct Shown {
    id: String,
    is_video: bool,
}

impl InfoPanel {
    pub fn new() -> Rc<Self> {
        let title = gtk::Label::builder()
            .css_classes(["title-3"])
            .xalign(0.0)
            .wrap(true)
            .wrap_mode(gtk::pango::WrapMode::WordChar)
            .build();
        let captured = gtk::Label::builder()
            .css_classes(["dim-label"])
            .xalign(0.0)
            .wrap(true)
            .build();
        let heading = gtk::Box::new(gtk::Orientation::Vertical, 4);
        heading.set_margin_start(6);
        heading.set_margin_end(6);
        heading.append(&title);
        heading.append(&captured);

        // „property" stellt den Wert in den Vordergrund und die Bezeichnung
        // klein darüber — wie im Sync-Manager. Kein Markup: Dateinamen und
        // Ortsnamen kommen von außen, ein „&" darin wäre sonst ein Fehler.
        let row = |title: &str| {
            adw::ActionRow::builder()
                .title(title)
                .use_markup(false)
                .subtitle_selectable(true)
                .css_classes(["property"])
                .visible(false)
                .build()
        };

        let filename_row = row("Filename");
        let kind_row = row("Type");
        let dimensions_row = row("Dimensions");
        let size_row = row("File Size");
        let details = adw::PreferencesGroup::builder().title("Details").build();
        details.add(&filename_row);
        details.add(&kind_row);
        details.add(&dimensions_row);
        details.add(&size_row);

        let camera_row = row("Camera");
        let lens_row = row("Lens");
        let focal_row = row("Focal Length");
        let aperture_row = row("Aperture");
        let shutter_row = row("Shutter Speed");
        let iso_row = row("ISO");
        let flash_row = row("Flash");
        let pending_row = adw::ActionRow::builder()
            .title("Camera details appear when the original has loaded")
            .use_markup(false)
            .css_classes(["dim-label"])
            .visible(false)
            .build();
        let camera = adw::PreferencesGroup::builder().title("Camera").build();
        for row in [
            &camera_row,
            &lens_row,
            &focal_row,
            &aperture_row,
            &shutter_row,
            &iso_row,
            &flash_row,
            &pending_row,
        ] {
            camera.add(row);
        }

        let place_row = row("Place");
        let coordinates_row = row("Coordinates");
        let location = adw::PreferencesGroup::builder().title("Location").build();
        location.add(&place_row);
        location.add(&coordinates_row);

        let column = gtk::Box::new(gtk::Orientation::Vertical, 18);
        column.set_margin_top(12);
        column.set_margin_bottom(24);
        column.set_margin_start(12);
        column.set_margin_end(12);
        column.append(&heading);
        column.append(&details);
        column.append(&camera);
        column.append(&location);

        let widget = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .child(&column)
            .build();

        Rc::new(Self {
            widget,
            title,
            captured,
            details,
            filename_row,
            kind_row,
            dimensions_row,
            size_row,
            camera,
            camera_row,
            lens_row,
            focal_row,
            aperture_row,
            shutter_row,
            iso_row,
            flash_row,
            pending_row,
            location,
            place_row,
            coordinates_row,
            current: RefCell::new(None),
            generation: Cell::new(0),
        })
    }

    /// Zeigt die Angaben zu diesem Foto.
    ///
    /// `captured` ist die fertige Datumszeile aus dem `PhotoObject` — dieselbe
    /// wie im Kopf des Betrachters, damit beide nie auseinanderlaufen.
    pub fn show(self: &Rc<Self>, photo: &Photo, captured: &str, place: Option<&str>) {
        let record = &photo.record;
        let is_video = record.is_video();
        *self.current.borrow_mut() = Some(Shown {
            id: record.id.clone(),
            is_video,
        });
        let generation = self.generation.get() + 1;
        self.generation.set(generation);

        self.title.set_label(place.unwrap_or(&record.filename));
        self.captured.set_label(captured);

        set(&self.filename_row, Some(&record.filename));
        set(
            &self.kind_row,
            Some(if is_video { "Video" } else { "Photo" }),
        );
        let dimensions = match (record.width, record.height) {
            (Some(width), Some(height)) => exif_info::format_dimensions(width, height).map(|d| {
                match exif_info::format_megapixels(width, height) {
                    Some(mp) => format!("{d} · {mp}"),
                    None => d,
                }
            }),
            _ => None,
        };
        set(&self.dimensions_row, dimensions.as_deref());

        let original = photo
            .orig_path
            .as_deref()
            .filter(|path| photo.is_orig_cached && path.is_file());
        // Die Größe aus dem Katalog gleich zeigen; das Lesen des Originals
        // bestätigt sie danach an der Datei selbst.
        let size = original.and(photo.orig_bytes).map(exif_info::format_bytes);
        set(&self.size_row, size.as_deref());
        self.details.set_visible(true);

        self.set_place_rows(place, record.coordinates());

        self.show_exif(None);
        if is_video {
            // Videos tragen kein EXIF; ihre Metadaten stecken im Container und
            // bräuchten GStreamer — für die paar Zeilen nicht lohnend.
            self.pending_row.set_visible(false);
            self.camera.set_visible(false);
        } else if let Some(path) = original {
            // Bis das Ergebnis da ist, bleibt die Gruppe weg — die Werte des
            // vorigen Fotos stünden sonst kurz beim neuen.
            self.pending_row.set_visible(false);
            self.camera.set_visible(false);
            self.load_exif(generation, path.to_path_buf());
        } else {
            self.pending_row.set_visible(true);
            self.camera.set_visible(true);
        }
    }

    /// Das Original zu diesem Foto ist eingetroffen — jetzt gibt es EXIF.
    pub fn original_arrived(self: &Rc<Self>, id: &str, path: &Path) {
        let wanted = self
            .current
            .borrow()
            .as_ref()
            .is_some_and(|shown| shown.id == id && !shown.is_video);
        if wanted {
            self.load_exif(self.generation.get(), path.to_path_buf());
        }
    }

    /// Ein Ortsname ist eingetroffen. Verspätete Antworten für ein anderes
    /// Foto verfallen, wie im Titel des Betrachters.
    pub fn set_place(&self, id: &str, place: &str) {
        let wanted = self
            .current
            .borrow()
            .as_ref()
            .is_some_and(|shown| shown.id == id);
        if !wanted {
            return;
        }
        self.title.set_label(place);
        set(&self.place_row, Some(place));
        self.location.set_visible(true);
    }

    /// Nichts zu zeigen — etwa ein Platzhalter, dessen Foto noch nicht
    /// synchronisiert ist.
    pub fn clear(&self) {
        self.current.borrow_mut().take();
        self.generation.set(self.generation.get() + 1);
        self.title.set_label("");
        self.captured.set_label("");
        self.details.set_visible(false);
        self.camera.set_visible(false);
        self.location.set_visible(false);
    }

    fn set_place_rows(&self, place: Option<&str>, coordinates: Option<(f64, f64)>) {
        set(&self.place_row, place);
        let coordinates = coordinates.map(|(lat, lon)| exif_info::format_coordinates(lat, lon));
        set(&self.coordinates_row, coordinates.as_deref());
        self.location
            .set_visible(place.is_some() || coordinates.is_some());
    }

    /// Liest EXIF und Dateigröße im Hilfsthread.
    ///
    /// Ein HEIC wird dafür nur bis zum EXIF-Kasten gelesen, trotzdem ist es
    /// Datei-I/O auf einem Original von mehreren MB — das gehört nicht in den
    /// UI-Thread, sonst stockt das Blättern.
    fn load_exif(self: &Rc<Self>, generation: u64, path: PathBuf) {
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let source = path.clone();
            let result = gio::spawn_blocking(move || {
                let size = std::fs::metadata(&source).ok().map(|meta| meta.len());
                (exif_info::read(&source), size)
            })
            .await;
            let Some(panel) = weak.upgrade() else { return };
            // Inzwischen weitergeblättert: das Ergebnis gehört nicht hierher.
            if panel.generation.get() != generation {
                return;
            }
            let Ok((exif, size)) = result else { return };
            if let Some(size) = size {
                set(&panel.size_row, Some(&exif_info::format_bytes(size)));
            }
            match exif {
                Ok(info) => panel.show_exif(Some(&info)),
                Err(error) => {
                    tracing::debug!(path = %path.display(), %error, "kein EXIF im Original");
                    panel.show_exif(None);
                }
            }
            panel.pending_row.set_visible(false);
            panel.camera.set_visible(camera_visible(&panel));
        });
    }

    fn show_exif(&self, info: Option<&ExifInfo>) {
        let empty = ExifInfo::default();
        let info = info.unwrap_or(&empty);
        set(&self.camera_row, info.camera.as_deref());
        set(&self.lens_row, info.lens.as_deref());
        set(&self.focal_row, info.focal_length.as_deref());
        set(&self.aperture_row, info.aperture.as_deref());
        set(&self.shutter_row, info.shutter.as_deref());
        set(&self.iso_row, info.iso.as_deref());
        set(&self.flash_row, info.flash.as_deref());
    }
}

/// Ob die Kameragruppe etwas zu zeigen hat. Ein Screenshot hat kein EXIF —
/// dann fehlt die ganze Gruppe statt einer leeren Überschrift.
fn camera_visible(panel: &InfoPanel) -> bool {
    [
        &panel.camera_row,
        &panel.lens_row,
        &panel.focal_row,
        &panel.aperture_row,
        &panel.shutter_row,
        &panel.iso_row,
        &panel.flash_row,
        &panel.pending_row,
    ]
    .iter()
    .any(|row| row.is_visible())
}

/// Setzt den Wert einer Zeile; ohne Wert verschwindet die Zeile.
fn set(row: &adw::ActionRow, value: Option<&str>) {
    match value {
        Some(value) if !value.is_empty() => {
            row.set_subtitle(value);
            row.set_visible(true);
        }
        _ => row.set_visible(false),
    }
}
