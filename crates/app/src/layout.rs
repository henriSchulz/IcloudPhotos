//! Zwei kleine Layout-Widgets, die GTK nicht mitbringt und die Startseite
//! braucht: eine Fläche mit festem Seitenverhältnis (`RatioBox`, CSS
//! `aspect-ratio`) und zwei Spalten, die sich ab einer Breite nebeneinander
//! stellen und darunter stapeln (`Columns`, CSS-Grid mit Container-Query).
//!
//! Beide sind „höhe aus breite" (`HeightForWidth`): erst steht die Breite
//! fest, daraus folgt die Höhe. Nur so wächst ein Titelbild mit der Karte.

use std::cell::Cell;

use gtk::prelude::*;
use gtk::subclass::prelude::*;
use gtk::{glib, graphene, gsk};

mod imp {
    use super::*;

    pub struct RatioBox {
        /// Breite durch Höhe.
        pub ratio: Cell<f64>,
        /// Wunschbreite, wenn niemand eine vorgibt.
        pub natural: Cell<i32>,
        /// Fest so breit wie `natural` (Titelbild in einer Zeile), statt
        /// mit dem Platz zu wachsen und zu schrumpfen.
        pub fixed: Cell<bool>,
        /// Mindestbreite — eine Kachel in einem `FlowBox`, das aus ihr die
        /// Spaltenzahl rechnet.
        pub min_width: Cell<i32>,
    }

    impl Default for RatioBox {
        fn default() -> Self {
            Self {
                ratio: Cell::new(1.0),
                natural: Cell::new(0),
                fixed: Cell::new(false),
                min_width: Cell::new(0),
            }
        }
    }

    #[glib::object_subclass]
    impl ObjectSubclass for RatioBox {
        const NAME: &'static str = "PhotosRatioBox";
        type Type = super::RatioBox;
        type ParentType = gtk::Widget;

        fn class_init(klass: &mut Self::Class) {
            klass.set_css_name("ratiobox");
        }
    }

    impl ObjectImpl for RatioBox {
        fn dispose(&self) {
            while let Some(child) = self.obj().first_child() {
                child.unparent();
            }
        }
    }

    impl WidgetImpl for RatioBox {
        fn request_mode(&self) -> gtk::SizeRequestMode {
            gtk::SizeRequestMode::HeightForWidth
        }

        fn measure(&self, orientation: gtk::Orientation, for_size: i32) -> (i32, i32, i32, i32) {
            let natural = self.natural.get();
            let fixed = self.fixed.get();
            match orientation {
                gtk::Orientation::Horizontal if fixed => (natural, natural, -1, -1),
                gtk::Orientation::Horizontal => {
                    let min = self.min_width.get();
                    (min, natural.max(min), -1, -1)
                }
                _ => {
                    let width = if for_size >= 0 && !fixed {
                        for_size
                    } else {
                        natural
                    };
                    let height = (f64::from(width) / self.ratio.get().max(0.01)).round() as i32;
                    // Kein Minimum: GtkBox misst Höhe-aus-Breite für Minimum
                    // und Wunsch mit verschiedenen Breiten — ein Minimum über
                    // dem Wunsch wäre ein Fehler. Gezeichnet wird in Spalten,
                    // die ohnehin die Wunschhöhe vergeben.
                    // Mit Mindestbreite (Kachel in „Days") gilt auch die
                    // Höhe als Minimum: Listenzeilen vergeben nur das.
                    let strict = fixed || self.min_width.get() > 0;
                    (if strict { height } else { 0 }, height, -1, -1)
                }
            }
        }

        fn size_allocate(&self, width: i32, height: i32, baseline: i32) {
            let mut child = self.obj().first_child();
            while let Some(widget) = child {
                widget.allocate(width, height, baseline, None);
                child = widget.next_sibling();
            }
        }
    }

    pub struct Columns {
        /// Ab dieser eigenen Breite zwei Spalten.
        pub threshold: Cell<i32>,
        /// Linke Spalte im Verhältnis zur rechten.
        pub lead: Cell<f64>,
        pub gap: Cell<i32>,
    }

    impl Default for Columns {
        fn default() -> Self {
            Self {
                threshold: Cell::new(728),
                lead: Cell::new(1.0),
                gap: Cell::new(12),
            }
        }
    }

    #[glib::object_subclass]
    impl ObjectSubclass for Columns {
        const NAME: &'static str = "PhotosColumns";
        type Type = super::Columns;
        type ParentType = gtk::Widget;

        fn class_init(klass: &mut Self::Class) {
            klass.set_css_name("columns");
        }
    }

    impl ObjectImpl for Columns {
        fn dispose(&self) {
            while let Some(child) = self.obj().first_child() {
                child.unparent();
            }
        }
    }

    impl Columns {
        fn children(&self) -> Vec<gtk::Widget> {
            let mut out = Vec::new();
            let mut child = self.obj().first_child();
            while let Some(widget) = child {
                child = widget.next_sibling();
                if widget.should_layout() {
                    out.push(widget);
                }
            }
            out
        }

