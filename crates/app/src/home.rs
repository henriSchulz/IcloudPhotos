//! „Home" — die Startseite nach Vorschlag A des Velum-Labs
//! (`~/Projects/velum/lab/web/photos.html?p=a&s=home`).
//!
//! Karten wie in System Settings und im Finder: links „On This Day" (derselbe
//! Tag in einem früheren Jahr, wie `velum-photo-picks` es für das Foto-Widget
//! auswählt) und „Recent Days", rechts iCloud (Stand, Bestand, Originale-
//! Cache, Sync Now), „Add Photos" und die drei angehefteten Sammlungen
//! (Favorites, Map, Videos); darunter die Alben und die Medientypen.
//!
//! Alles kommt aus dem Katalog — nichts ist ausgedacht. Was die App nicht
//! weiß, fehlt: den iCloud-Speicherplatz kennt sie nicht (pyicloud fragt ihn
//! nicht ab), also steht er nicht da; die Zahl „in iCloud" nur, wenn ein
//! Erstaufbau sie gemeldet hat.
//!
//! Zwei Spalten ab `surface.photos.twoColumnsFrom` (eigene Breite),
//! darunter eine — `layout::Columns`.

use std::path::{Path, PathBuf};
use std::rc::Rc;

use chrono::{Datelike, NaiveDate};
use gtk::prelude::*;
use icloud_photos_cache::{Album, Day, Memory};
use icloud_photos_core::model::AlbumKind;

use crate::layout::{Columns, RatioBox};
use crate::photo_object::month_name;
use crate::sidebar::{children, display_name, glyph_name, media_types};
use crate::sync_manager::{SyncStatus, thousands};
use crate::velum;

/// Was ein Klick auf der Startseite auslöst.
#[derive(Debug, Clone)]
pub enum Event {
    /// Ein Foto im Betrachter öffnen (Katalog-ID).
    Photo(String),
    /// Alle Fotos eines Tages als Ausschnitt.
    Day(NaiveDate),
    Library,
    Album(String),
    Folder(String),
    Map,
    Collections,
    SyncNow,
    SyncDetails,
    AddPhotos,
}

/// Der Stand der iCloud-Karte.
#[derive(Debug, Clone, Default)]
pub struct Cloud {
    pub status: Option<SyncStatus>,
    /// „vor 4 Minuten".
    pub last_sync: Option<String>,
    pub on_device: u64,
    /// Bestand laut iCloud — nur bekannt, wenn ein Erstaufbau ihn gemeldet hat.
    pub in_icloud: Option<u64>,
    pub cache_used: u64,
    pub cache_limit: u64,
    pub cache_label: String,
}

/// Was die Startseite zeigt, gelesen aus dem Katalog.
#[derive(Debug, Clone, Default)]
pub struct Data {
    pub today: Option<NaiveDate>,
    pub memory: Option<Memory>,
    pub recent: Vec<Day>,
    pub albums: Vec<Album>,
    /// Bis zu vier Vorschauen aus Favorites.
    pub favorites: Vec<PathBuf>,
    /// Vorschau des jüngsten Fotos mit Ort und wie viele einen haben.
    pub map_cover: Option<PathBuf>,
    pub located: u64,
    pub video_cover: Option<PathBuf>,
    pub videos: u64,
}

type Handler = Rc<dyn Fn(Event)>;

pub struct Home {
    pub widget: gtk::ScrolledWindow,
    content: gtk::Box,
    cloud: gtk::Box,
    handler: Handler,
}

impl Home {
    pub fn new(handler: impl Fn(Event) + 'static) -> Rc<Self> {
        let content = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .css_classes(["photos-home"])
            .build();
        let widget = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .child(&content)
            .css_classes(["photos-pane"])
            .build();
        // Die Seite bekommt ihre Wunschhöhe, nicht ihr Minimum: Titelbilder
        // melden kein Minimum (siehe `layout::RatioBox`).
        if let Some(viewport) = widget.child().and_downcast::<gtk::Viewport>() {
            viewport.set_vscroll_policy(gtk::ScrollablePolicy::Natural);
        }
        let cloud = card();
        cloud.add_css_class("cloud-card");
        Rc::new(Self {
            widget,
            content,
            cloud,
            handler: Rc::new(handler),
        })
    }

