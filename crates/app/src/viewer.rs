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
use crate::zoom_view::{ZOOM_STEP, ZoomView};

/// So viele Nachbarn in jede Richtung werden im Voraus geholt. Einer reicht:
/// er deckt den nächsten Tastendruck ab, ohne die Leitung vollzulaufen.
const PREFETCH_NEIGHBOURS: i32 = 1;

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
    spinner: gtk::Spinner,
    zoom_label: gtk::Label,
    /// Die Fehlerseite — sie bekommt je nach Ursache einen anderen Text.
    failed: adw::StatusPage,
    /// Position im Modell des Rasters.
    position: Cell<u32>,
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
    stack.add_named(&picture, Some("image"));
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
        position: Cell::new(position),
    });

    connect_navigation(&viewer, &previous, &next);
    // Der Maßstab steht nur im Kopf, solange hineingezoomt ist.
    picture.connect_changed({
        let label = viewer.zoom_label.clone();
        move |view| {
            label.set_visible(!view.is_fitted());
            label.set_label(&format!("{:.0} %", view.pixel_scale() * 100.0));
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
        self.picture.reset();

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

        self.failed.set_title("Original nicht verfügbar");
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
fn connect_actions(viewer: &Rc<Viewer>, group: &gio::SimpleActionGroup) {
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
            "Video lässt sich nicht abspielen",
            "Das Video liegt lokal, aber dem System fehlen die Bausteine, um \
             iPhone-Videos abzuspielen. Installiere:\n\n\
             sudo pacman -S gst-plugins-good gst-libav gst-plugin-va\n\n\
             Danach die App neu starten.",
        )
    } else {
        (
            "Video lässt sich nicht abspielen",
            "Die Datei ließ sich nicht abspielen. Einzelheiten stehen im Log.",
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