        fn split(&self, width: i32) -> Option<(i32, i32)> {
            if width < self.threshold.get() {
                return None;
            }
            let room = (width - self.gap.get()).max(0);
            let lead = self.lead.get();
            let left = (f64::from(room) * lead / (lead + 1.0)).round() as i32;
            Some((left, room - left))
        }

        fn height(widget: &gtk::Widget, width: i32) -> (i32, i32) {
            let (min, nat, _, _) = widget.measure(gtk::Orientation::Vertical, width);
            (min, nat)
        }
    }

    impl WidgetImpl for Columns {
        fn request_mode(&self) -> gtk::SizeRequestMode {
            gtk::SizeRequestMode::HeightForWidth
        }

        fn measure(&self, orientation: gtk::Orientation, for_size: i32) -> (i32, i32, i32, i32) {
            let children = self.children();
            let gap = self.gap.get();
            match orientation {
                gtk::Orientation::Horizontal => {
                    let mut min = 0;
                    let mut nat = 0;
                    for (index, child) in children.iter().enumerate() {
                        let (m, n, _, _) = child.measure(orientation, -1);
                        min = min.max(m);
                        nat += n + if index > 0 { gap } else { 0 };
                    }
                    (min, nat.max(min), -1, -1)
                }
                _ => {
                    let width = if for_size >= 0 {
                        for_size
                    } else {
                        self.measure(gtk::Orientation::Horizontal, -1).1
                    };
                    match (self.split(width), children.as_slice()) {
                        (Some((left, right)), [a, b]) => {
                            let (am, an) = Self::height(a, left);
                            let (bm, bn) = Self::height(b, right);
                            (am.max(bm), an.max(bn), -1, -1)
                        }
                        _ => {
                            let mut min = 0;
                            let mut nat = 0;
                            for (index, child) in children.iter().enumerate() {
                                let (m, n) = Self::height(child, width);
                                let space = if index > 0 { gap } else { 0 };
                                min += m + space;
                                nat += n + space;
                            }
                            (min, nat, -1, -1)
                        }
                    }
                }
            }
        }

        fn size_allocate(&self, width: i32, height: i32, _baseline: i32) {
            let children = self.children();
            let place = |widget: &gtk::Widget, x: i32, y: i32, w: i32, h: i32| {
                let transform =
                    gsk::Transform::new().translate(&graphene::Point::new(x as f32, y as f32));
                widget.allocate(w, h, -1, Some(transform));
            };
            match (self.split(width), children.as_slice()) {
                (Some((left, right)), [a, b]) => {
                    // Jede Spalte so hoch, wie sie es braucht — die Karten
                    // sollen nicht auf die Höhe der Nachbarspalte gestreckt
                    // werden.
                    let ah = Self::height(a, left).1.min(height);
                    let bh = Self::height(b, right).1.min(height);
                    place(a, 0, 0, left, ah);
                    place(b, left + self.gap.get(), 0, right, bh);
                }
                _ => {
                    let mut y = 0;
                    for child in &children {
                        let h = Self::height(child, width).1;
                        place(child, 0, y, width, h);
                        y += h + self.gap.get();
                    }
                }
            }
        }
    }

    pub struct TileGrid {
        /// Kleinste Kachel; daraus folgt die Spaltenzahl.
        pub tile: Cell<i32>,
        pub gap: Cell<i32>,
    }

    impl Default for TileGrid {
        fn default() -> Self {
            Self {
                tile: Cell::new(120),
                gap: Cell::new(2),
            }
        }
    }

    #[glib::object_subclass]
    impl ObjectSubclass for TileGrid {
        const NAME: &'static str = "PhotosTileGrid";
        type Type = super::TileGrid;
        type ParentType = gtk::Widget;

        fn class_init(klass: &mut Self::Class) {
            klass.set_css_name("tilegrid");
        }
    }

    impl ObjectImpl for TileGrid {
        fn dispose(&self) {
            while let Some(child) = self.obj().first_child() {
                child.unparent();
            }
        }
    }

    impl TileGrid {
        fn count(&self) -> i32 {
            let mut n = 0;
            let mut child = self.obj().first_child();
            while let Some(widget) = child {
                n += 1;
                child = widget.next_sibling();
            }
            n
        }

        /// Spalten und Kantenlänge für eine Breite.
        fn layout(&self, width: i32) -> (i32, i32) {
            let gap = self.gap.get();
            let tile = self.tile.get().max(1);
            let columns = ((width + gap) / (tile + gap)).max(1);
            let size = (width - gap * (columns - 1)) / columns;
            (columns, size.max(1))
        }
    }

    impl WidgetImpl for TileGrid {
        fn request_mode(&self) -> gtk::SizeRequestMode {
            gtk::SizeRequestMode::HeightForWidth
        }

