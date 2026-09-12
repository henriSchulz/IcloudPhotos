//! Der Bildbetrachter: blättern, zoomen, Vollbild, Videos.
//!
//! Ein einziges Fenster, das seine Position im Raster kennt — nicht eines pro
//! Foto. Nur so lässt sich mit den Pfeiltasten durch die Bibliothek gehen,
//! ohne zwischendurch immer wieder ins Raster zurückzumüssen.

use std::cell::{Cell, RefCell};
use std::path::Path;
use std::rc::{Rc, Weak};

use adw::prelude::*;
use gtk::{gdk, gio, glib};

use crate::export;
use crate::photo_object::PhotoObject;
use crate::window::{AppState, UiAction};

/// So viele Nachbarn in jede Richtung werden im Voraus geholt. Einer reicht:
/// er deckt den nächsten Tastendruck ab, ohne die Leitung vollzulaufen.
const PREFETCH_NEIGHBOURS: i32 = 1;

/// Ein Zoomschritt pro Rasterung des Mausrads.
const ZOOM_STEP: f64 = 1.25;
const MAX_ZOOM: f64 = 8.0;
const MIN_ZOOM: f64 = 0.02;

/// Der Betrachter, solange er offen ist.
pub struct Viewer {
    state: Rc<AppState>,
    /// Das Hauptfenster — fuer echtes Vollbild auf Tastendruck.
    window: adw::ApplicationWindow,
    /// Die beiden Ebenen des Fensters: Bibliothek und Betrachter.
    screens: gtk::Stack,
    /// Die eigene Seite, um sie beim Schliessen wieder auszuhaengen.
    page: gtk::Box,
    /// Die Tastatursteuerung haengt am Fenster, nicht an der Seite — sonst
    /// bekaeme sie nichts, solange der Fokus im Raster steht. Beim Schliessen
    /// muss sie wieder ab, sonst sammelt das Fenster mit jedem geoeffneten
    /// Foto eine weitere.
    keys: RefCell<Option<gtk::EventControllerKey>>,
    title: adw::WindowTitle,
    stack: gtk::Stack,
    picture: gtk::Picture,
    video: gtk::Video,
    scroller: gtk::ScrolledWindow,
    spinner: gtk::Spinner,
    zoom_label: gtk::Label,
    /// Die Fehlerseite — sie bekommt je nach Ursache einen anderen Text.
    failed: adw::StatusPage,
    /// Position im Modell des Rasters.
    position: Cell<u32>,
    /// Maßstab, oder `None` für "ins Fenster eingepasst".
    zoom: Cell<Option<f64>>,
    /// Größe des gezeigten Bildes in Pixeln — Bezugsgröße jedes Maßstabs.
    natural: Cell<(f64, f64)>,
    /// Letzte Zeigerposition im Scrollbereich, damit das Rad dort zoomt, wo
    /// der Zeiger steht, statt in die Bildmitte.
    pointer: Cell<(f64, f64)>,
    /// Scrollstand beim Beginn einer Ziehbewegung.
    drag_origin: Cell<(f64, f64)>,
}

