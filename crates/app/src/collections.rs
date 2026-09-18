//! „Collections" — die Übersicht über alle Alben, als Karten wie in Photos.
//!
//! Oben die angehefteten (Favorites, Videos), dann die eigenen Alben der
//! obersten Ebene, zuletzt die Medientypen. Ein Ordner öffnet sich hier selbst,
//! mit einem Zurück-Knopf; ein Album wechselt ins Raster.

use std::cell::RefCell;
use std::rc::Rc;

use gtk::prelude::*;
use icloud_photos_cache::Album;
use icloud_photos_core::model::AlbumKind;

use crate::sidebar::{children, display_name, folder_count, icon_name, media_types, pinned};
use crate::sync_manager::thousands;

/// Kantenlänge eines Titelbilds.
const COVER_SIZE: i32 = 168;

/// Was eine Karte auslöst.
#[derive(Debug, Clone)]
pub enum Pick {
    Album(String),
    Folder(String),
}

type PickCallback = Rc<dyn Fn(Pick)>;

pub struct Collections {
    pub widget: gtk::ScrolledWindow,
    content: gtk::Box,
    /// Der geöffnete Ordner, `None` für die oberste Ebene.
    folder: RefCell<Option<String>>,
    albums: RefCell<Vec<Album>>,
    on_pick: RefCell<Option<PickCallback>>,
}

impl Collections {
    pub fn new() -> Rc<Self> {
        let content = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(6)
            .margin_top(12)
            .margin_bottom(24)
            .margin_start(24)
            .margin_end(24)
            .build();
        let widget = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .child(&content)
            .build();
        Rc::new(Self {
            widget,
            content,
            folder: RefCell::new(None),
            albums: RefCell::new(Vec::new()),
            on_pick: RefCell::new(None),
        })
    }

    pub fn connect_pick(&self, callback: impl Fn(Pick) + 'static) {
        *self.on_pick.borrow_mut() = Some(Rc::new(callback));
    }

    pub fn set_albums(self: &Rc<Self>, albums: Vec<Album>) {
        *self.albums.borrow_mut() = albums;
        // Ein verschwundener Ordner führt zurück nach oben.
        let gone = self
            .folder
            .borrow()
            .as_ref()
            .is_some_and(|id| !self.albums.borrow().iter().any(|album| &album.id == id));
        if gone {
            *self.folder.borrow_mut() = None;
        }
        self.rebuild();
    }

    /// Zeigt einen Ordner, oder mit `None` die oberste Ebene.
    pub fn open_folder(self: &Rc<Self>, folder: Option<String>) {
        *self.folder.borrow_mut() = folder;
        self.rebuild();
        self.widget.vadjustment().set_value(0.0);
    }

    fn rebuild(self: &Rc<Self>) {
        while let Some(child) = self.content.first_child() {
            self.content.remove(&child);
        }
        let albums = self.albums.borrow().clone();
        let folder = self.folder.borrow().clone();

        if let Some(folder) = folder {
            let title = albums
                .iter()
                .find(|album| album.id == folder)
                .map(display_name)
                .unwrap_or_default();
            let parent = albums
                .iter()
                .find(|album| album.id == folder)
                .and_then(|album| album.parent.clone());
            self.content.append(&self.folder_header(&title, parent));
            self.section(None, &children(&albums, Some(&folder)), &albums);
            return;
        }

        if albums.is_empty() {
            self.content.append(
                &adw::StatusPage::builder()
                    .icon_name("folder-pictures-symbolic")
                    .title("No Albums Yet")
                    .description("Albums appear here after the next sync.")
                    .vexpand(true)
                    .build(),
            );
            return;
        }
        self.section(Some("Pinned"), &pinned(&albums), &albums);
        self.section(Some("Albums"), &children(&albums, None), &albums);
        self.section(Some("Media Types"), &media_types(&albums), &albums);
    }

