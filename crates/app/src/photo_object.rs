//! GObject-Wrapper für eine Katalogzeile, damit Fotos in ein `GListModel` passen.

use gtk::glib;
use gtk::subclass::prelude::*;
use icloud_photos_core::model::Photo;

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

    /// Pfad zum Original, sofern es wirklich im Cache liegt.
    pub fn cached_original(&self) -> Option<String> {
        if *self.imp().is_orig_cached.borrow() {
            self.imp().orig_path.borrow().clone()
        } else {
            None
        }
    }
}