/// Öffnet den Betrachter an dieser Position im Raster.
pub fn open(
    state: &Rc<AppState>,
    window: &adw::ApplicationWindow,
    screens: &gtk::Stack,
    position: u32,
) -> Rc<Viewer> {
    let picture = gtk::Picture::builder()
        .content_fit(gtk::ContentFit::Contain)
        .hexpand(true)
        .vexpand(true)
        .build();

    let scroller = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vscrollbar_policy(gtk::PolicyType::Never)
        .hexpand(true)
        .vexpand(true)
        .child(&picture)
        .build();

    let video = gtk::Video::builder().autoplay(true).build();

    let loading = adw::StatusPage::builder()
        .title("Original wird geladen …")
        .description("Die Vorschau liegt lokal, das Original kommt aus iCloud.")
        .child(&gtk::Spinner::builder().spinning(true).build())
        .build();

    let failed = adw::StatusPage::builder()
        .icon_name("dialog-warning-symbolic")
        .title("Original nicht verfügbar")
        .build();
    let failed_page = failed.clone();

    let stack = gtk::Stack::new();
    stack.add_named(&scroller, Some("image"));
    stack.add_named(&video, Some("video"));
    stack.add_named(&loading, Some("loading"));
    stack.add_named(&failed, Some("failed"));

    let title = adw::WindowTitle::new("", "");
    let header = adw::HeaderBar::builder()
        .title_widget(&title)
        // Die Fensterknoepfe gehoeren zur Bibliotheksseite; hier fuehrt der
        // Weg zurueck ins Raster, nicht aus der App heraus.
        .show_end_title_buttons(false)
        .build();

    let back = gtk::Button::builder()
        .icon_name("go-previous-symbolic")
        .tooltip_text("Zurück zur Übersicht (Esc)")
        .build();
    header.pack_start(&back);

    let previous = gtk::Button::builder()
        .icon_name("go-up-symbolic")
        .tooltip_text("Vorheriges Foto (←)")
        .build();
    let next = gtk::Button::builder()
        .icon_name("go-down-symbolic")
        .tooltip_text("Nächstes Foto (→)")
        .build();
    let navigation = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    navigation.add_css_class("linked");
    navigation.append(&previous);
    navigation.append(&next);
    header.pack_start(&navigation);

    let spinner = gtk::Spinner::builder().visible(false).build();
    header.pack_start(&spinner);

    let fullscreen = gtk::Button::builder()
        .icon_name("view-fullscreen-symbolic")
        .tooltip_text("Vollbild (F)")
        .build();
    header.pack_end(&fullscreen);

    let menu = gio::Menu::new();
    menu.append(Some("Sichern unter …"), Some("viewer.save"));
    menu.append(Some("Kopieren"), Some("viewer.copy"));
    menu.append(Some("Öffnen mit …"), Some("viewer.open"));
    let menu_button = gtk::MenuButton::builder()
        .icon_name("view-more-symbolic")
        .menu_model(&menu)
        .tooltip_text("Aktionen")
        .build();
    header.pack_end(&menu_button);

    let zoom_label = gtk::Label::builder()
        .css_classes(["dim-label", "numeric"])
        .visible(false)
        .build();
    header.pack_end(&zoom_label);

    let page = gtk::Box::new(gtk::Orientation::Vertical, 0);
    page.append(&header);
    page.append(&stack);
    // Eine Seite dieses Namens kann noch stehen, wenn der Stack sie waehrend
    // einer laufenden Blende nicht sofort losgelassen hat. Zwei gleichnamige
    // Seiten sind ein Fehler, und GTK zeigte dann die alte.
    if let Some(stale) = screens.child_by_name(VIEWER_PAGE) {
        screens.remove(&stale);
    }
    screens.add_named(&page, Some(VIEWER_PAGE));

    let viewer = Rc::new(Viewer {
        state: state.clone(),
        window: window.clone(),
        screens: screens.clone(),
        page: page.clone(),
        keys: RefCell::new(None),
        title,
        stack,
        picture,
        video,
        scroller: scroller.clone(),
        spinner,
        zoom_label,
        failed: failed_page,
        position: Cell::new(position),
        zoom: Cell::new(None),
        natural: Cell::new((0.0, 0.0)),
        pointer: Cell::new((0.0, 0.0)),
        drag_origin: Cell::new((0.0, 0.0)),
    });

    connect_navigation(&viewer, &previous, &next);
    connect_zoom(&viewer, &scroller);
    connect_keys(&viewer);
    connect_actions(&viewer, &page);

    back.connect_clicked({
        let weak = Rc::downgrade(&viewer);
        move |_| with(&weak, |viewer| viewer.close())
    });
    fullscreen.connect_clicked({
        let weak = Rc::downgrade(&viewer);
        move |_| with(&weak, |viewer| viewer.toggle_fullscreen())
    });

    viewer.show_current();
    screens.set_visible_child_name(VIEWER_PAGE);
    viewer
}

/// Namen der beiden Fensterebenen.
pub const VIEWER_PAGE: &str = "viewer";
pub const LIBRARY_PAGE: &str = "library";

impl Viewer {
    /// Ob der Betrachter auf dieses Original wartet.
    pub fn wants(&self, id: &str) -> bool {
        self.current().is_some_and(|photo| photo.id() == id)
    }