    fn emit(&self, event: Event) -> impl Fn() + 'static {
        let handler = self.handler.clone();
        move || handler(event.clone())
    }

    /// Baut die Seite aus `data` neu. Billig: ein paar Dutzend Widgets, die
    /// Vorschauen kommen aus dem Speicher der App.
    pub fn show(&self, data: &Data, cloud: &Cloud) {
        while let Some(child) = self.content.first_child() {
            self.content.remove(&child);
        }
        if let Some(parent) = self.cloud.parent().and_downcast::<gtk::Box>() {
            parent.remove(&self.cloud);
        }

        let left = column();
        left.append(&self.memory_card(data));
        left.append(&self.recent_card(data));
        let right = column();
        self.show_cloud(cloud);
        right.append(&self.cloud);
        right.append(&self.add_card());
        if let Some(pinned) = self.pinned_card(data) {
            right.append(&pinned);
        }
        let columns = Columns::new(
            &left,
            &right,
            velum::photos("twoColumnsFrom", 728.0) as i32,
            velum::photos("homeLead", 1.45),
            velum::photos("cardGap", 12.0) as i32,
        );
        self.content.append(&columns);

        self.albums_section(&data.albums);
        self.media_section(&data.albums);
    }

    /// Nur die iCloud-Karte — bei jedem Wechsel des Sync-Stands.
    pub fn show_cloud(&self, cloud: &Cloud) {
        while let Some(child) = self.cloud.first_child() {
            self.cloud.remove(&child);
        }
        let status = cloud.status.clone().unwrap_or(SyncStatus {
            running: false,
            failed: false,
            phase: String::new(),
        });
        let glyph = crate::icons::icon(
            if status.failed { "warning" } else { "cloud" },
            crate::icons::Weight::Regular,
        );
        glyph.add_css_class("stat-glyph");
        glyph.set_valign(gtk::Align::Center);
        let state = if status.running {
            "Syncing"
        } else if status.failed {
            "Sync Failed"
        } else {
            "Up to Date"
        };
        let title = label(&format!("iCloud · {state}"), "stat-title");
        let detail = label(
            &if status.running {
                status.phase.clone()
            } else {
                match &cloud.last_sync {
                    Some(when) => format!("Last synced {when}"),
                    None => "Not synced yet".to_string(),
                }
            },
            "card-sub",
        );
        let texts = gtk::Box::new(gtk::Orientation::Vertical, 0);
        texts.set_valign(gtk::Align::Center);
        texts.append(&title);
        texts.append(&detail);
        let stat = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .css_classes(["stat"])
            .build();
        stat.append(&glyph);
        stat.append(&texts);
        self.cloud.append(&stat);

        let facts = gtk::Grid::builder().css_classes(["facts"]).build();
        let mut row = 0;
        let mut fact = |name: &str, value: String| {
            facts.attach(&label(name, "fact-name"), 0, row, 1, 1);
            let value = label(&value, "fact-value");
            value.set_xalign(1.0);
            value.set_hexpand(true);
            facts.attach(&value, 1, row, 1, 1);
            row += 1;
        };
        fact("Photos on This Device", thousands(cloud.on_device));
        if let Some(total) = cloud.in_icloud {
            fact("Photos in iCloud", thousands(total));
        }
        fact("Originals Cache", cloud.cache_label.clone());
        self.cloud.append(&facts);
        let meter = gtk::LevelBar::builder()
            .min_value(0.0)
            .max_value(1.0)
            .value(if cloud.cache_limit > 0 {
                (cloud.cache_used as f64 / cloud.cache_limit as f64).clamp(0.0, 1.0)
            } else {
                0.0
            })
            .css_classes(["cache-meter"])
            .build();
        meter.remove_offset_value(Some(gtk::LEVEL_BAR_OFFSET_LOW));
        meter.remove_offset_value(Some(gtk::LEVEL_BAR_OFFSET_HIGH));
        meter.remove_offset_value(Some(gtk::LEVEL_BAR_OFFSET_FULL));
        self.cloud.append(&meter);

        let now = gtk::Button::builder()
            .label("Sync Now")
            .css_classes(["suggested-action"])
            .sensitive(!status.running)
            .build();
        let emit = self.emit(Event::SyncNow);
        now.connect_clicked(move |_| emit());
        let details = gtk::Button::builder().label("Details…").build();
        let emit = self.emit(Event::SyncDetails);
        details.connect_clicked(move |_| emit());
        let buttons = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .css_classes(["card-buttons"])
            .build();
        buttons.append(&now);
        buttons.append(&details);
        self.cloud.append(&buttons);
    }

    fn memory_card(&self, data: &Data) -> gtk::Box {
        let card = card();
        card.add_css_class("memory-card");
        let Some(memory) = &data.memory else {
            card.append(&card_head("On This Day", None));
            card.append(&label(
                "Nothing from this day in earlier years.",
                "card-sub",
            ));
            return card;
        };
        let link = link_button("Show All", self.emit(Event::Day(memory.day)));
        card.append(&card_head("On This Day", Some(&link)));
        let years = data
            .today
            .map(|today| today.year() - memory.day.year())
            .unwrap_or(1);
        card.append(&label(
            &format!(
                "{} · {}",
                long_date(memory.day),
                match years {
                    1 => "one year ago".to_string(),
                    n => format!("{n} years ago"),
                }
            ),
            "card-sub",
        ));
        if let Some(first) = memory.photos.first() {
            let hero = self.photo(
                first.thumb_path.as_deref(),
                first.id(),
                velum::photos("heroAspect", 2.0),
                480,
            );
            hero.add_css_class("hero");
            let caption = gtk::Box::builder()
                .orientation(gtk::Orientation::Vertical)
                .halign(gtk::Align::Start)
                .valign(gtk::Align::End)
                .css_classes(["hero-caption"])
                .can_target(false)
                .build();
            let place = memory_place(memory);
            caption.append(&label(&place, "caption-title"));
            caption.append(&label(&items_word(memory.count), "caption-sub"));
            let overlay = gtk::Overlay::new();
            overlay.set_child(Some(&hero));
            overlay.add_overlay(&caption);
            card.append(&overlay);
        }
        let count = velum::photos("strip", 6.0) as usize;
        let rest: Vec<_> = memory.photos.iter().skip(1).take(count).collect();
        if !rest.is_empty() {
            let strip = strip(count);
            for photo in rest {
                strip.append(&self.photo(photo.thumb_path.as_deref(), photo.id(), 1.0, 72));
            }
            pad_strip(&strip, count);
            card.append(&strip);
        }
        card
    }

    fn recent_card(&self, data: &Data) -> gtk::Box {
        let card = card();
        card.add_css_class("days-card");
        let link = link_button("Open Library", self.emit(Event::Library));
        card.append(&card_head("Recent Days", Some(&link)));
        card.append(&label("Newest first, as in the Library", "card-sub"));
        let count = velum::photos("recentStrip", 5.0) as usize;
        // Ein Gitter statt Zeilen: die Spalte der Tage ist so breit wie ihr
        // längster Name (bricht erst sehr lang um), die Streifen stehen in
        // jeder Zeile gleich.
        let rows = gtk::Grid::builder().css_classes(["day-rows"]).build();
        for (index, day) in data.recent.iter().enumerate() {
            let line = (index * 2) as i32;
            if index > 0 {
                let rule = gtk::Separator::builder()
                    .orientation(gtk::Orientation::Horizontal)
                    .css_classes(["day-rule"])
                    .build();
                rows.attach(&rule, 0, line - 1, 2, 1);
            }
            let texts = gtk::Box::builder()
                .orientation(gtk::Orientation::Vertical)
                .valign(gtk::Align::Center)
                .css_classes(["day-label"])
                .build();
            let name = label(&day_title(day.day, data.today), "day-title");
            name.set_wrap(true);
            name.set_max_width_chars(22);
            let title = gtk::Button::builder()
                .child(&name)
                .css_classes(["flat", "day-link"])
                .build();
            let emit = self.emit(Event::Day(day.day));
            title.connect_clicked(move |_| emit());
            texts.append(&title);
            let sub = match &day.place {
                Some(place) => format!("{} · {}", town(place), items_word(day.count)),
                None => items_word(day.count),
            };
            let sub = label(&sub, "card-sub");
            sub.set_wrap(true);
            sub.set_max_width_chars(22);
            texts.append(&sub);
            rows.attach(&texts, 0, line, 1, 1);
            let strip = strip(count);
            strip.set_hexpand(true);
            strip.add_css_class("day-strip");
            for photo in day.photos.iter().take(count) {
                strip.append(&self.photo(photo.thumb_path.as_deref(), photo.id(), 1.0, 64));
            }
            pad_strip(&strip, count);
            rows.attach(&strip, 1, line, 1, 1);
        }
        card.append(&rows);
        if data.recent.is_empty() {
            card.append(&label("No photos yet.", "card-sub"));
        }
        card
    }

    fn add_card(&self) -> gtk::Box {
        let card = card();
        card.add_css_class("add-card");
        card.set_orientation(gtk::Orientation::Horizontal);
        let glyph = crate::icons::icon("plus", crate::icons::Weight::Regular);
        glyph.add_css_class("add-glyph");
        glyph.set_valign(gtk::Align::Center);
        let texts = gtk::Box::new(gtk::Orientation::Vertical, 0);
        texts.set_valign(gtk::Align::Center);
        texts.append(&label("Add Photos", "add-title"));
        let sub = label(
            "Drop photos and videos anywhere, or Ctrl+Shift+I",
            "card-sub",
        );
        sub.set_wrap(true);
        texts.append(&sub);
        card.append(&glyph);
        card.append(&texts);
        let click = gtk::GestureClick::new();
        let emit = self.emit(Event::AddPhotos);
        click.connect_released(move |_, _, _, _| emit());
        card.add_controller(click);
        card.set_cursor_from_name(Some("pointer"));
        card.set_tooltip_text(Some("Add Photos… (Ctrl+Shift+I)"));
        card
    }

    fn pinned_card(&self, data: &Data) -> Option<gtk::Box> {
        let favorites = data
            .albums
            .iter()
            .find(|a| a.id == "Favorites" && a.count > 0);
        let card = card();
        card.add_css_class("pinned-card");
        card.append(&card_head("Pinned", None));
        card.append(&label("Favorites, places and videos", "card-sub"));
        let row = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .homogeneous(true)
            .css_classes(["pins"])
            .build();
        if let Some(favorites) = favorites {
            let grid = gtk::Grid::builder()
                .row_homogeneous(true)
                .column_homogeneous(true)
                .css_classes(["quad"])
                .build();
            for (index, path) in data.favorites.iter().take(4).enumerate() {
                grid.attach(
                    &thumb(Some(path), 1.0, 40),
                    (index % 2) as i32,
                    (index / 2) as i32,
                    1,
                    1,
                );
            }
            let cover = RatioBox::new(&grid, 1.0, 96);
            cover.add_overlay(&badge("heart"));
            row.append(&self.pin(
                &cover,
                "Favorites",
                &photos_word(favorites.count),
                Event::Album(favorites.id.clone()),
            ));
        }
        if data.located > 0 {
            let cover = thumb(data.map_cover.as_deref(), 1.0, 96);
            cover.add_overlay(&badge("network"));
            row.append(&self.pin(&cover, "Map", &photos_word(data.located), Event::Map));
        }
        if data.videos > 0 {
            let cover = thumb(data.video_cover.as_deref(), 1.0, 96);
            cover.add_overlay(&badge("play"));
            let videos = data.albums.iter().find(|a| a.id == "Videos");
            let event = match videos {
                Some(album) => Event::Album(album.id.clone()),
                None => Event::Library,
            };
            row.append(&self.pin(&cover, "Videos", &videos_word(data.videos), event));
        }
        row.first_child()?;
        card.append(&row);
        Some(card)
    }

    fn pin(&self, cover: &RatioBox, title: &str, count: &str, event: Event) -> gtk::Button {
        cover.add_css_class("pin-cover");
        let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
        content.append(cover);
        content.append(&label(title, "pin-title"));
        content.append(&label(count, "card-sub"));
        let button = gtk::Button::builder()
            .child(&content)
            .css_classes(["flat", "tile-button"])
            .build();
        let emit = self.emit(event);
        button.connect_clicked(move |_| emit());
        button
    }

    fn albums_section(&self, albums: &[Album]) {
        let top = children(albums, None);
        if top.is_empty() {
            return;
        }
        let folders = top.iter().filter(|a| a.kind == AlbumKind::Folder).count();
        let plain = top.len() - folders;
        let mut summary = format!("{plain} {}", if plain == 1 { "album" } else { "albums" });
        if folders > 0 {
            summary.push_str(&format!(
                " · {folders} {}",
                if folders == 1 { "folder" } else { "folders" }
            ));
        }
        let link = link_button("Collections", self.emit(Event::Collections));
        self.content
            .append(&section_head("Albums", &summary, Some(&link)));

        let flow = gtk::FlowBox::builder()
            .homogeneous(true)
            .selection_mode(gtk::SelectionMode::None)
            .max_children_per_line(12)
            .min_children_per_line(2)
            .row_spacing(velum::photos("albumGap", 16.0) as u32)
            .column_spacing(velum::photos("cardGap", 12.0) as u32)
            .css_classes(["album-grid"])
            .build();
        let min = velum::photos("albumMin", 132.0) as i32;
        for album in top {
            let (cover, sub, event) = if album.kind == AlbumKind::Folder {
                let inner = children(albums, Some(&album.id));
                let grid = gtk::Grid::builder()
                    .row_homogeneous(true)
                    .column_homogeneous(true)
                    .css_classes(["folder-cover"])
                    .build();
                for (index, child) in inner.iter().take(4).enumerate() {
                    let tile = thumb(child.cover.as_deref(), 1.0, 40);
                    grid.attach(&tile, (index % 2) as i32, (index / 2) as i32, 1, 1);
                }
                let count = inner.len();
                (
                    RatioBox::new(&grid, 1.0, min),
                    format!("{count} {}", if count == 1 { "album" } else { "albums" }),
                    Event::Folder(album.id.clone()),
                )
            } else {
                (
                    thumb(album.cover.as_deref(), 1.0, min),
                    photos_word(album.count),
                    Event::Album(album.id.clone()),
                )
            };
            cover.add_css_class("album-cover-large");
            let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
            content.append(&cover);
            let name = label(&display_name(album), "album-name");
            name.set_ellipsize(gtk::pango::EllipsizeMode::End);
            content.append(&name);
            content.append(&label(&sub, "card-sub"));
            let button = gtk::Button::builder()
                .child(&content)
                .css_classes(["flat", "tile-button"])
                .width_request(min)
                .build();
            let emit = self.emit(event);
            button.connect_clicked(move |_| emit());
            flow.append(&button);
        }
        self.content.append(&flow);
    }

    fn media_section(&self, albums: &[Album]) {
        let media = media_types(albums);
        if media.is_empty() {
            return;
        }
        self.content.append(&section_head("Media Types", "", None));
        let flow = gtk::FlowBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .max_children_per_line(12)
            .row_spacing(velum::photos("chipGap", 6.0) as u32)
            .column_spacing(velum::photos("chipGap", 6.0) as u32)
            .css_classes(["chips"])
            .build();
        for album in media {
            let content = gtk::Box::new(gtk::Orientation::Horizontal, 0);
            let (name, fallback) = glyph_name(album);
            content.append(&crate::icons::glyph(
                name,
                crate::icons::Weight::Regular,
                fallback,
            ));
            content.append(&label(&display_name(album), "chip-name"));
            content.append(&label(&thousands(album.count), "chip-count"));
            let chip = gtk::Button::builder()
                .child(&content)
                .css_classes(["chip"])
                .build();
            let emit = self.emit(Event::Album(album.id.clone()));
            chip.connect_clicked(move |_| emit());
            flow.append(&chip);
        }
        self.content.append(&flow);
    }

    /// Ein Foto als Knopf: öffnet es im Betrachter.
    fn photo(&self, path: Option<&Path>, id: &str, ratio: f64, natural: i32) -> gtk::Button {
        let tile = thumb(path, ratio, natural);
        let button = gtk::Button::builder()
            .child(&tile)
            .css_classes(["flat", "photo-button"])
            .build();
        let emit = self.emit(Event::Photo(id.to_string()));
        button.connect_clicked(move |_| emit());
        button
    }
}

