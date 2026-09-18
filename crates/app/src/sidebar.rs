//! Die Seitenleiste — aufgebaut wie in Apple Photos.
//!
//! Oben stehen Library und Collections, darunter drei Abschnitte: „Pinned" mit
//! Favorites und Videos, „Albums" mit den eigenen Alben samt Ordnern und
//! „Media Types" mit den übrigen intelligenten Alben. Leere intelligente Alben
//! fehlen, wie in Photos.
//!
//! Ordner klappen auf und zu, statt selbst etwas zu zeigen: Sie enthalten
//! keine Fotos, nur Alben.

use std::cell::RefCell;
use std::collections::HashSet;
use std::rc::Rc;

use gtk::prelude::*;
use icloud_photos_cache::Album;
use icloud_photos_core::model::AlbumKind;

use crate::sync_manager::thousands;

/// Was ausgewählt werden kann.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    Library,
    Collections,
    Album(String),
}

/// Intelligente Alben, die oben unter „Pinned" stehen — wie in Photos.
const PINNED: [&str; 2] = ["Favorites", "Videos"];

/// Anzeigename eines intelligenten Albums. iCloud nennt sie englisch, aber
/// nicht immer so, wie Photos sie zeigt.
pub fn display_name(album: &Album) -> String {
    match (album.kind, album.id.as_str()) {
        (AlbumKind::Smart, "Live") => "Live Photos".to_string(),
        _ => album.name.trim().to_string(),
    }
}

/// Symbol eines Albums in Seitenleiste und Übersicht.
pub fn icon_name(album: &Album) -> &'static str {
    match album.kind {
        AlbumKind::Folder => "folder-symbolic",
        AlbumKind::Album => "folder-pictures-symbolic",
        AlbumKind::Smart => match album.id.as_str() {
            "Favorites" => "starred-symbolic",
            "Videos" => "camera-video-symbolic",
            "Live" => "camera-photo-symbolic",
            "Panoramas" => "zoom-fit-best-symbolic",
            "Screenshots" => "applets-screenshooter-symbolic",
            "Slo-mo" => "media-playback-start-symbolic",
            "Time-lapse" => "alarm-symbolic",
            "Bursts" => "view-paged-symbolic",
            _ => "folder-pictures-symbolic",
        },
    }
}

/// Die Abschnitte, in die Alben fallen — auch für die Übersicht.
pub fn pinned(albums: &[Album]) -> Vec<&Album> {
    PINNED
        .iter()
        .filter_map(|id| {
            albums
                .iter()
                .find(|album| album.kind == AlbumKind::Smart && album.id == *id)
        })
        .filter(|album| album.count > 0)
        .collect()
}

pub fn media_types(albums: &[Album]) -> Vec<&Album> {
    albums
        .iter()
        .filter(|album| album.kind == AlbumKind::Smart && !PINNED.contains(&album.id.as_str()))
        .filter(|album| album.count > 0)
        .collect()
}

/// Eigene Alben und Ordner direkt unter `parent` (`None` = oberste Ebene).
///
/// Ein Album, dessen Ordner iCloud nicht mitgeliefert hat, steht oben — sonst
/// wäre es unerreichbar.
pub fn children<'a>(albums: &'a [Album], parent: Option<&str>) -> Vec<&'a Album> {
    let known: HashSet<&str> = albums.iter().map(|album| album.id.as_str()).collect();
    albums
        .iter()
        .filter(|album| album.kind != AlbumKind::Smart)
        .filter(|album| {
            let effective = album
                .parent
                .as_deref()
                .filter(|parent| known.contains(parent));
            effective == parent
        })
        .collect()
}

/// Wie viele Fotos ein Ordner insgesamt enthält — über alle Ebenen.
pub fn folder_count(albums: &[Album], folder: &str) -> u64 {
    children(albums, Some(folder))
        .iter()
        .map(|album| match album.kind {
            AlbumKind::Folder => folder_count(albums, &album.id),
            _ => album.count,
        })
        .sum()
}

enum Row {
    Target(Target),
    Folder(String),
}

type SelectCallback = Rc<dyn Fn(&Target)>;

pub struct Sidebar {
    pub widget: gtk::ScrolledWindow,
    list: gtk::ListBox,
    rows: RefCell<Vec<(gtk::ListBoxRow, Row)>>,
    albums: RefCell<Vec<Album>>,
    expanded: RefCell<HashSet<String>>,
    current: RefCell<Target>,
    on_select: RefCell<Option<SelectCallback>>,
}

