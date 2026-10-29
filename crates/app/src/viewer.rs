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
use crate::info_panel::InfoPanel;
use crate::photo_object::PhotoObject;
use crate::window::{AppState, UiAction};
use crate::zoom_view::{Swipe, ZOOM_STEP, ZoomView};

/// So viele Nachbarn in jede Richtung werden im Voraus geholt. Einer reicht:
/// er deckt den nächsten Tastendruck ab, ohne die Leitung vollzulaufen.
const PREFETCH_NEIGHBOURS: i32 = 1;

/// Unterhalb dieser Seitenbreite wird Info ein Sheet unten über dem Foto
/// (Velum `surface.photos.compactBelow`, wie die Container-Abfrage im Lab).
fn sheet_below() -> f64 {
    crate::velum::photos("compactBelow", 640.0)
}

thread_local! {
    /// Ob das Info-Panel offen ist. Die Seite wird bei jedem Öffnen neu
    /// gebaut (siehe CLAUDE.md, „Drei Fallen"); ohne diesen Merker wäre das
    /// Panel nach jedem Schließen wieder zu — Photos behält es offen.
    static INFO_OPEN: Cell<bool> = const { Cell::new(false) };
}

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
    picture: ZoomView,
    video: gtk::Video,
    /// Kleine Ladeanzeige unten rechts, solange nur die Vorschau zu sehen
    /// ist und das Original noch aus iCloud kommt.
    spinner: gtk::Box,
    zoom_label: gtk::Label,
    /// Die Fehlerseite — sie bekommt je nach Ursache einen anderen Text.
    failed: adw::StatusPage,
    /// Das ⓘ-Panel rechts neben dem Foto, und die Teilung, die es ein- und
    /// ausblendet.
    info: Rc<InfoPanel>,
    split: adw::OverlaySplitView,
    /// Die Info-Karte (Kopf „Info" + Panel) — rechts in der Teilung oder,
    /// auf schmaler Seite, als Sheet im Overlay über der Bühne.
    inspector: gtk::Box,
    stage: gtk::Overlay,
    narrow: Cell<bool>,
    info_toggle: gtk::ToggleButton,
    /// Position im Modell des Rasters.
    position: Cell<u32>,
}

/// Info beim nächsten Öffnen zeigen (Kontextmenü „Show Info").
pub fn set_info_open(open: bool) {
    INFO_OPEN.set(open);
}