/// Eine Vorschau mit festem Seitenverhältnis, beschnitten wie im Raster.
pub fn thumb(path: Option<&Path>, ratio: f64, natural: i32) -> RatioBox {
    let picture = gtk::Picture::builder()
        .content_fit(gtk::ContentFit::Cover)
        .build();
    if let Some(path) = path {
        crate::window::set_image(&picture, &path.to_string_lossy());
    }
    let tile = RatioBox::new(&picture, ratio, natural);
    tile.add_css_class("thumb");
    tile
}

fn badge(icon: &str) -> gtk::Widget {
    let badge = crate::icons::icon(icon, crate::icons::Weight::Fill);
    badge.set_halign(gtk::Align::Start);
    badge.set_valign(gtk::Align::End);
    badge.add_css_class("cover-badge");
    badge
}

fn card() -> gtk::Box {
    gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .css_classes(["home-card"])
        .build()
}

fn column() -> gtk::Box {
    gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .css_classes(["home-column"])
        .build()
}

fn strip(count: usize) -> gtk::Box {
    let strip = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .homogeneous(true)
        .css_classes(["strip"])
        .build();
    strip.set_widget_name(&count.to_string());
    strip
}

/// Füllt einen Streifen mit leeren Plätzen auf, damit die Vorschauen gleich
/// groß bleiben, auch wenn ein Tag weniger Fotos hat.
fn pad_strip(strip: &gtk::Box, count: usize) {
    let mut have = 0;
    let mut child = strip.first_child();
    while let Some(widget) = child {
        have += 1;
        child = widget.next_sibling();
    }
    for _ in have..count {
        strip.append(&RatioBox::new(
            &gtk::Box::new(gtk::Orientation::Vertical, 0),
            1.0,
            64,
        ));
    }
}

