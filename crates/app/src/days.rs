//! „Days" — die Bibliothek nach Tagen, mit einem Kopf je Tag (Lab
//! `photos.html?p=a&s=library`, die Vorgabe dort).
//!
//! `GtkGridView` kennt keine Abschnittsköpfe, und eine `GtkListView` aus
//! Tagen stockte bei schnellem Scrollen: ihre Zeilen sind verschieden hoch,
//! sie schätzt die Höhe der ungebundenen und band bei großen Sprüngen jedes
//! Bild einen Großteil ihrer 200 Zeilen neu (gemessen an 15.000 Fotos bei
//! 120 px je Bild: ~20.000 Bindungen, im Mittel 55 ms je Bild).
//!
//! Deshalb ein eigenes, scrollbares Widget (`DaysCanvas`): die Höhe jedes Tages
//! ist ausgerechnet — Kopf plus Reihen aus gleich großen Kacheln —, also ist
//! die ganze Seite ohne ein einziges Widget bekannt. Gebaut werden nur die
//! Tage im Sichtfeld und einen Bildschirm darüber und darunter; was
//! hinausscrollt, wird abgebaut. Eine Bibliothek beliebiger Größe hält damit
//! ein paar Dutzend Tage und ein paar hundert Vorschauen.
//!
//! Positionen sind die des Rasters (`Catalog::page_in`): Tag + Index. Damit
//! teilen sich beide Ansichten Modell und Auswahl — Aktionsleiste, Menü,
//! Kürzel und Betrachter arbeiten in „Days" wie im Raster, nur dass vor einer
//! Wahl das Modell bis dorthin nachgeladen wird.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

use chrono::NaiveDate;
use gtk::prelude::*;
use gtk::subclass::prelude::*;
use gtk::{glib, graphene, gsk};
use icloud_photos_cache::DayGroup;

use crate::icons::{self, Weight};
use crate::layout::{RatioBox, TileGrid};

/// Was ein Klick auf eine Kachel will.
#[derive(Debug, Clone, Copy)]
pub enum Click {
    /// Einfach: wählen (im Auswahlmodus dazu oder heraus).
    Select {
        position: u32,
        toggle: bool,
        range: bool,
    },
    /// Doppelt: öffnen.
    Open(u32),
    /// Rechts: Menü an der Stelle.
    Menu { position: u32, x: f64, y: f64 },
    /// „Select" im Kopf eines Tages (Auswahlmodus): alle Fotos des Tages.
    Day { offset: u32, count: u32 },
}

type Handler = Rc<dyn Fn(Click, &gtk::Widget)>;
/// Liest Fotos: (Rasterposition, Anzahl) → Fotos.
type Loader = Rc<dyn Fn(u64, u64) -> Vec<icloud_photos_core::model::Photo>>;
type Query = Rc<dyn Fn(u32) -> bool>;
type Favorite = Rc<dyn Fn(&str) -> bool>;

pub struct Hooks {
    pub click: Handler,
    pub load: Loader,
    pub selected: Query,
    pub favorite: Favorite,
    pub selecting: Rc<dyn Fn() -> bool>,
    pub today: Rc<dyn Fn() -> NaiveDate>,
}

thread_local! {
    /// Messung (`ICLOUD_PHOTOS_BENCH`): Zeit im Layout (µs), —, gebaute Tage.
    pub static COST: Cell<(u64, u64, u64)> = const { Cell::new((0, 0, 0)) };
}

// ── Das scrollbare Widget ─────────────────────────────────────────────────

type Allocate = Box<dyn Fn(i32, i32)>;

mod imp {
    use super::*;

    #[derive(glib::Properties)]
    #[properties(wrapper_type = super::DaysCanvas)]
    pub struct DaysCanvas {
        #[property(override_interface = gtk::Scrollable, get, set = Self::set_hadjustment, nullable)]
        pub hadjustment: RefCell<Option<gtk::Adjustment>>,
        #[property(override_interface = gtk::Scrollable, get, set = Self::set_vadjustment, nullable)]
        pub vadjustment: RefCell<Option<gtk::Adjustment>>,
        #[property(override_interface = gtk::Scrollable, get, set, builder(gtk::ScrollablePolicy::Minimum))]
        pub hscroll_policy: Cell<gtk::ScrollablePolicy>,
        #[property(override_interface = gtk::Scrollable, get, set, builder(gtk::ScrollablePolicy::Minimum))]
        pub vscroll_policy: Cell<gtk::ScrollablePolicy>,
        pub allocate: RefCell<Option<Allocate>>,
        pub value_handler: RefCell<Option<glib::SignalHandlerId>>,
    }