    /// Ein Original ist eingetroffen.
    pub fn deliver(&self, id: &str, path: &Path) {
        if !self.wants(id) {
            return;
        }
        let Some(photo) = self.current() else { return };
        self.spinner.set_visible(false);
        if photo.is_video() {
            self.show_video(path);
        } else {
            self.show_image(path);
        }
    }

    /// Das Original ist nicht zu haben.
    pub fn fail(&self, id: &str) {
        if !self.wants(id) {
            return;
        }
        self.spinner.set_visible(false);
        // Eine Vorschau ist besser als eine Fehlerseite: das Foto ist ja da,
        // nur nicht in voller Auflösung.
        if self.stack.visible_child_name().as_deref() != Some("image") {
            self.failed
                .set_description(Some("Das Original kam nicht aus iCloud."));
            self.stack.set_visible_child_name("failed");
        }
    }

    /// Ein Ortsname ist eingetroffen.
    ///
    /// Kommt er verspätet — die Anfrage geht über das Netz —, hat der Betrachter
    /// womöglich längst ein anderes Foto. Dann gehört der Name nicht hierher.
    pub fn deliver_place(&self, id: &str, place: Option<&str>) {
        let (Some(place), true) = (place, self.wants(id)) else {
            return;
        };
        self.title.set_title(place);
    }

    /// Sucht das gerade gezeigte Foto im Modell wieder.
    ///
    /// Während eines Syncs wird das Modell neu bestückt; ohne dieses Nachziehen
    /// zeigte ein Tastendruck plötzlich auf ein anderes Foto.
    pub fn relocate(&self, id: &str) {
        let model = &self.state.model;
        for index in 0..model.n_items() {
            if let Some(photo) = model.item(index).and_downcast::<PhotoObject>()
                && photo.id() == id
            {
                self.position.set(index);
                return;
            }
        }
    }

    /// Die ID des gezeigten Fotos.
    pub fn current_id(&self) -> Option<String> {
        self.current().map(|photo| photo.id())
    }

    fn current(&self) -> Option<PhotoObject> {
        self.state
            .model
            .item(self.position.get())
            .and_downcast::<PhotoObject>()
    }

    /// Blättert um `delta` Fotos weiter.
    fn step(&self, delta: i32) {
        let count = self.state.model.n_items() as i64;
        if count == 0 {
            return;
        }
        let target = self.position.get() as i64 + delta as i64;
        // Am Rand bleibt es stehen, statt umzulaufen — ein Sprung vom letzten
        // zum ersten Foto wäre als Geste nicht zu unterscheiden.
        let target = target.clamp(0, count - 1) as u32;
        if target == self.position.get() {
            return;
        }
        self.position.set(target);
        self.show_current();
    }

    /// Zeigt das Foto an dieser Position.
    pub fn show(&self, position: u32) {
        self.position.set(position);
        self.show_current();
        self.screens.set_visible_child_name(VIEWER_PAGE);
    }

    /// Zurück zur Übersicht.
    pub fn close(&self) {
        self.stop_video();
        // Vollbild gehört zum geöffneten Foto, nicht zum Raster.
        if self.window.is_fullscreen() {
            self.window.unfullscreen();
        }
        self.window.set_title(Some("iCloud Photos"));
        self.screens.set_visible_child_name(LIBRARY_PAGE);
        // Die Seite wieder aushaengen: der naechste Aufruf baut sie neu, und
        // zwei Seiten desselben Namens im Stack waeren ein Fehler.
        self.screens.remove(&self.page);
        if let Some(keys) = self.keys.borrow_mut().take() {
            self.window.remove_controller(&keys);
        }
        // Das Raster soll dort stehen, wo das Foto lag.
        self.state.return_to_grid(self.position.get());
        self.state.close_viewer();
    }

    /// Ob der Betrachter gerade die sichtbare Seite ist.
    fn is_showing(&self) -> bool {
        self.screens.visible_child_name().as_deref() == Some(VIEWER_PAGE)
    }

    fn jump(&self, position: u32) {
        self.position.set(position);
        self.show_current();
    }