pub fn label(text: &str, class: &str) -> gtk::Label {
    gtk::Label::builder()
        .label(text)
        .xalign(0.0)
        .css_classes([class])
        .build()
}

fn link_button(text: &str, on_click: impl Fn() + 'static) -> gtk::Button {
    let button = gtk::Button::builder()
        .label(text)
        .css_classes(["flat", "more-link"])
        .valign(gtk::Align::Center)
        .build();
    button.connect_clicked(move |_| on_click());
    button
}

fn card_head(title: &str, link: Option<&gtk::Button>) -> gtk::Box {
    let head = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .css_classes(["card-head"])
        .build();
    let title = label(title, "card-title");
    title.set_hexpand(true);
    head.append(&title);
    if let Some(link) = link {
        head.append(link);
    }
    head
}

fn section_head(title: &str, summary: &str, link: Option<&gtk::Button>) -> gtk::Box {
    let head = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .css_classes(["section-head"])
        .build();
    head.append(&label(title, "section-title"));
    let summary = label(summary, "section-summary");
    summary.set_hexpand(true);
    summary.set_valign(gtk::Align::Baseline);
    head.append(&summary);
    if let Some(link) = link {
        head.append(link);
    }
    head
}

/// Der Ort einer Erinnerung: die Stadt, falls bekannt — sonst das Jahr.
fn memory_place(memory: &Memory) -> String {
    match &memory.place {
        Some(place) => town(place).to_string(),
        None => memory.day.year().to_string(),
    }
}