    impl Default for DaysCanvas {
        fn default() -> Self {
            Self {
                hadjustment: RefCell::new(None),
                vadjustment: RefCell::new(None),
                hscroll_policy: Cell::new(gtk::ScrollablePolicy::Minimum),
                vscroll_policy: Cell::new(gtk::ScrollablePolicy::Minimum),
                allocate: RefCell::new(None),
                value_handler: RefCell::new(None),
            }
        }
    }

    impl DaysCanvas {
        fn set_hadjustment(&self, adjustment: Option<gtk::Adjustment>) {
            self.hadjustment.replace(adjustment);
        }

        fn set_vadjustment(&self, adjustment: Option<gtk::Adjustment>) {
            let old = self.vadjustment.borrow().clone();
            if let (Some(old), Some(id)) = (old, self.value_handler.borrow_mut().take()) {
                old.disconnect(id);
            }
            if let Some(adjustment) = &adjustment {
                let widget = self.obj().downgrade();
                let id = adjustment.connect_value_changed(move |_| {
                    if let Some(widget) = widget.upgrade() {
                        widget.queue_allocate();
                    }
                });
                self.value_handler.replace(Some(id));
            }
            self.vadjustment.replace(adjustment);
            self.obj().queue_allocate();
        }
    }

    #[glib::object_subclass]
    impl ObjectSubclass for DaysCanvas {
        const NAME: &'static str = "PhotosDaysCanvas";
        type Type = super::DaysCanvas;
        type ParentType = gtk::Widget;
        type Interfaces = (gtk::Scrollable,);

        fn class_init(klass: &mut Self::Class) {
            klass.set_css_name("dayscanvas");
        }
    }

    #[glib::derived_properties]
    impl ObjectImpl for DaysCanvas {
        fn dispose(&self) {
            while let Some(child) = self.obj().first_child() {
                child.unparent();
            }
        }
    }

    impl WidgetImpl for DaysCanvas {
        fn measure(&self, _orientation: gtk::Orientation, _for_size: i32) -> (i32, i32, i32, i32) {
            (0, 0, -1, -1)
        }

        fn size_allocate(&self, width: i32, height: i32, _baseline: i32) {
            let callback = self.allocate.borrow();
            if let Some(callback) = callback.as_ref() {
                callback(width, height);
            }
        }
    }

    impl ScrollableImpl for DaysCanvas {}
}

glib::wrapper! {
    /// Die Seite aus Tagen; scrollt in einem `GtkScrolledWindow`.
    pub struct DaysCanvas(ObjectSubclass<imp::DaysCanvas>)
        @extends gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget, gtk::Scrollable;
}

// ── Ein gebauter Tag ───────────────────────────────────────────────────────

struct Section {
    widget: gtk::Box,
    select: gtk::Button,
    /// Die Kacheln mit ihrer Rasterposition.
    tiles: Vec<(u32, gtk::Overlay)>,
}

/// Wo die Tage stehen, ausgerechnet für eine Breite.
#[derive(Default)]
struct Plan {
    width: i32,
    tile: i32,
    head: i32,
    days: usize,
    /// Oberkante und Höhe je Tag, die Höhe der ganzen Seite.
    tops: Vec<f64>,
    heights: Vec<f64>,
    total: f64,
}

pub struct DaysView {
    pub widget: gtk::ScrolledWindow,
    /// Das Widget mit den Tagen (Elternteil des Kontextmenüs).
    pub list: gtk::Widget,
    canvas: DaysCanvas,
    hooks: Rc<Hooks>,
    groups: RefCell<Vec<DayGroup>>,
    built: RefCell<HashMap<usize, Section>>,
    plan: RefCell<Plan>,
    tile_size: Cell<i32>,
    /// Höhe eines Tageskopfs, gemessen am ersten gebauten (Schätzung bis dahin).
    head_height: Cell<i32>,
    selecting: Cell<bool>,
}