/// Öffnet den Betrachter an dieser Position im Raster.
pub fn open(
    state: &Rc<AppState>,
    window: &adw::ApplicationWindow,
    screens: &gtk::Stack,
    position: u32,
) -> Rc<Viewer> {
    let picture = ZoomView::new();

    let video = gtk::Video::builder().autoplay(true).build();

    let loading = adw::StatusPage::builder()
        .title("Loading Original…")
        .description("The preview is stored locally; the original is coming from iCloud.")
        .child(&gtk::Spinner::builder().spinning(true).build())
        .build();

    let failed = adw::StatusPage::builder()
        .icon_name("dialog-warning-symbolic")
        .title("Original Unavailable")
        .build();
    let failed_page = failed.clone();

    let stack = gtk::Stack::new();

    // Unten in der Ecke statt in der Kopfzeile: dort, wo man hinsieht, und
    // ohne das Bild zu verdecken. Derselbe Grund wie das Video-Kennzeichen im
    // Raster, damit sie auf hellen wie dunklen Fotos lesbar ist.
    let spinner = gtk::Box::builder()
        .css_classes(["photo-badge"])
        .halign(gtk::Align::End)
        .valign(gtk::Align::End)
        .margin_end(12)
        .margin_bottom(12)
        .can_target(false)
        .visible(false)
        .build();
    spinner.append(&gtk::Spinner::builder().spinning(true).build());

    // Die Anzeige haengt nur an der Bildseite: Laedt ein Video oder ein Foto
    // ohne Vorschau, zeigt die Ladeseite ihren eigenen, grossen Kreisel.
    let image_page = gtk::Overlay::builder().child(&picture).build();
    image_page.add_overlay(&spinner);
    stack.add_named(&image_page, Some("image"));
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

    // Der Zurück-Knopf nennt die Seite, von der man kam („Library",
    // Albumname, „Photos" für die Startseite).
    let back_label = state.viewer_back.borrow().clone();
    let back_label = if back_label.is_empty() {
        "Library".to_string()
    } else {
        back_label
    };
    let back = gtk::Button::builder()
        .child(
            &adw::ButtonContent::builder()
                .icon_name("go-previous-symbolic")
                .label(&back_label)
                .build(),
        )
        .tooltip_text(format!("Back to {back_label} (Esc)"))
        .css_classes(["back"])
        .build();
    header.pack_start(&back);
    let head_separator = || {
        gtk::Separator::builder()
            .orientation(gtk::Orientation::Vertical)
            .valign(gtk::Align::Center)
            .css_classes(["head-separator"])
            .build()
    };
    let nav_separator = head_separator();
    header.pack_start(&nav_separator);

    let previous = gtk::Button::builder()
        .icon_name("go-previous-symbolic")
        .tooltip_text("Previous Photo (←)")
        .build();
    let next = gtk::Button::builder()
        .icon_name("go-next-symbolic")
        .tooltip_text("Next Photo (→)")
        .build();
    let navigation = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    navigation.append(&previous);
    navigation.append(&next);
    header.pack_start(&navigation);

    let fullscreen = gtk::Button::builder()
        .icon_name("view-fullscreen-symbolic")
        .tooltip_text("Full Screen (F)")
        .build();
    header.pack_end(&fullscreen);

    let info_toggle = gtk::ToggleButton::builder()
        .icon_name("dialog-information-symbolic")
        .tooltip_text("Info (I)")
        .active(INFO_OPEN.get())
        .build();
    header.pack_end(&info_toggle);
    header.pack_end(&head_separator());

    let menu = gio::Menu::new();
    menu.append(Some("Export…"), Some("viewer.save"));
    menu.append(Some("Copy"), Some("viewer.copy"));
    menu.append(Some("Open in Default App"), Some("viewer.open"));
    if crate::retouch::available() {
        menu.append(Some("Edit in Retouch"), Some("viewer.retouch"));
    }
    let danger = gio::Menu::new();
    danger.append(Some("Delete…"), Some("viewer.delete"));
    menu.append_section(None, &danger);
    let menu_button = gtk::MenuButton::builder()
        .icon_name("view-more-symbolic")
        .menu_model(&menu)
        .tooltip_text("Actions")
        .build();
    header.pack_end(&menu_button);
    let export = gtk::Button::builder()
        .icon_name("document-save-symbolic")
        .tooltip_text("Export… (Ctrl+S)")
        .action_name("viewer.save")
        .css_classes(["wide-only"])
        .build();
    header.pack_end(&export);
    if crate::retouch::available() {
        header.pack_end(
            &gtk::Button::builder()
                .icon_name("document-edit-symbolic")
                .tooltip_text("Edit in Retouch (E)")
                .action_name("viewer.retouch")
                .css_classes(["wide-only"])
                .build(),
        );
    }

    let zoom_label = gtk::Label::builder()
        .css_classes(["dim-label", "numeric"])
        .visible(false)
        .build();
    header.pack_end(&zoom_label);

    // Die Bühne: das Foto auf einer ruhigen, gerundeten Fläche (Lab
    // `.vstage`). Ein Overlay, damit Info auf schmaler Seite als Sheet
    // darüber liegen kann.
    let stage_box = gtk::Box::builder()
        .css_classes(["photos-stage"])
        .overflow(gtk::Overflow::Hidden)
        .build();
    stack.set_hexpand(true);
    stage_box.append(&stack);
    let stage = gtk::Overlay::builder().child(&stage_box).build();

    // Info als Karte: Kopf „Info" mit Schließen, darunter das Panel.
    let info = InfoPanel::new();
    let close_info = gtk::Button::builder()
        .icon_name("window-close-symbolic")
        .tooltip_text("Hide Info (I)")
        .css_classes(["flat"])
        .build();
    let info_head = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .css_classes(["inspector-head"])
        .build();
    let info_title = gtk::Label::builder()
        .label("Info")
        .xalign(0.0)
        .hexpand(true)
        .build();
    info_head.append(&info_title);
    info_head.append(&close_info);
    info.widget.set_vexpand(true);
    info.widget.set_propagate_natural_height(true);
    let inspector = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .css_classes(["photos-inspector"])
        .overflow(gtk::Overflow::Hidden)
        .build();
    inspector.append(&info_head);
    inspector.append(&info.widget);

    // Breit: Info rechts neben dem Foto. `ZoomView` passt sich über seine
    // Zuteilung neu ein, Maßstab und Mittelpunkt bleiben.
    let split = adw::OverlaySplitView::builder()
        .content(&stage)
        .sidebar(&inspector)
        .sidebar_position(gtk::PackType::End)
        .min_sidebar_width(crate::velum::photos("inspector", 290.0))
        .max_sidebar_width(crate::velum::photos("inspector", 290.0))
        .show_sidebar(INFO_OPEN.get())
        .css_classes(["photos-viewer-split"])
        .build();
    // Der Umbruch hängt an einer eigenen BreakpointBin, nicht am Fenster: die
    // Seite wird bei jedem Öffnen neu gebaut, und ein Breakpoint am Fenster
    // hielte die alte Teilung fest. Eine Mindestgröße verlangt die Bin; sie
    // bleibt bewusst klein, sonst hielte sie das Fenster vom Schrumpfen ab.
    let narrow = adw::Breakpoint::new(adw::BreakpointCondition::new_length(
        adw::BreakpointConditionLengthType::MaxWidth,
        sheet_below(),
        adw::LengthUnit::Px,
    ));
    let content = adw::BreakpointBin::builder()
        .child(&split)
        .width_request(320)
        .height_request(200)
        .vexpand(true)
        .build();
    content.add_breakpoint(narrow.clone());

    // Im Tablet-Modus blendet ein Tipp aufs Foto die Leiste aus und wieder
    // ein, wie in Photos auf dem iPad; das Foto steht dann auf Schwarz.
    let chrome = gtk::Revealer::builder()
        .child(&header)
        .reveal_child(true)
        .transition_type(gtk::RevealerTransitionType::Crossfade)
        .transition_duration(crate::velum::duration("fast"))
        .build();
    // Mit dem Finger wird gewischt; die Pfeile braucht es dann nicht.
    crate::velum::on_tablet({
        let navigation = navigation.downgrade();
        move |on| {
            if let Some(navigation) = navigation.upgrade() {
                navigation.set_visible(!on);
            }
        }
    });
    let page = gtk::Box::new(gtk::Orientation::Vertical, 0);
    page.add_css_class("photos-viewer");
    page.append(&chrome);
    page.append(&content);
    picture.connect_tap({
        let chrome = chrome.clone();
        let page = page.clone();
        move || {
            if !crate::velum::tablet() {
                return;
            }
            let show = !chrome.reveals_child();
            chrome.set_reveal_child(show);
            if show {
                page.remove_css_class("immersive");
            } else {
                page.add_css_class("immersive");
            }
        }
    });
    // Die Gruppe muss haengen, bevor die Seite ins Fenster kommt. Spaeter
    // eingehaengt, loest GTK die Aktionen zwar noch auf, das Menue erfaehrt
    // davon aber nichts und zeigt alle Eintraege ausgegraut. Die Aktionen
    // selbst kommen erst in `connect_actions` dazu — das bemerkt es.
    let actions = gio::SimpleActionGroup::new();
    page.insert_action_group("viewer", Some(&actions));
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
        picture: picture.clone(),
        video,
        spinner,
        zoom_label,
        failed: failed_page,
        info,
        split: split.clone(),
        inspector: inspector.clone(),
        stage: stage.clone(),
        narrow: Cell::new(false),
        info_toggle: info_toggle.clone(),
        position: Cell::new(position),
    });

    connect_navigation(&viewer, &previous, &next);
    info_toggle.connect_toggled({
        let weak = Rc::downgrade(&viewer);
        move |toggle| {
            let open = toggle.is_active();
            with(&weak, |viewer| {
                if INFO_OPEN.get() != open || viewer.shows_info() != open {
                    viewer.set_info(open);
                }
            });
        }
    });
    close_info.connect_clicked({
        let weak = Rc::downgrade(&viewer);
        move |_| with(&weak, |viewer| viewer.set_info(false))
    });
    narrow.connect_apply({
        let weak = Rc::downgrade(&viewer);
        move |_| with(&weak, |viewer| viewer.set_narrow(true))
    });
    narrow.connect_unapply({
        let weak = Rc::downgrade(&viewer);
        move |_| with(&weak, |viewer| viewer.set_narrow(false))
    });
    // Der Maßstab steht nur im Kopf, solange hineingezoomt ist.
    picture.connect_changed({
        let label = viewer.zoom_label.clone();
        move |view| {
            label.set_visible(!view.is_fitted());
            label.set_label(&format!("{:.0} %", view.pixel_scale() * 100.0));
        }
    });
    // Wischen wie in Photos auf dem iPad: zur Seite blättert, nach unten
    // geht es zurück zur Übersicht. Geschlossen wird erst nach der Geste —
    // mitten in ihr hängte das Schließen das Widget aus, das sie meldet.
    picture.connect_swipe({
        let weak = Rc::downgrade(&viewer);
        move |swipe| {
            let Some(viewer) = weak.upgrade() else {
                return false;
            };
            match swipe {
                Swipe::Next => viewer.try_step(1),
                Swipe::Previous => viewer.try_step(-1),
                Swipe::Close => {
                    let weak = weak.clone();
                    glib::idle_add_local_once(move || with(&weak, |viewer| viewer.close()));
                    true
                }
            }
        }
    });
    connect_keys(&viewer);
    connect_actions(&viewer, &actions);

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
        // Erst jetzt gibt es EXIF. Ist das Panel zu, holt es beim Öffnen den
        // Katalogstand — der Downloader hat das Original dort schon vermerkt.
        if INFO_OPEN.get() {
            self.info.original_arrived(id, path);
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
                .set_description(Some("The original could not be downloaded from iCloud."));
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
        self.info.set_place(id, place);
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

    /// Blättert und sagt, ob es ein Foto weiter ging (am Rand nicht).
    fn try_step(&self, delta: i32) -> bool {
        let before = self.position.get();
        self.step(delta);
        self.position.get() != before
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
        self.picture.reset();
        self.refresh_info();

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
                self.picture.set_texture(Some(&texture));
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

        self.failed.set_title("Original Unavailable");
        self.failed.set_description(Some(if heic {
            "The original is stored locally but cannot be displayed. \
             gdk-pixbuf needs the libheif library for HEIC files \
             (package “libheif”)."
        } else {
            "The original could not be loaded."
        }));
        self.stack.set_visible_child_name("failed");
    }

    fn show_video(&self, path: &Path) {
        self.video.set_filename(Some(path));
        self.stack.set_visible_child_name("video");

        // GtkVideo scheitert sonst stumm: eine schwarze Flaeche mit
        // Abspielknopf, der nichts tut. Der Fehler steht nur am Stream.
        let Some(stream) = self.video.media_stream() else {
            return;
        };
        let failed = self.failed.clone();
        let stack = self.stack.clone();
        let path = path.to_path_buf();
        let report = move |stream: &gtk::MediaStream| {
            let Some(error) = stream.error() else {
                return;
            };
            // Der rohe GStreamer-Text gehoert ins Log, nicht in die Oberflaeche.
            tracing::warn!(path = %path.display(), %error, "Video nicht abspielbar");
            let (title, body) = video_failure(&error.to_string());
            failed.set_title(title);
            failed.set_description(Some(body));
            stack.set_visible_child_name("failed");
        };
        report(&stream);
        stream.connect_error_notify(report);
    }

    /// Hält ein laufendes Video an und gibt die Datei frei.
    fn stop_video(&self) {
        self.video.set_media_stream(None::<&gtk::MediaStream>);
    }

    /// Füllt das Info-Panel für das gezeigte Foto — nur, wenn es offen ist.
    ///
    /// Beim Blättern mit geschlossenem Panel kostete jedes Foto sonst eine
    /// Katalogabfrage und das Lesen des Originals, das niemand sieht.
    fn refresh_info(&self) {
        if !INFO_OPEN.get() {
            return;
        }
        let Some(photo) = self.current().filter(|photo| !photo.is_placeholder()) else {
            self.info.clear();
            return;
        };
        // Die volle Katalogzeile: Maße, Dateigröße und der Pfad des Originals
        // stehen nicht im `PhotoObject`.
        let row = self.state.catalog.borrow().get(&photo.id());
        let row = match row {
            Ok(Some(row)) => row,
            Ok(None) => {
                self.info.clear();
                return;
            }
            Err(error) => {
                tracing::warn!(%error, "Katalogzeile für das Info-Panel nicht lesbar");
                self.info.clear();
                return;
            }
        };
        let place = row
            .record
            .coordinates()
            .and_then(|(lat, lon)| self.state.known_place(lat, lon));
        self.info
            .show(&row, &photo.captured_label(), place.as_deref());
    }

    fn toggle_info(&self) {
        self.set_info(!INFO_OPEN.get());
    }

    /// Ob Info gerade zu sehen ist — rechts oder als Sheet.
    fn shows_info(&self) -> bool {
        if self.narrow.get() {
            self.inspector.is_visible()
                && self
                    .inspector
                    .parent()
                    .is_some_and(|p| p != *self.split.upcast_ref::<gtk::Widget>())
        } else {
            self.split.shows_sidebar()
        }
    }

    /// Info zeigen oder verbergen; der Zustand überlebt das Blättern und das
    /// Schließen des Betrachters (`INFO_OPEN`).
    fn set_info(&self, open: bool) {
        INFO_OPEN.set(open);
        if self.narrow.get() {
            self.inspector.set_visible(open);
        } else {
            self.inspector.set_visible(true);
            self.split.set_show_sidebar(open);
        }
        if self.info_toggle.is_active() != open {
            self.info_toggle.set_active(open);
        }
        // Geschlossen wird nichts nachgeführt; beim Öffnen holt das Panel
        // den Stand des gerade gezeigten Fotos.
        if open {
            self.refresh_info();
        }
    }

    /// Schmale Seite: Info wandert aus der Teilung als Sheet unten über die
    /// Bühne (höchstens `surface.photos.sheetShare` der Höhe) — und zurück.
    fn set_narrow(&self, narrow: bool) {
        if self.narrow.replace(narrow) == narrow {
            return;
        }
        if narrow {
            self.page.add_css_class("narrow");
            self.split.set_show_sidebar(false);
            self.split.set_sidebar(gtk::Widget::NONE);
            self.inspector.set_valign(gtk::Align::End);
            self.inspector.set_vexpand(false);
            let height =
                self.window.height().max(1) as f64 * crate::velum::photos("sheetShare", 0.56);
            if let Some(scroller) = self
                .inspector
                .last_child()
                .and_downcast::<gtk::ScrolledWindow>()
            {
                scroller.set_max_content_height(height as i32);
                scroller.set_vexpand(false);
            }
            self.stage.add_overlay(&self.inspector);
            self.inspector.set_visible(INFO_OPEN.get());
        } else {
            self.page.remove_css_class("narrow");
            self.stage.remove_overlay(&self.inspector);
            self.inspector.set_valign(gtk::Align::Fill);
            self.inspector.set_vexpand(true);
            if let Some(scroller) = self
                .inspector
                .last_child()
                .and_downcast::<gtk::ScrolledWindow>()
            {
                scroller.set_max_content_height(-1);
                scroller.set_vexpand(true);
            }
            self.inspector.set_visible(true);
            self.split.set_sidebar(Some(&self.inspector));
            self.split.set_show_sidebar(INFO_OPEN.get());
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
                // Um die Fenstermitte: die Tastatur hat keinen Zeiger.
                gdk::Key::plus | gdk::Key::equal | gdk::Key::KP_Add => {
                    viewer.picture.zoom_by(ZOOM_STEP, None)
                }
                gdk::Key::minus | gdk::Key::KP_Subtract => {
                    viewer.picture.zoom_by(1.0 / ZOOM_STEP, None)
                }
                gdk::Key::_0 | gdk::Key::KP_0 => viewer.picture.fit(),
                gdk::Key::_1 | gdk::Key::KP_1 => viewer.picture.actual_size(None),
                gdk::Key::f | gdk::Key::F11 => viewer.toggle_fullscreen(),
                gdk::Key::i | gdk::Key::I => viewer.toggle_info(),
                gdk::Key::e | gdk::Key::E if crate::retouch::available() => {
                    let _ = WidgetExt::activate_action(&viewer.page, "viewer.retouch", None);
                }
                // Vor dem Kürzel des Rasters: im Betrachter gilt das offene Foto.
                gdk::Key::Delete | gdk::Key::KP_Delete => {
                    let _ = WidgetExt::activate_action(&viewer.page, "viewer.delete", None);
                }
                gdk::Key::Escape => {
                    // Erst was über dem Foto liegt, dann das Vollbild, dann
                    // zurueck ins Raster — sonst waere das Foto weg, bevor man
                    // wieder etwas sieht.
                    if viewer.narrow.get() && INFO_OPEN.get() {
                        viewer.set_info(false);
                    } else if viewer.window.is_fullscreen() {
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
fn connect_actions(viewer: &Rc<Viewer>, group: &gio::SimpleActionGroup) {
    for (name, action) in [
        ("save", UiAction::Save),
        ("copy", UiAction::Clipboard),
        ("open", UiAction::Open),
        ("retouch", UiAction::Retouch),
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

    // Löschen: nachfragen, dann zurück ins Raster und in iCloud löschen.
    let delete = gio::SimpleAction::new("delete", None);
    delete.connect_activate({
        let weak = Rc::downgrade(viewer);
        move |_, _| {
            with(&weak, |viewer| {
                let Some(item) = viewer.item() else {
                    return;
                };
                let close = {
                    let weak = weak.clone();
                    move || with(&weak, |viewer| viewer.close())
                };
                crate::window::delete_photos(&viewer.state, &viewer.window, vec![item.id], close);
            });
        }
    });
    group.add_action(&delete);
}

/// Erklaert, warum ein Video nicht laeuft.
///
/// Wie bei HEIC liegt es fast immer an einem Paket, nicht an der Datei: GTK
/// spielt ueber GStreamer ab, und dessen Grundausstattung kann weder `.mov`
/// oeffnen (`qtdemux` steckt in gst-plugins-good) noch HEVC dekodieren — und
/// iPhones filmen in HEVC. Ohne diesen Hinweis sucht man den Fehler bei iCloud.
fn video_failure(message: &str) -> (&'static str, &'static str) {
    let lower = message.to_lowercase();
    let missing_plugin = ["missing a plug-in", "no suitable plugins", "no decoder"]
        .iter()
        .any(|needle| lower.contains(needle));
    if missing_plugin {
        (
            "Video Can’t Be Played",
            "The video is stored locally, but your system is missing the \
             components needed to play iPhone videos. Install:\n\n\
             sudo pacman -S gst-plugins-good gst-libav gst-plugin-va\n\n\
             Then restart the app.",
        )
    } else {
        (
            "Video Can’t Be Played",
            "The file could not be played. See the log for details.",
        )
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_gstreamer_plugins_are_named() {
        // Die Meldung, die GTK auf einem System ohne gst-plugins-good liefert.
        let (_, text) = video_failure("Your GStreamer installation is missing a plug-in.");
        assert!(text.contains("gst-plugins-good"));
        assert!(text.contains("gst-libav"));

        let (_, other) = video_failure("Datei beschädigt");
        assert!(!other.contains("gst-"));
    }
}