/// „Markt, Leipzig" → „Leipzig": die Stadt reicht für eine Zeile.
pub fn town(place: &str) -> &str {
    place.rsplit(',').next().map(str::trim).unwrap_or(place)
}

/// „October 29, 2025"
pub fn long_date(day: NaiveDate) -> String {
    format!("{} {}, {}", month_name(day.month()), day.day(), day.year())
}

/// „Today", „Yesterday", „Tuesday, October 27" (mit Jahr, wenn es ein anderes ist).
pub fn day_title(day: NaiveDate, today: Option<NaiveDate>) -> String {
    if let Some(today) = today {
        if day == today {
            return "Today".to_string();
        }
        if Some(day) == today.pred_opt() {
            return "Yesterday".to_string();
        }
    }
    let weekday = match day.weekday() {
        chrono::Weekday::Mon => "Monday",
        chrono::Weekday::Tue => "Tuesday",
        chrono::Weekday::Wed => "Wednesday",
        chrono::Weekday::Thu => "Thursday",
        chrono::Weekday::Fri => "Friday",
        chrono::Weekday::Sat => "Saturday",
        chrono::Weekday::Sun => "Sunday",
    };
    let year = match today {
        Some(today) if today.year() == day.year() => String::new(),
        _ => format!(", {}", day.year()),
    };
    format!("{weekday}, {} {}{year}", month_name(day.month()), day.day())
}