    /// Baut die Ansicht auf das Foto an der aktuellen Position um.
    fn show_current(&self) {
        self.stop_video();
        self.zoom.set(None);
        self.zoom_label.set_visible(false);

        let Some(photo) = self.current() else {
            self.stack.set_visible_child_name("failed");
            return;
        };
        if photo.is_placeholder() {
            // Ein Skelett hat noch kein Foto dahinter — weiter zum nächsten.
            self.stack.set_visible_child_name("loading");
            return;
        }

        // Der Ort sagt mehr über ein Foto als sein Dateiname. Er kommt aber
        // erst aus dem Netz, deshalb steht bis dahin der Dateiname da.
        let place = photo
            .coordinates()
            .and_then(|(lat, lon)| self.state.known_place(lat, lon));
        self.title
            .set_title(place.as_deref().unwrap_or(&photo.filename()));
        if place.is_none()
            && let Some((lat, lon)) = photo.coordinates()
        {
            self.state.request_place(&photo.id(), lat, lon);
        }

        let kind = if photo.is_video() { "Video · " } else { "" };
        self.title
            .set_subtitle(&format!("{kind}{}", photo.captured_label()));
        self.window.set_title(Some(&photo.filename()));

        // Auch das reine Ansehen eines bereits gecachten Fotos zählt als
        // Zugriff — sonst verhält sich der Full-Res-Cache wie FIFO statt LRU.
        self.state.touch(&photo.id());

        match photo.cached_original() {
            Some(path) if Path::new(&path).is_file() => {
                self.spinner.set_visible(false);
                if photo.is_video() {
                    self.show_video(Path::new(&path));
                } else {
                    self.show_image(Path::new(&path));
                }
            }
            _ => {
                self.spinner.set_visible(true);
                match photo.thumb_path() {
                    // Die Vorschau sofort zeigen, das Original ersetzt sie.
                    // Besser eine unscharfe Fläche als eine leere.
                    Some(thumb) if !photo.is_video() => self.show_image(Path::new(&thumb)),
                    _ => self.stack.set_visible_child_name("loading"),
                }
                self.state.request_original(&photo.id());
            }
        }

        self.prefetch_neighbours();
    }

    /// Holt die Nachbarn im Voraus, damit das nächste Blättern sofort sitzt.
    fn prefetch_neighbours(&self) {
        let count = self.state.model.n_items() as i64;
        let here = self.position.get() as i64;
        for delta in -PREFETCH_NEIGHBOURS..=PREFETCH_NEIGHBOURS {
            if delta == 0 {
                continue;
            }
            let index = here + delta as i64;
            if index < 0 || index >= count {
                continue;
            }
            let Some(photo) = self
                .state
                .model
                .item(index as u32)
                .and_downcast::<PhotoObject>()
            else {
                continue;
            };
            if photo.is_placeholder() || photo.cached_original().is_some() {
                continue;
            }
            self.state.prefetch_original(&photo.id());
        }
    }

    fn show_image(&self, path: &Path) {
        match gdk::Texture::from_filename(path) {
            Ok(texture) => {
                self.natural
                    .set((texture.width() as f64, texture.height() as f64));
                self.picture.set_paintable(Some(&texture));
                self.apply_zoom(None);
                self.stack.set_visible_child_name("image");
            }
            Err(error) => {
                tracing::warn!(path = %path.display(), %error, "Bild nicht ladbar");
                self.show_failure(path);
            }
        }
    }

    /// Zeigt die Fehlerseite — und sagt dazu, woran es liegt.
    ///
    /// Bei HEIC ist die Ursache fast immer dieselbe und von aussen behebbar:
    /// `gdk-pixbuf2` fuehrt `libheif` nur als *optionale* Abhaengigkeit. Fehlt
    /// sie, laedt das Raster brav seine JPEG-Vorschauen und der Betrachter
    /// zeigt bei jedem zweiten Foto nichts — ohne diesen Hinweis sucht man den
    /// Fehler bei iCloud statt beim Paketmanager.
    fn show_failure(&self, path: &Path) {
        let heic = path
            .extension()
            .map(|ext| ext.to_string_lossy().to_lowercase())
            .is_some_and(|ext| matches!(ext.as_str(), "heic" | "heif" | "avif"));

        self.failed.set_description(Some(if heic {
            "Das Original liegt lokal, lässt sich aber nicht anzeigen. \
             Für HEIC-Dateien braucht gdk-pixbuf die Bibliothek libheif \
             (Paket »libheif«)."
        } else {
            "Das Original ließ sich nicht laden."
        }));
        self.stack.set_visible_child_name("failed");
    }

