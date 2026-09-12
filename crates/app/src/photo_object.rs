//! GObject-Wrapper für eine Katalogzeile, damit Fotos in ein `GListModel` passen.

use chrono::{Datelike, Local};
use gtk::glib;
use gtk::subclass::prelude::*;
use icloud_photos_core::model::Photo;

/// Monatsnamen von Hand: `chrono` formatiert `%B` nur englisch, und eine
/// Lokalisierungsbibliothek fuer zwoelf Woerter waere unverhaeltnismaessig.
pub fn month_name(month: u32) -> &'static str {
    const NAMES: [&str; 12] = [
        "Januar",
        "Februar",
        "März",
        "April",
        "Mai",
        "Juni",
        "Juli",
        "August",
        "September",
        "Oktober",
        "November",
        "Dezember",
    ];
    NAMES.get((month as usize).saturating_sub(1)).unwrap_or(&"")
}

mod imp {
    use std::cell::RefCell;

    use super::*;

    #[derive(Default)]
    pub struct PhotoObject {
        pub id: RefCell<String>,
        pub filename: RefCell<String>,
        pub thumb_path: RefCell<Option<String>>,
        pub orig_path: RefCell<Option<String>>,
        pub is_orig_cached: RefCell<bool>,
        /// Platzhalter fuer ein noch nicht synchronisiertes Foto.
        pub is_placeholder: RefCell<bool>,
        pub is_video: RefCell<bool>,
        /// Aufnahmeort, sofern das Foto einen hat.
        pub coordinates: RefCell<Option<(f64, f64)>>,
        /// Aufnahmemonat als Beschriftung, z.B. "Juli 2024" — die Ansicht
        /// zeigt ihn als mitlaufende Datumszeile ueber dem Raster.
        pub month_label: RefCell<String>,
        /// Voller Aufnahmezeitpunkt fuer die Kopfzeile des Viewers.
        pub captured_label: RefCell<String>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for PhotoObject {
        const NAME: &'static str = "IcloudPhotoObject";
        type Type = super::PhotoObject;
    }

    impl ObjectImpl for PhotoObject {}
}

glib::wrapper! {
    pub struct PhotoObject(ObjectSubclass<imp::PhotoObject>);
}

impl PhotoObject {
    pub fn new(photo: &Photo) -> Self {
        let object: Self = glib::Object::builder().build();
        let imp = object.imp();
        *imp.id.borrow_mut() = photo.id().to_string();
        *imp.filename.borrow_mut() = photo.record.filename.clone();
        *imp.thumb_path.borrow_mut() = photo
            .thumb_path
            .as_ref()
            .map(|p| p.to_string_lossy().into_owned());
        *imp.orig_path.borrow_mut() = photo
            .orig_path
            .as_ref()
            .map(|p| p.to_string_lossy().into_owned());
        *imp.is_orig_cached.borrow_mut() = photo.is_orig_cached;
        *imp.is_video.borrow_mut() = photo.record.is_video();
        *imp.coordinates.borrow_mut() = photo.record.coordinates();

        // Gezeigt wird Ortszeit: der Katalog haelt UTC, aber "aufgenommen am"
        // meint die Uhr, die beim Auslösen danebenstand.
        let captured = photo.record.capture_date().with_timezone(&Local);
        *imp.month_label.borrow_mut() =
            format!("{} {}", month_name(captured.month()), captured.format("%Y"));
        *imp.captured_label.borrow_mut() = format!(
            "{}. {} {}, {}",
            captured.format("%-d"),
            month_name(captured.month()),
            captured.format("%Y"),
            captured.format("%H:%M")
        );
        object
    }

    /// Ein Skelett fuer ein Foto, das der Sync noch nicht geliefert hat.
    pub fn placeholder() -> Self {
        let object: Self = glib::Object::builder().build();
        *object.imp().is_placeholder.borrow_mut() = true;
        object
    }

    pub fn is_placeholder(&self) -> bool {
        *self.imp().is_placeholder.borrow()
    }

    /// Ob die Kachel als Skelett dargestellt werden muss — Platzhalter oder
    /// ein Foto, dessen Vorschau noch fehlt.
    pub fn needs_skeleton(&self) -> bool {
        self.is_placeholder() || self.thumb_path().is_none()
    }

    pub fn id(&self) -> String {
        self.imp().id.borrow().clone()
    }

    pub fn filename(&self) -> String {
        self.imp().filename.borrow().clone()
    }

    pub fn thumb_path(&self) -> Option<String> {
        self.imp().thumb_path.borrow().clone()
    }

    pub fn is_video(&self) -> bool {
        *self.imp().is_video.borrow()
    }

    /// Breiten- und Längengrad der Aufnahme, sofern hinterlegt.
    pub fn coordinates(&self) -> Option<(f64, f64)> {
        *self.imp().coordinates.borrow()
    }

    pub fn month_label(&self) -> String {
        self.imp().month_label.borrow().clone()
    }

    pub fn captured_label(&self) -> String {
        self.imp().captured_label.borrow().clone()
    }

    /// Pfad zum Original, sofern es wirklich im Cache liegt.
    pub fn cached_original(&self) -> Option<String> {
        if *self.imp().is_orig_cached.borrow() {
            self.imp().orig_path.borrow().clone()
        } else {
            None
        }
    }
}