impl Sidebar {
    pub fn new() -> Rc<Self> {
        let list = gtk::ListBox::builder()
            .css_classes(["navigation-sidebar"])
            .selection_mode(gtk::SelectionMode::Single)
            .activate_on_single_click(true)
            .build();
        let widget = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .child(&list)
            .build();

        let sidebar = Rc::new(Self {
            widget,
            list: list.clone(),
            rows: RefCell::new(Vec::new()),
            albums: RefCell::new(Vec::new()),
            expanded: RefCell::new(HashSet::new()),
            current: RefCell::new(Target::Library),
            on_select: RefCell::new(None),
        });

        list.connect_row_activated({
            let weak = Rc::downgrade(&sidebar);
            move |_, row| {
                if let Some(sidebar) = weak.upgrade() {
                    sidebar.activated(row);
                }
            }
        });
        sidebar.rebuild();
        sidebar
    }

    pub fn connect_select(&self, callback: impl Fn(&Target) + 'static) {
        *self.on_select.borrow_mut() = Some(Rc::new(callback));
    }

    pub fn albums(&self) -> Vec<Album> {
        self.albums.borrow().clone()
    }

    /// Übernimmt einen neuen Album-Stand. Ist das gewählte Album verschwunden,
    /// geht es zurück zur Bibliothek.
    pub fn set_albums(&self, albums: Vec<Album>) {
        *self.albums.borrow_mut() = albums;
        let gone = match &*self.current.borrow() {
            Target::Album(id) => !self.albums.borrow().iter().any(|album| &album.id == id),
            _ => false,
        };
        self.rebuild();
        if gone {
            self.select(&Target::Library);
        }
    }

    /// Wählt ein Ziel aus, als hätte der Nutzer darauf geklickt. Liegt ein
    /// Album in einem zugeklappten Ordner, klappt der auf.
    pub fn select(&self, target: &Target) {
        if let Target::Album(id) = target {
            let mut parent = self
                .albums
                .borrow()
                .iter()
                .find(|album| &album.id == id)
                .and_then(|album| album.parent.clone());
            let mut opened = false;
            // Begrenzt, falls iCloud je einen Ordner in sich selbst meldet.
            let mut depth = 0;
            while let Some(folder) = parent.filter(|_| depth < 32) {
                depth += 1;
                opened |= self.expanded.borrow_mut().insert(folder.clone());
                parent = self
                    .albums
                    .borrow()
                    .iter()
                    .find(|album| album.id == folder)
                    .and_then(|album| album.parent.clone());
            }
            if opened {
                self.rebuild();
            }
        }
        *self.current.borrow_mut() = target.clone();
        self.highlight();
        let callback = self.on_select.borrow().clone();
        if let Some(callback) = callback {
            callback(target);
        }
    }

    /// Klappt einen Ordner auf, etwa wenn die Übersicht ihn öffnet.
    pub fn expand(&self, folder: &str) {
        if self.expanded.borrow_mut().insert(folder.to_string()) {
            self.rebuild();
        }
    }

    fn activated(&self, row: &gtk::ListBoxRow) {
        let action = self
            .rows
            .borrow()
            .iter()
            .find(|(candidate, _)| candidate == row)
            .map(|(_, kind)| match kind {
                Row::Target(target) => Ok(target.clone()),
                Row::Folder(id) => Err(id.clone()),
            });
        match action {
            Some(Ok(target)) => self.select(&target),
            Some(Err(folder)) => {
                {
                    let mut expanded = self.expanded.borrow_mut();
                    if !expanded.remove(&folder) {
                        expanded.insert(folder);
                    }
                }
                self.rebuild();
            }
            None => {}
        }
    }

    /// Markiert die Zeile des aktuellen Ziels.
    fn highlight(&self) {
        let current = self.current.borrow().clone();
        let row = self
            .rows
            .borrow()
            .iter()
            .find(|(_, kind)| matches!(kind, Row::Target(target) if *target == current))
            .map(|(row, _)| row.clone());
        match row {
            Some(row) => self.list.select_row(Some(&row)),
            None => self.list.unselect_all(),
        }
    }

    fn rebuild(&self) {
        while let Some(child) = self.list.first_child() {
            self.list.remove(&child);
        }
        self.rows.borrow_mut().clear();

        self.push(
            entry("image-x-generic-symbolic", "Library", None, 0),
            Row::Target(Target::Library),
        );
        self.push(
            entry("view-grid-symbolic", "Collections", None, 0),
            Row::Target(Target::Collections),
        );

        let albums = self.albums.borrow().clone();

        let pinned = pinned(&albums);
        if !pinned.is_empty() {
            self.list.append(&heading("Pinned"));
            for album in pinned {
                self.push_album(album, 0);
            }
        }

        if !children(&albums, None).is_empty() {
            self.list.append(&heading("Albums"));
            self.push_tree(&albums, None, 0);
        }

        let media = media_types(&albums);
        if !media.is_empty() {
            self.list.append(&heading("Media Types"));
            for album in media {
                self.push_album(album, 0);
            }
        }

        self.highlight();
    }

