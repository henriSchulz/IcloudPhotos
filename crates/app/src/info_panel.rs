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

    details: gtk::Box,
    filename_row: Fact,
    kind_row: Fact,
    dimensions_row: Fact,
    size_row: Fact,

    camera: gtk::Box,
    camera_row: Fact,
    lens_row: Fact,
    focal_row: Fact,
    aperture_row: Fact,
    shutter_row: Fact,
    iso_row: Fact,
    flash_row: Fact,
    /// Hinweis, solange das Original — und damit EXIF — noch fehlt.
    pending_row: Fact,

    location: gtk::Box,
    place_row: Fact,
    coordinates_row: Fact,

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
            .css_classes(["info-title"])
            .xalign(0.0)
            .wrap(true)
            .wrap_mode(gtk::pango::WrapMode::WordChar)
            .build();
        let captured = gtk::Label::builder()
            .css_classes(["info-when"])
            .xalign(0.0)
            .wrap(true)
            .build();
        let heading = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .css_classes(["info-heading"])
            .build();
        heading.append(&title);
        heading.append(&captured);

        // Wie im Lab: Bezeichnung links, Wert rechts, eine Zeile je Angabe.
        // Kein Markup: Dateinamen und Ortsnamen kommen von außen.
        let row = |title: &str| Fact::new(title);

        let filename_row = row("Filename");
        let kind_row = row("Type");
        let dimensions_row = row("Dimensions");
        let size_row = row("File Size");
        let details = group("Details");
        for row in [&filename_row, &kind_row, &dimensions_row, &size_row] {
            details.append(&row.row);
        }

        let camera_row = row("Camera");
        let lens_row = row("Lens");
        let focal_row = row("Focal Length");
        let aperture_row = row("Aperture");
        let shutter_row = row("Shutter Speed");
        let iso_row = row("ISO");
        let flash_row = row("Flash");
        let pending_row = Fact::note("Camera details appear when the original has loaded");
        let camera = group("Camera");
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
            camera.append(&row.row);
        }

        let place_row = row("Place");
        let coordinates_row = row("Coordinates");
        let location = group("Location");
        location.append(&place_row.row);
        location.append(&coordinates_row.row);

        let column = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .css_classes(["info-column"])
            .build();
        column.append(&heading);
        column.append(&details);
        column.append(&camera);
        column.append(&location);

        let widget = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .child(&column)
            .css_classes(["photos-info"])
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
fn set(row: &Fact, value: Option<&str>) {
    match value {
        Some(value) if !value.is_empty() => {
            row.value.set_label(value);
            row.set_visible(true);
        }
        _ => row.set_visible(false),
    }
}

/// Eine Angabe: Bezeichnung links, Wert rechts (wählbar, bricht um).
#[derive(Clone)]
struct Fact {
    row: gtk::Box,
    value: gtk::Label,
}

impl Fact {
    fn new(title: &str) -> Self {
        let name = gtk::Label::builder()
            .label(title)
            .xalign(0.0)
            .valign(gtk::Align::Start)
            .css_classes(["fact-name"])
            .build();
        let value = gtk::Label::builder()
            .xalign(1.0)
            .hexpand(true)
            .wrap(true)
            .wrap_mode(gtk::pango::WrapMode::WordChar)
            .justify(gtk::Justification::Right)
            .selectable(true)
            .css_classes(["fact-value"])
            .build();
        let row = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .css_classes(["fact"])
            .visible(false)
            .build();
        row.append(&name);
        row.append(&value);
        Self { row, value }
    }

    /// Ein Hinweis über die ganze Breite statt einer Angabe.
    fn note(text: &str) -> Self {
        let value = gtk::Label::builder()
            .label(text)
            .xalign(0.0)
            .wrap(true)
            .css_classes(["fact-note"])
            .build();
        let row = gtk::Box::builder().visible(false).build();
        row.append(&value);
        Self { row, value }
    }

    fn is_visible(&self) -> bool {
        self.row.is_visible()
    }

    fn set_visible(&self, visible: bool) {
        self.row.set_visible(visible);
    }
}

/// Eine Gruppe mit Überschrift (Details, Camera, Location).
fn group(title: &str) -> gtk::Box {
    let group = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .css_classes(["facts"])
        .build();
    group.append(
        &gtk::Label::builder()
            .label(title)
            .xalign(0.0)
            .css_classes(["facts-title"])
            .build(),
    );
    group
}