    fn show_video(&self, path: &Path) {
        self.video.set_filename(Some(path));
        self.stack.set_visible_child_name("video");
    }

    /// Hält ein laufendes Video an und gibt die Datei frei.
    fn stop_video(&self) {
        self.video.set_media_stream(None::<&gtk::MediaStream>);
    }

    // ------------------------------------------------------------------
    // Zoom
    // ------------------------------------------------------------------

    /// Der Maßstab, bei dem das Bild gerade ins Fenster passt.
    fn fit_scale(&self) -> f64 {
        let (width, height) = self.natural.get();
        if width <= 0.0 || height <= 0.0 {
            return 1.0;
        }
        let available_w = self.scroller.width() as f64;
        let available_h = self.scroller.height() as f64;
        if available_w <= 0.0 || available_h <= 0.0 {
            return 1.0;
        }
        (available_w / width).min(available_h / height)
    }

    /// Zoomt um einen Schritt, ausgehend vom aktuellen Maßstab.
    fn zoom_by(&self, factor: f64, anchor: Option<(f64, f64)>) {
        let current = self.zoom.get().unwrap_or_else(|| self.fit_scale());
        let target = (current * factor).clamp(MIN_ZOOM, MAX_ZOOM);
        // Bis dicht an die Einpassung zurückgezoomt heisst: eingepasst. Sonst
        // bliebe ein Bild bei 99 % mit Scrollbalken stehen.
        if target <= self.fit_scale() * 1.01 {
            self.apply_zoom(None);
        } else {
            self.zoom.set(Some(target));
            self.apply_zoom(anchor);
        }
    }

    /// Setzt einen festen Maßstab (`None` = einpassen) und richtet die Ansicht
    /// danach aus.
    fn apply_zoom(&self, anchor: Option<(f64, f64)>) {
        let hadj = self.scroller.hadjustment();
        let vadj = self.scroller.vadjustment();
        // Den Punkt merken, der unter dem Zeiger liegt — nach dem Umbau soll
        // er wieder dort sein.
        let before = anchor.map(|(x, y)| {
            (
                (hadj.value() + x) / hadj.upper().max(1.0),
                (vadj.value() + y) / vadj.upper().max(1.0),
            )
        });

        match self.zoom.get() {
            None => {
                self.picture.set_content_fit(gtk::ContentFit::Contain);
                self.picture.set_size_request(-1, -1);
                self.picture.set_halign(gtk::Align::Fill);
                self.picture.set_valign(gtk::Align::Fill);
                self.scroller
                    .set_policy(gtk::PolicyType::Never, gtk::PolicyType::Never);
                self.scroller.set_cursor_from_name(None);
                self.zoom_label.set_visible(false);
            }
            Some(scale) => {
                let (width, height) = self.natural.get();
                self.picture.set_content_fit(gtk::ContentFit::Fill);
                self.picture
                    .set_size_request((width * scale) as i32, (height * scale) as i32);
                self.picture.set_halign(gtk::Align::Center);
                self.picture.set_valign(gtk::Align::Center);
                self.scroller
                    .set_policy(gtk::PolicyType::Automatic, gtk::PolicyType::Automatic);
                self.scroller.set_cursor_from_name(Some("grab"));
                self.zoom_label
                    .set_label(&format!("{:.0} %", scale * 100.0));
                self.zoom_label.set_visible(true);
            }
        }

        // Die Scrollgrenzen stehen erst nach der nächsten Zuteilung fest.
        if let (Some((fx, fy)), Some((ax, ay))) = (before, anchor) {
            let scroller = self.scroller.clone();
            glib::idle_add_local_once(move || {
                restore_anchor(&scroller.hadjustment(), fx, ax);
                restore_anchor(&scroller.vadjustment(), fy, ay);
            });
        }
    }