pub fn items_word(count: u64) -> String {
    format!(
        "{} {}",
        thousands(count),
        if count == 1 { "item" } else { "items" }
    )
}

pub fn photos_word(count: u64) -> String {
    format!(
        "{} {}",
        thousands(count),
        if count == 1 { "photo" } else { "photos" }
    )
}

pub fn videos_word(count: u64) -> String {
    format!(
        "{} {}",
        thousands(count),
        if count == 1 { "video" } else { "videos" }
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn day(raw: &str) -> NaiveDate {
        NaiveDate::parse_from_str(raw, "%Y-%m-%d").unwrap()
    }

    #[test]
    fn day_titles_read_like_photos() {
        let today = Some(day("2026-10-29"));
        assert_eq!(day_title(day("2026-10-29"), today), "Today");
        assert_eq!(day_title(day("2026-10-28"), today), "Yesterday");
        assert_eq!(day_title(day("2026-10-27"), today), "Tuesday, October 27");
        assert_eq!(
            day_title(day("2025-12-24"), today),
            "Wednesday, December 24, 2025"
        );
    }

    #[test]
    fn towns_are_the_last_part_of_a_place() {
        assert_eq!(town("Markt, Leipzig"), "Leipzig");
        assert_eq!(town("Monaco"), "Monaco");
    }

    #[test]
    fn counts_have_words() {
        assert_eq!(items_word(1), "1 item");
        assert_eq!(photos_word(19_624), "19,624 photos");
        assert_eq!(videos_word(2), "2 videos");
    }
}