        // Ohne die Kinder zu fragen: alle Kacheln sind gleich groß und
        // quadratisch. Das hält das Messen billig — eine Liste aus Tagen misst
        // bei jedem Bild jede gebundene Zeile.
        fn measure(&self, orientation: gtk::Orientation, for_size: i32) -> (i32, i32, i32, i32) {
            let n = self.count();
            let gap = self.gap.get();
            match orientation {
                gtk::Orientation::Horizontal => {
                    let tile = self.tile.get();
                    (tile, (tile + gap) * n.max(1) - gap, -1, -1)
                }
                _ => {
                    if n == 0 {
                        return (0, 0, -1, -1);
                    }
                    let width = if for_size >= 0 {
                        for_size
                    } else {
                        self.tile.get() * n
                    };
                    let (columns, size) = self.layout(width);
                    let rows = (n + columns - 1) / columns;
                    let height = rows * size + (rows - 1) * gap;
                    (height, height, -1, -1)
                }
            }
        }

        fn size_allocate(&self, width: i32, _height: i32, _baseline: i32) {
            let (columns, size) = self.layout(width);
            let gap = self.gap.get();
            let mut index = 0;
            let mut child = self.obj().first_child();
            while let Some(widget) = child {
                let (column, row) = (index % columns, index / columns);
                let transform = gsk::Transform::new().translate(&graphene::Point::new(
                    (column * (size + gap)) as f32,
                    (row * (size + gap)) as f32,
                ));
                widget.allocate(size, size, -1, Some(transform));
                index += 1;
                child = widget.next_sibling();
            }
        }
    }
}

glib::wrapper! {
    /// Gleich große, quadratische Kacheln in so vielen Spalten, wie die
    /// Breite bei `tile` hergibt — „Days".
    pub struct TileGrid(ObjectSubclass<imp::TileGrid>)
        @extends gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl TileGrid {
    pub fn new(tile: i32, gap: i32) -> Self {
        let this: Self = glib::Object::new();
        this.imp().tile.set(tile);
        this.imp().gap.set(gap);
        this
    }

    pub fn append(&self, child: &impl IsA<gtk::Widget>) {
        child.set_parent(self);
        self.queue_resize();
    }

    pub fn remove_all(&self) {
        while let Some(child) = self.first_child() {
            child.unparent();
        }
        self.queue_resize();
    }

    pub fn set_tile(&self, tile: i32) {
        if self.imp().tile.replace(tile) != tile {
            self.queue_resize();
        }
    }
}

glib::wrapper! {
    /// Eine Fläche mit festem Seitenverhältnis; ihr Kind füllt sie ganz.
    pub struct RatioBox(ObjectSubclass<imp::RatioBox>)
        @extends gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl RatioBox {
    /// `ratio` = Breite / Höhe, `natural` = Wunschbreite.
    pub fn new(child: &impl IsA<gtk::Widget>, ratio: f64, natural: i32) -> Self {
        let this: Self = glib::Object::new();
        this.imp().ratio.set(ratio);
        this.imp().natural.set(natural);
        this.set_overflow(gtk::Overflow::Hidden);
        child.set_parent(&this);
        this
    }

    /// Fest in der Wunschbreite, etwa als Titelbild in einer Listenzeile.
    pub fn fixed(child: &impl IsA<gtk::Widget>, ratio: f64, size: i32) -> Self {
        let this = Self::new(child, ratio, size);
        this.imp().fixed.set(true);
        this
    }

    /// Mindestens so breit (Kachel im Tagesraster).
    pub fn set_min_width(&self, width: i32) {
        if self.imp().min_width.replace(width) != width {
            self.imp().natural.set(width);
            self.queue_resize();
        }
    }

    /// Ein weiteres Kind darüber, etwa ein Kennzeichen in der Ecke. Es wird
    /// wie das erste auf die ganze Fläche gelegt und richtet sich über
    /// `halign`/`valign` selbst aus.
    pub fn add_overlay(&self, child: &impl IsA<gtk::Widget>) {
        let overlay = gtk::Overlay::new();
        // Das erste Kind wandert in ein Overlay, damit `halign` gilt.
        if let Some(first) = self.first_child() {
            first.unparent();
            overlay.set_child(Some(&first));
        }
        overlay.add_overlay(child);
        overlay.set_parent(self);
    }
}

glib::wrapper! {
    /// Zwei Spalten im Verhältnis `lead : 1` ab einer Mindestbreite,
    /// darunter übereinander.
    pub struct Columns(ObjectSubclass<imp::Columns>)
        @extends gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl Columns {
    pub fn new(
        left: &impl IsA<gtk::Widget>,
        right: &impl IsA<gtk::Widget>,
        threshold: i32,
        lead: f64,
        gap: i32,
    ) -> Self {
        let this: Self = glib::Object::new();
        this.imp().threshold.set(threshold);
        this.imp().lead.set(lead);
        this.imp().gap.set(gap);
        left.set_parent(&this);
        right.set_parent(&this);
        this
    }
}