impl DaysView {
    pub fn new(hooks: Hooks, tile: i32) -> Rc<Self> {
        let canvas: DaysCanvas = glib::Object::new();
        canvas.add_css_class("days-list");
        canvas.set_overflow(gtk::Overflow::Hidden);
        let widget = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .child(&canvas)
            .css_classes(["photos-pane", "days-pane"])
            .build();
        let view = Rc::new(Self {
            widget,
            list: canvas.clone().upcast(),
            canvas: canvas.clone(),
            hooks: Rc::new(hooks),
            groups: RefCell::new(Vec::new()),
            built: RefCell::new(HashMap::new()),
            plan: RefCell::new(Plan::default()),
            tile_size: Cell::new(tile),
            head_height: Cell::new(40),
            selecting: Cell::new(false),
        });
        let weak = Rc::downgrade(&view);
        canvas
            .imp()
            .allocate
            .replace(Some(Box::new(move |width, height| {
                if let Some(view) = weak.upgrade() {
                    view.allocate(width, height);
                }
            })));
        view
    }

    /// Seitenrand links/rechts (wie das Raster, Velum `space.12`).
    fn inset() -> i32 {
        crate::velum::photos("daysInset", 12.0) as i32
    }

    fn gap() -> i32 {
        crate::velum::photos("tileGap", 2.0) as i32
    }

    /// Rechnet die Lage aller Tage für diese Breite.
    fn plan(&self, width: i32) {
        let gap = Self::gap();
        let tile = self.tile_size.get().max(1);
        let inner = (width - 2 * Self::inset()).max(tile);
        let columns = ((inner + gap) / (tile + gap)).max(1);
        let size = (inner - gap * (columns - 1)) / columns;
        let head = self.head_height.get();
        let groups = self.groups.borrow();
        let mut tops = Vec::with_capacity(groups.len());
        let mut heights = Vec::with_capacity(groups.len());
        let mut y = 0.0;
        for group in groups.iter() {
            let n = group.count as i32;
            let rows = (n + columns - 1) / columns;
            let height = f64::from(head + rows * size + (rows - 1).max(0) * gap);
            tops.push(y);
            heights.push(height);
            y += height;
        }
        *self.plan.borrow_mut() = Plan {
            width,
            tile,
            head,
            days: groups.len(),
            tops,
            heights,
            total: y + crate::velum::photos("daysBottom", 16.0),
        };
    }

    fn allocate(&self, width: i32, height: i32) {
        let start = std::time::Instant::now();
        let stale = {
            let plan = self.plan.borrow();
            plan.width != width
                || plan.tile != self.tile_size.get()
                || plan.head != self.head_height.get()
                || plan.days != self.groups.borrow().len()
        };
        if stale {
            self.plan(width);
        }
        let Some(adjustment) = self.canvas.vadjustment() else {
            return;
        };
        let page = f64::from(height.max(1));
        let total = self.plan.borrow().total.max(page);
        let value = adjustment.value().clamp(0.0, (total - page).max(0.0));
        if adjustment.upper() != total || adjustment.page_size() != page {
            adjustment.configure(value, 0.0, total, page * 0.1, page * 0.9, page);
        }

        // Welche Tage zu sehen sind, samt einem Bildschirm Rand.
        let (first, last) = {
            let plan = self.plan.borrow();
            let first = plan
                .tops
                .partition_point(|top| *top < value - page)
                .saturating_sub(1);
            let last = plan.tops.partition_point(|top| *top < value + 2.0 * page);
            (first, last)
        };
        let mut built = self.built.borrow_mut();
        built.retain(|index, section| {
            let keep = *index >= first && *index < last;
            if !keep {
                section.widget.unparent();
            }
            keep
        });
        let inset = Self::inset();
        let inner = (width - 2 * inset).max(1);
        let mut new_head: Option<i32> = None;
        for index in first..last {
            if built.contains_key(&index) {
                continue;
            }
            let Some(group) = self.groups.borrow().get(index).cloned() else {
                continue;
            };
            let section = self.build_section(&group);
            section.widget.set_parent(&self.canvas);
            if new_head.is_none()
                && let Some(head) = section.widget.first_child()
            {
                new_head = Some(head.measure(gtk::Orientation::Vertical, inner).1);
            }
            built.insert(index, section);
            COST.with(|cost| {
                let (micros, load, count) = cost.get();
                cost.set((micros, load, count + 1));
            });
        }
        {
            let plan = self.plan.borrow();
            for (index, section) in built.iter() {
                let (Some(top), Some(h)) = (plan.tops.get(*index), plan.heights.get(*index)) else {
                    continue;
                };
                let _ = section.widget.measure(gtk::Orientation::Vertical, inner);
                let transform = gsk::Transform::new()
                    .translate(&graphene::Point::new(inset as f32, (top - value) as f32));
                section
                    .widget
                    .allocate(inner, *h as i32, -1, Some(transform));
            }
        }
        drop(built);
        // Der erste echte Kopf ist anders hoch als geschätzt: neu rechnen.
        if let Some(head) = new_head
            && head > 0
            && head != self.head_height.get()
        {
            self.head_height.set(head);
            self.canvas.queue_allocate();
        }
        COST.with(|cost| {
            let (micros, load, count) = cost.get();
            cost.set((micros + start.elapsed().as_micros() as u64, load, count));
        });
    }