    fn push_tree(&self, albums: &[Album], parent: Option<&str>, depth: u32) {
        for album in children(albums, parent) {
            if album.kind == AlbumKind::Folder {
                let open = self.expanded.borrow().contains(&album.id);
                let row = entry(
                    icon_name(album),
                    &display_name(album),
                    Some(folder_count(albums, &album.id)),
                    depth,
                );
                add_disclosure(&row, open);
                self.push(row, Row::Folder(album.id.clone()));
                if open {
                    self.push_tree(albums, Some(&album.id), depth + 1);
                }
            } else {
                self.push_album(album, depth);
            }
        }
    }

    fn push_album(&self, album: &Album, depth: u32) {
        let row = entry(
            icon_name(album),
            &display_name(album),
            Some(album.count),
            depth,
        );
        self.push(row, Row::Target(Target::Album(album.id.clone())));
    }

    fn push(&self, row: gtk::ListBoxRow, kind: Row) {
        self.list.append(&row);
        self.rows.borrow_mut().push((row, kind));
    }
}

/// Eine Zeile: Symbol, Name und rechts die Anzahl.
fn entry(icon: &str, label: &str, count: Option<u64>, depth: u32) -> gtk::ListBoxRow {
    let content = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(10)
        // Unterordner rücken ein, wie im Finder.
        .margin_start(depth as i32 * 16)
        .build();
    content.append(&gtk::Image::from_icon_name(icon));
    let name = gtk::Label::builder()
        .label(label)
        .xalign(0.0)
        .hexpand(true)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .tooltip_text(label)
        .build();
    content.append(&name);
    if let Some(count) = count {
        content.append(
            &gtk::Label::builder()
                .label(thousands(count))
                .css_classes(["dim-label", "numeric", "caption"])
                .build(),
        );
    }
    gtk::ListBoxRow::builder().child(&content).build()
}

/// Pfeil vor einem Ordner: zeigt, ob er auf- oder zugeklappt ist.
fn add_disclosure(row: &gtk::ListBoxRow, open: bool) {
    let Some(content) = row.child().and_downcast::<gtk::Box>() else {
        return;
    };
    let arrow = gtk::Image::from_icon_name(if open {
        "pan-down-symbolic"
    } else {
        "pan-end-symbolic"
    });
    arrow.add_css_class("dim-label");
    content.prepend(&arrow);
}

/// Abschnittsüberschrift — nicht auswählbar.
fn heading(title: &str) -> gtk::ListBoxRow {
    let label = gtk::Label::builder()
        .label(title)
        .xalign(0.0)
        .css_classes(["heading", "dim-label"])
        .margin_top(12)
        .build();
    gtk::ListBoxRow::builder()
        .child(&label)
        .selectable(false)
        .activatable(false)
        .can_focus(false)
        .build()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn album(id: &str, kind: AlbumKind, parent: Option<&str>, count: u64) -> Album {
        Album {
            id: id.into(),
            name: id.into(),
            kind,
            parent: parent.map(Into::into),
            count,
            cover: None,
        }
    }

    fn library() -> Vec<Album> {
        vec![
            album("Videos", AlbumKind::Smart, None, 5),
            album("Favorites", AlbumKind::Smart, None, 3),
            album("Bursts", AlbumKind::Smart, None, 0),
            album("Live", AlbumKind::Smart, None, 9),
            album("2024", AlbumKind::Folder, None, 0),
            album("trip", AlbumKind::Album, Some("2024"), 4),
            album("inner", AlbumKind::Folder, Some("2024"), 0),
            album("deep", AlbumKind::Album, Some("inner"), 6),
            album("orphan", AlbumKind::Album, Some("gone"), 1),
            album("loose", AlbumKind::Album, None, 2),
        ]
    }

    #[test]
    fn pinned_keeps_photos_order_not_icloud_order() {
        let albums = library();
        let ids: Vec<_> = pinned(&albums).iter().map(|a| a.id.as_str()).collect();
        assert_eq!(ids, ["Favorites", "Videos"]);
    }

    #[test]
    fn empty_media_types_are_hidden() {
        let albums = library();
        let ids: Vec<_> = media_types(&albums).iter().map(|a| a.id.as_str()).collect();
        assert_eq!(ids, ["Live"]);
    }

    #[test]
    fn an_album_without_its_folder_moves_to_the_top() {
        let albums = library();
        let top: Vec<_> = children(&albums, None)
            .iter()
            .map(|a| a.id.as_str())
            .collect();
        assert_eq!(top, ["2024", "orphan", "loose"]);
    }

    #[test]
    fn folders_count_everything_below_them() {
        assert_eq!(folder_count(&library(), "2024"), 10);
    }

    #[test]
    fn live_photos_get_their_photos_name() {
        let live = album("Live", AlbumKind::Smart, None, 1);
        assert_eq!(display_name(&live), "Live Photos");
    }
}