    fn toggle_fullscreen(&self) {
        if self.window.is_fullscreen() {
            self.window.unfullscreen();
        } else {
            self.window.fullscreen();
        }
    }

    /// Die gerade gezeigte Aufnahme als Auftragsposten.
    fn item(&self) -> Option<export::Item> {
        let photo = self.current()?;
        Some(export::Item {
            id: photo.id(),
            filename: photo.filename(),
        })
    }
}

/// Setzt den Scrollstand so, dass der gemerkte Punkt wieder unter dem Zeiger
/// liegt.
fn restore_anchor(adjustment: &gtk::Adjustment, fraction: f64, pointer: f64) {
    let upper = adjustment.upper();
    let max = (upper - adjustment.page_size()).max(0.0);
    adjustment.set_value((fraction * upper - pointer).clamp(0.0, max));
}

fn connect_navigation(viewer: &Rc<Viewer>, previous: &gtk::Button, next: &gtk::Button) {
    previous.connect_clicked({
        let weak = Rc::downgrade(viewer);
        move |_| with(&weak, |viewer| viewer.step(-1))
    });
    next.connect_clicked({
        let weak = Rc::downgrade(viewer);
        move |_| with(&weak, |viewer| viewer.step(1))
    });
}

fn connect_zoom(viewer: &Rc<Viewer>, scroller: &gtk::ScrolledWindow) {
    // Zeigerposition mitschreiben, damit das Rad dort zoomt, wo hingesehen wird.
    let motion = gtk::EventControllerMotion::new();
    motion.connect_motion({
        let weak = Rc::downgrade(viewer);
        move |_, x, y| with(&weak, |viewer| viewer.pointer.set((x, y)))
    });
    scroller.add_controller(motion);

    let scroll = gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::VERTICAL);
    scroll.connect_scroll({
        let weak = Rc::downgrade(viewer);
        move |_, _, dy| {
            let Some(viewer) = weak.upgrade() else {
                return glib::Propagation::Proceed;
            };
            let factor = if dy < 0.0 { ZOOM_STEP } else { 1.0 / ZOOM_STEP };
            viewer.zoom_by(factor, Some(viewer.pointer.get()));
            glib::Propagation::Stop
        }
    });
    scroller.add_controller(scroll);

    // Ziehen verschiebt den Ausschnitt, sobald hineingezoomt ist.
    let drag = gtk::GestureDrag::new();
    drag.connect_drag_begin({
        let weak = Rc::downgrade(viewer);
        move |_, _, _| {
            with(&weak, |viewer| {
                viewer.drag_origin.set((
                    viewer.scroller.hadjustment().value(),
                    viewer.scroller.vadjustment().value(),
                ));
                if viewer.zoom.get().is_some() {
                    viewer.scroller.set_cursor_from_name(Some("grabbing"));
                }
            });
        }
    });
    drag.connect_drag_update({
        let weak = Rc::downgrade(viewer);
        move |_, dx, dy| {
            with(&weak, |viewer| {
                if viewer.zoom.get().is_none() {
                    return;
                }
                let (x, y) = viewer.drag_origin.get();
                set_clamped(&viewer.scroller.hadjustment(), x - dx);
                set_clamped(&viewer.scroller.vadjustment(), y - dy);
            });
        }
    });
    drag.connect_drag_end({
        let weak = Rc::downgrade(viewer);
        move |_, _, _| {
            with(&weak, |viewer| {
                if viewer.zoom.get().is_some() {
                    viewer.scroller.set_cursor_from_name(Some("grab"));
                }
            });
        }
    });
    scroller.add_controller(drag);

    // Doppelklick wechselt zwischen eingepasst und 1:1.
    let click = gtk::GestureClick::new();
    click.set_button(gdk::BUTTON_PRIMARY);
    click.connect_pressed({
        let weak = Rc::downgrade(viewer);
        move |_, clicks, x, y| {
            if clicks != 2 {
                return;
            }
            with(&weak, |viewer| match viewer.zoom.get() {
                Some(_) => viewer.apply_zoom_fit(),
                None => {
                    viewer.zoom.set(Some(1.0));
                    viewer.apply_zoom(Some((x, y)));
                }
            });
        }
    });
    scroller.add_controller(click);
}