    /// Baut einen Tag: Kopf und Kacheln samt Bildern (gebaut werden ohnehin
    /// nur Tage nahe am Sichtfeld).
    fn build_section(&self, group: &DayGroup) -> Section {
        let hooks = &self.hooks;
        let title = gtk::Label::builder()
            .xalign(0.0)
            .css_classes(["day-head-title"])
            .label(match group.day {
                Some(day) => crate::home::day_title(day, Some((hooks.today)())),
                None => group.key.clone(),
            })
            .build();
        let items = crate::home::items_word(group.count);
        let sub = gtk::Label::builder()
            .xalign(0.0)
            .hexpand(true)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .css_classes(["day-head-sub"])
            .label(match &group.place {
                Some(place) => format!("{} · {items}", crate::home::town(place)),
                None => items,
            })
            .build();
        let select = gtk::Button::builder()
            .label("Select")
            .css_classes(["flat", "more-link"])
            .visible(self.selecting.get())
            .build();
        let (offset, count) = (group.offset as u32, group.count as u32);
        select.connect_clicked({
            let hooks = hooks.clone();
            move |button| (hooks.click)(Click::Day { offset, count }, button.upcast_ref())
        });
        let head = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .css_classes(["day-head"])
            .build();
        head.append(&title);
        head.append(&sub);
        head.append(&select);
        let grid = TileGrid::new(self.tile_size.get(), Self::gap());
        grid.set_overflow(gtk::Overflow::Hidden);
        grid.add_css_class("day-grid");
        let mut tiles = Vec::new();
        for (index, photo) in (hooks.load)(group.offset, group.count).iter().enumerate() {
            let position = offset + index as u32;
            let tile = build_tile(hooks, position, photo, self.tile_size.get());
            grid.append(&tile);
            tiles.push((position, tile));
        }
        let widget = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .css_classes(["day-section"])
            .build();
        widget.append(&head);
        widget.append(&grid);
        Section {
            widget,
            select,
            tiles,
        }
    }

    fn clear(&self) {
        for (_, section) in self.built.borrow_mut().drain() {
            section.widget.unparent();
        }
    }

    /// Übernimmt die Tage; unverändert bleibt die Seite, wie sie ist.
    pub fn show(&self, groups: Vec<DayGroup>) {
        if *self.groups.borrow() == groups {
            return;
        }
        *self.groups.borrow_mut() = groups;
        self.clear();
        self.plan.borrow_mut().width = -1;
        self.canvas.queue_allocate();
    }

    /// Springt zum Tag, der diese Rasterposition enthält.
    pub fn scroll_to_offset(&self, offset: u64) {
        let index = {
            let groups = self.groups.borrow();
            match groups.binary_search_by(|g| g.offset.cmp(&offset)) {
                Ok(index) => index,
                Err(index) => index.saturating_sub(1),
            }
        };
        let top = self.plan.borrow().tops.get(index).copied();
        if let (Some(top), Some(adjustment)) = (top, self.canvas.vadjustment()) {
            adjustment.set_value(top);
        }
    }

    /// Auswahl und Favoriten der gebauten Kacheln nachführen.
    pub fn refresh_marks(&self, selected: &dyn Fn(u32) -> bool, favorite: &dyn Fn(&str) -> bool) {
        for section in self.built.borrow().values() {
            for (position, tile) in &section.tiles {
                if selected(*position) {
                    tile.add_css_class("selected");
                } else {
                    tile.remove_css_class("selected");
                }
                let id = tile.widget_name();
                if let Some(heart) = tile.last_child() {
                    heart.set_visible(favorite(&id));
                }
            }
        }
    }