    /// Kopfzeile eines geöffneten Ordners: zurück und Name.
    fn folder_header(self: &Rc<Self>, title: &str, parent: Option<String>) -> gtk::Box {
        let row = gtk::Box::builder().spacing(8).margin_bottom(6).build();
        let back = gtk::Button::builder()
            .icon_name("go-previous-symbolic")
            .tooltip_text("Back")
            .css_classes(["flat"])
            .build();
        back.connect_clicked({
            let weak = Rc::downgrade(self);
            move |_| {
                if let Some(collections) = weak.upgrade() {
                    collections.open_folder(parent.clone());
                }
            }
        });
        row.append(&back);
        row.append(
            &gtk::Label::builder()
                .label(title)
                .css_classes(["title-2"])
                .xalign(0.0)
                .build(),
        );
        row
    }

    fn section(self: &Rc<Self>, title: Option<&str>, items: &[&Album], all: &[Album]) {
        if items.is_empty() {
            return;
        }
        if let Some(title) = title {
            self.content.append(
                &gtk::Label::builder()
                    .label(title)
                    .css_classes(["title-4"])
                    .xalign(0.0)
                    .margin_top(12)
                    .build(),
            );
        }
        let flow = gtk::FlowBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .homogeneous(true)
            .min_children_per_line(2)
            .max_children_per_line(12)
            .column_spacing(12)
            .row_spacing(12)
            .build();
        for album in items {
            flow.append(&self.card(album, all));
        }
        self.content.append(&flow);
    }

    fn card(self: &Rc<Self>, album: &Album, all: &[Album]) -> gtk::Button {
        let (count, cover) = match album.kind {
            AlbumKind::Folder => (folder_count(all, &album.id), folder_cover(all, &album.id)),
            _ => (album.count, album.cover.clone()),
        };

        let picture = gtk::Picture::builder()
            .content_fit(gtk::ContentFit::Cover)
            .width_request(COVER_SIZE)
            .height_request(COVER_SIZE)
            .css_classes(["card"])
            .build();
        picture.set_overflow(gtk::Overflow::Hidden);
        let frame = gtk::Overlay::builder().child(&picture).build();
        match cover {
            Some(path) => picture.set_filename(Some(&path)),
            // Ohne Titelbild ein Symbol auf der leeren Karte.
            None => {
                let icon = gtk::Image::builder()
                    .icon_name(icon_name(album))
                    .pixel_size(48)
                    .css_classes(["dim-label"])
                    .build();
                frame.add_overlay(&icon);
            }
        }

        let name = gtk::Label::builder()
            .label(display_name(album))
            .xalign(0.0)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .max_width_chars(18)
            .css_classes(["heading"])
            .build();
        let detail = gtk::Label::builder()
            .label(thousands(count))
            .xalign(0.0)
            .css_classes(["dim-label", "caption", "numeric"])
            .build();

        let body = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(4)
            .build();
        body.append(&frame);
        body.append(&name);
        body.append(&detail);

        let button = gtk::Button::builder()
            .child(&body)
            .css_classes(["flat"])
            .tooltip_text(display_name(album))
            .build();
        let pick = match album.kind {
            AlbumKind::Folder => Pick::Folder(album.id.clone()),
            _ => Pick::Album(album.id.clone()),
        };
        button.connect_clicked({
            let weak = Rc::downgrade(self);
            move |_| {
                let Some(collections) = weak.upgrade() else {
                    return;
                };
                if let Pick::Folder(id) = &pick {
                    collections.open_folder(Some(id.clone()));
                }
                let callback = collections.on_pick.borrow().clone();
                if let Some(callback) = callback {
                    callback(pick.clone());
                }
            }
        });
        button
    }
}

/// Titelbild eines Ordners: das des ersten Albums darin, das eins hat.
fn folder_cover(all: &[Album], folder: &str) -> Option<std::path::PathBuf> {
    children(all, Some(folder))
        .iter()
        .find_map(|album| match album.kind {
            AlbumKind::Folder => folder_cover(all, &album.id),
            _ => album.cover.clone(),
        })
}