fn connect_keys(viewer: &Rc<Viewer>) {
    let keys = gtk::EventControllerKey::new();
    keys.connect_key_pressed({
        let weak = Rc::downgrade(viewer);
        move |_, key, _, _| {
            let Some(viewer) = weak.upgrade() else {
                return glib::Propagation::Proceed;
            };
            // Der Steuerung haengt am Fenster, nicht an der Seite: sie darf dem
            // Raster seine Pfeiltasten nicht wegnehmen.
            if !viewer.is_showing() {
                return glib::Propagation::Proceed;
            }
            match key {
                gdk::Key::Left | gdk::Key::Up | gdk::Key::BackSpace => viewer.step(-1),
                gdk::Key::Right | gdk::Key::Down | gdk::Key::space => viewer.step(1),
                gdk::Key::Page_Up => viewer.step(-10),
                gdk::Key::Page_Down => viewer.step(10),
                gdk::Key::Home => viewer.jump(0),
                gdk::Key::End => {
                    let last = viewer.state.model.n_items().saturating_sub(1);
                    viewer.jump(last);
                }
                gdk::Key::plus | gdk::Key::equal | gdk::Key::KP_Add => {
                    viewer.zoom_by(ZOOM_STEP, None)
                }
                gdk::Key::minus | gdk::Key::KP_Subtract => viewer.zoom_by(1.0 / ZOOM_STEP, None),
                gdk::Key::_0 | gdk::Key::KP_0 => viewer.apply_zoom_fit(),
                gdk::Key::_1 | gdk::Key::KP_1 => {
                    viewer.zoom.set(Some(1.0));
                    viewer.apply_zoom(None);
                }
                gdk::Key::f | gdk::Key::F11 => viewer.toggle_fullscreen(),
                gdk::Key::Escape => {
                    // Erst das Vollbild verlassen, dann zurueck ins Raster —
                    // sonst waere das Foto weg, bevor man wieder etwas sieht.
                    if viewer.window.is_fullscreen() {
                        viewer.window.unfullscreen();
                    } else {
                        viewer.close();
                    }
                }
                _ => return glib::Propagation::Proceed,
            }
            glib::Propagation::Stop
        }
    });
    // In der Erfassungsphase, damit die Tasten beim offenen Foto hier landen
    // und nicht im Raster, das im Hintergrund weiter den Fokus haelt.
    keys.set_propagation_phase(gtk::PropagationPhase::Capture);
    viewer.window.add_controller(keys.clone());
    *viewer.keys.borrow_mut() = Some(keys);
}

/// Sichern, Kopieren und Öffnen hängen an der Aktionsstelle des Hauptfensters —
/// dort liegen Dialog, Fortschritt und Download-Warteschlange.
fn connect_actions(viewer: &Rc<Viewer>, page: &gtk::Box) {
    let group = gio::SimpleActionGroup::new();
    for (name, action) in [
        ("save", UiAction::Save),
        ("copy", UiAction::Clipboard),
        ("open", UiAction::Open),
    ] {
        let entry = gio::SimpleAction::new(name, None);
        entry.connect_activate({
            let weak = Rc::downgrade(viewer);
            move |_, _| {
                with(&weak, |viewer| {
                    if let Some(item) = viewer.item() {
                        viewer.state.run_action(action, vec![item]);
                    }
                });
            }
        });
        group.add_action(&entry);
    }
    page.insert_action_group("viewer", Some(&group));
}

impl Viewer {
    fn apply_zoom_fit(&self) {
        self.zoom.set(None);
        self.apply_zoom(None);
    }
}

fn set_clamped(adjustment: &gtk::Adjustment, value: f64) {
    let max = (adjustment.upper() - adjustment.page_size()).max(0.0);
    adjustment.set_value(value.clamp(0.0, max));
}

/// Ruft `f` auf, solange der Betrachter noch lebt.
///
/// Die Steuerungen des Fensters halten den Betrachter nur schwach — sonst
/// hielten sich Fenster und Betrachter gegenseitig am Leben.
fn with(weak: &Weak<Viewer>, f: impl FnOnce(&Viewer)) {
    if let Some(viewer) = weak.upgrade() {
        f(&viewer);
    }
}