    /// Der Auswahlmodus: Ringe an, „Select" in jedem Tageskopf.
    pub fn set_selecting(&self, on: bool) {
        self.selecting.set(on);
        if on {
            self.canvas.add_css_class("selecting");
        } else {
            self.canvas.remove_css_class("selecting");
        }
        for section in self.built.borrow().values() {
            section.select.set_visible(on);
        }
    }

    /// Neue Kachelgröße (Pinch, Tablet-Modus): neu rechnen und bauen.
    pub fn set_tile_size(&self, size: i32) {
        if self.tile_size.replace(size) != size {
            self.clear();
            self.canvas.queue_allocate();
        }
    }
}

/// Eine Kachel: das Bild, Video- und Favoriten-Kennzeichen, der Ring der
/// Auswahl. Klicks gehen mit der Rasterposition an den Handler.
fn build_tile(
    hooks: &Rc<Hooks>,
    position: u32,
    photo: &icloud_photos_core::model::Photo,
    size: i32,
) -> gtk::Overlay {
    let picture = gtk::Picture::builder()
        .content_fit(gtk::ContentFit::Cover)
        .build();
    if let Some(path) = &photo.thumb_path {
        crate::window::set_image(&picture, &path.to_string_lossy());
    }
    let frame = RatioBox::new(&picture, 1.0, size);
    frame.add_css_class("day-photo");
    let overlay = gtk::Overlay::builder()
        .child(&frame)
        .css_classes(["day-tile"])
        .build();
    overlay.set_widget_name(photo.id());
    overlay.set_tooltip_text(Some(&photo.record.filename));
    if photo.record.is_video() {
        let badge = icons::glyph("play", Weight::Fill, "media-playback-start-symbolic");
        badge.add_css_class("photo-badge");
        badge.set_halign(gtk::Align::End);
        badge.set_valign(gtk::Align::End);
        badge.set_can_target(false);
        overlay.add_overlay(&badge);
    }
    let check = icons::glyph("check", Weight::Bold, "object-select-symbolic");
    check.add_css_class("photo-check");
    check.set_halign(gtk::Align::End);
    check.set_valign(gtk::Align::Start);
    check.set_can_target(false);
    overlay.add_overlay(&check);
    // Zuletzt: `refresh_marks` findet das Herz als letztes Kind.
    let heart = icons::glyph("heart", Weight::Fill, "starred-symbolic");
    heart.add_css_class("photo-heart");
    heart.set_halign(gtk::Align::Start);
    heart.set_valign(gtk::Align::End);
    heart.set_can_target(false);
    heart.set_visible((hooks.favorite)(photo.id()));
    overlay.add_overlay(&heart);
    if (hooks.selected)(position) {
        overlay.add_css_class("selected");
    }

    let click = gtk::GestureClick::builder().button(0).build();
    click.connect_pressed({
        let hooks = hooks.clone();
        let overlay = overlay.downgrade();
        move |gesture, presses, x, y| {
            let Some(overlay) = overlay.upgrade() else {
                return;
            };
            gesture.set_state(gtk::EventSequenceState::Claimed);
            let state = gesture.current_event_state();
            let click = match gesture.current_button() {
                gtk::gdk::BUTTON_SECONDARY => Click::Menu { position, x, y },
                _ if presses >= 2 && !(hooks.selecting)() => Click::Open(position),
                _ => Click::Select {
                    position,
                    toggle: (hooks.selecting)()
                        || state.contains(gtk::gdk::ModifierType::CONTROL_MASK),
                    range: state.contains(gtk::gdk::ModifierType::SHIFT_MASK),
                },
            };
            (hooks.click)(click, overlay.upcast_ref());
        }
    });
    overlay.add_controller(click);
    overlay
}

/// Für Messungen: gebaute Kacheln und wie viele davon ein Bild halten.
pub fn live_tiles(view: &DaysView) -> (usize, usize) {
    let built = view.built.borrow();
    let tiles: Vec<&gtk::Overlay> = built
        .values()
        .flat_map(|section| section.tiles.iter().map(|(_, tile)| tile))
        .collect();
    let with_image = tiles
        .iter()
        .filter(|tile| {
            tile.child()
                .and_then(|frame| frame.first_child())
                .and_downcast::<gtk::Picture>()
                .is_some_and(|picture| picture.paintable().is_some())
        })
        .count();
    (tiles.len(), with_image)
}
