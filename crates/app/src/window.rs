//! Hauptfenster: virtualisiertes Foto-Grid, Viewer und Sync-Steuerung.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use adw::prelude::*;
use gtk::{gdk, gio, glib};
use icloud_photos_cache::Catalog;
use icloud_photos_core::Config;
use std::path::PathBuf;

use crate::downloader::{self, DownloadMessage, Downloader};
use crate::export;
use crate::photo_object::PhotoObject;
use crate::progress::{SyncBanner, thousands};
use crate::sync_task::{self, SyncMessage};
use crate::viewer::{self, Viewer};
use icloud_photos_sync::SyncSummary;

/// So viele Katalogzeilen werden pro Nachladeschritt geholt. Der Katalog landet
/// dadurch nie vollständig im Speicher.
const PAGE_SIZE: u64 = 200;

/// Ab welcher Restscrollhöhe (in Pixeln) die nächste Seite geholt wird.
const PREFETCH_MARGIN: f64 = 600.0;

const THUMB_SIZE: i32 = 180;

/// So oft zieht die Ansicht waehrend eines Syncs nach.
const LIVE_REFRESH: std::time::Duration = std::time::Duration::from_secs(2);

/// So viele Skelett-Kacheln zeigt die Ansicht hoechstens fuer noch nicht
/// synchronisierte Fotos. Mehr als ein paar Bildschirme bringen nichts.
const MAX_PLACEHOLDERS: u64 = 120;

/// Skelett-Optik fuer Kacheln, deren Vorschau noch fehlt.
const SKELETON_CSS: &str = "
.photo-skeleton {
    background-image: linear-gradient(120deg,
        alpha(@theme_fg_color, 0.07),
        alpha(@theme_fg_color, 0.16),
        alpha(@theme_fg_color, 0.07));
    border-radius: 8px;
    animation: photo-skeleton-pulse 1.8s ease-in-out infinite;
}
@keyframes photo-skeleton-pulse {
    0%   { opacity: 0.55; }
    50%  { opacity: 1.0; }
    100% { opacity: 0.55; }
}
/* Video-Kennzeichen: auf jedem Bild lesbar, deshalb eigener Grund. */
.photo-badge {
    background-color: alpha(black, 0.55);
    color: white;
    border-radius: 99px;
    padding: 4px;
}
/* Mitlaufende Datumszeile ueber dem Raster. */
.photo-date {
    background-color: alpha(@theme_bg_color, 0.85);
    border-radius: 99px;
    padding: 4px 14px;
    font-weight: bold;
}
/* Der Fortschrittsstreifen unter der Kopfzeile. */
.sync-banner {
    background-color: alpha(@accent_bg_color, 0.13);
    border-bottom: 1px solid alpha(@theme_fg_color, 0.12);
}
.sync-banner progressbar trough,
.sync-banner progressbar progress {
    min-height: 6px;
}
/* Ausgewaehlte Kacheln sollen als solche erkennbar sein. */
gridview > child:selected picture {
    outline: 3px solid @accent_bg_color;
    outline-offset: -3px;
}
";

/// Geteilter UI-Zustand.
pub struct AppState {
    /// Veränderbar, weil die Anmeldemaske die Apple-ID setzt.
    pub config: RefCell<Config>,
    pub config_path: PathBuf,
    /// Eigene Verbindung des UI-Threads. Der Sync schreibt über eine zweite.
    pub catalog: RefCell<Catalog>,
    pub model: gio::ListStore,
    /// Passwort aus der Anmeldemaske, bis der Sidecar danach fragt.
    ///
    /// Bleibt nur im Speicher und nur bis zur ersten Verwendung — gespeichert
    /// wird es ausschliesslich im Keyring, und das erledigt der Sidecar.
    pending_password: RefCell<Option<String>>,
    /// Erst wahr, wenn der Sidecar `auth_ok` gemeldet hat — also Passwort und
    /// 2FA durch sind. Vorher bleibt die Anmeldemaske stehen.
    authenticated: Cell<bool>,
    /// Ein Sync laeuft, hat aber noch kein `auth_ok` geliefert.
    authenticating: Cell<bool>,
    /// Bedienelemente der Anmeldemaske, um sie waehrend der Anmeldung zu
    /// sperren und Fehler dort anzuzeigen.
    login: RefCell<Option<LoginWidgets>>,
    loaded: Cell<u64>,
    exhausted: Cell<bool>,
    syncing: Cell<bool>,
    /// Bestand laut iCloud, sobald der Sidecar ihn gemeldet hat. Bezugsgroesse
    /// fuer Skelette und Fortschrittsanzeige.
    expected_total: Cell<u64>,
    /// Katalogstand beim letzten Nachziehen, um unnoetige Arbeit zu sparen.
    last_seen_count: Cell<u64>,
    /// Laeuft im Hintergrund und holt Originale auf Abruf.
    downloader: RefCell<Option<Downloader>>,
    /// Der offene Betrachter, damit ein eintreffendes Original dorthin findet.
    viewer: RefCell<Option<Rc<Viewer>>>,
    /// Die beiden Fensterebenen — Bibliothek und Betrachter.
    screens: RefCell<Option<gtk::Stack>>,
    /// Das Raster, um nach dem Schliessen des Betrachters dorthin
    /// zurueckzufinden, wo das Foto lag.
    grid: RefCell<Option<gtk::GridView>>,
    /// Ein laufender Sicherungs-, Kopier- oder Oeffnen-Auftrag.
    job: RefCell<Option<export::Job>>,
    /// Loest Sichern, Kopieren und Oeffnen aus. Vom Hauptfenster eingehaengt,
    /// weil nur dort Dialog und Fortschrittsanzeige sitzen.
    actions: RefCell<Option<ActionHandler>>,
}

/// Die Stelle, die Sichern, Kopieren und Oeffnen ausfuehrt.
type ActionHandler = Rc<dyn Fn(UiAction, Vec<export::Item>)>;

/// Was mit den ausgewaehlten Fotos geschehen soll.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UiAction {
    /// In ein Verzeichnis sichern.
    Save,
    /// In die Zwischenablage legen.
    Clipboard,
    /// Mit der Standardanwendung oeffnen.
    Open,
}

/// Die Teile der Anmeldemaske, die von aussen gesteuert werden.
#[derive(Clone)]
struct LoginWidgets {
    form: gtk::Box,
    error: gtk::Label,
    busy: gtk::Box,
    username: adw::EntryRow,
}

impl AppState {
    pub fn new(config: Config, config_path: PathBuf, catalog: Catalog) -> Rc<Self> {
        Rc::new(Self {
            config: RefCell::new(config),
            config_path,
            catalog: RefCell::new(catalog),
            model: gio::ListStore::new::<PhotoObject>(),
            pending_password: RefCell::new(None),
            authenticated: Cell::new(false),
            authenticating: Cell::new(false),
            login: RefCell::new(None),
            loaded: Cell::new(0),
            exhausted: Cell::new(false),
            syncing: Cell::new(false),
            expected_total: Cell::new(0),
            last_seen_count: Cell::new(0),
            downloader: RefCell::new(None),
            viewer: RefCell::new(None),
            screens: RefCell::new(None),
            grid: RefCell::new(None),
            job: RefCell::new(None),
            actions: RefCell::new(None),
        })
    }

    /// Übernimmt die Apple-ID aus der Anmeldemaske und schreibt sie weg.
    fn set_username(&self, username: &str) -> Result<(), String> {
        self.config.borrow_mut().icloud.username = username.to_string();
        self.config
            .borrow()
            .save(&self.config_path)
            .map_err(|error| error.to_string())
    }

    /// Lädt die nächste Seite in das Modell. Gibt zurück, ob etwas dazukam.
    fn load_next_page(&self) -> bool {
        if self.exhausted.get() {
            return false;
        }
        let offset = self.loaded.get();
        let photos = match self.catalog.borrow().page(offset, PAGE_SIZE) {
            Ok(photos) => photos,
            Err(error) => {
                tracing::error!(%error, "Katalogseite konnte nicht gelesen werden");
                self.exhausted.set(true);
                return false;
            }
        };
        if photos.len() < PAGE_SIZE as usize {
            self.exhausted.set(true);
        }
        if photos.is_empty() {
            return false;
        }
        let objects: Vec<PhotoObject> = photos.iter().map(PhotoObject::new).collect();
        self.model.extend_from_slice(&objects);
        self.loaded.set(offset + photos.len() as u64);
        true
    }

    /// Ob ein Sync ueberhaupt losgeschickt werden kann.
    ///
    /// Auch der Fake-Modus verlangt eine Apple-ID, damit die Anmeldemaske im
    /// Test denselben Weg nimmt wie im Echtbetrieb.
    pub fn sync_configured(&self) -> bool {
        !self.config.borrow().icloud.username.trim().is_empty()
    }

    /// Welche Stack-Seite zum aktuellen Zustand passt.
    ///
    /// Vor der ersten erfolgreichen Anmeldung bleibt die Maske stehen. Wer
    /// bereits Fotos im Katalog hat, sieht sie trotzdem sofort — ein
    /// Hintergrund-Sync soll den Blick auf vorhandene Daten nicht verstellen.
    fn current_page(&self, total: u64) -> &'static str {
        if !self.authenticated.get() && total == 0 {
            return "login";
        }
        // Waehrend eines laufenden Syncs zeigt das Grid Skelette — "Noch keine
        // Fotos" waere dann schlicht falsch.
        if total == 0 && !self.syncing.get() {
            return "empty";
        }
        "grid"
    }

    /// Sperrt die Maske und zeigt den Fortschritt, oder gibt sie wieder frei.
    fn login_busy(&self, busy: bool) {
        if let Some(widgets) = self.login.borrow().as_ref() {
            widgets.form.set_sensitive(!busy);
            widgets.busy.set_visible(busy);
            if busy {
                widgets.error.set_visible(false);
            }
        }
    }

    /// Zeigt einen Fehler in der Maske und gibt sie wieder frei.
    fn login_error(&self, message: &str) {
        self.authenticating.set(false);
        self.login_busy(false);
        if let Some(widgets) = self.login.borrow().as_ref() {
            widgets.error.set_label(message);
            widgets.error.set_visible(true);
            widgets.username.grab_focus();
        }
    }

    /// Fordert das Original eines Fotos an.
    pub fn request_original(&self, id: &str) {
        if let Some(downloader) = self.downloader.borrow().as_ref() {
            downloader.request(id);
        } else {
            tracing::warn!("kein Download-Thread — Original kann nicht geholt werden");
        }
    }

    /// Holt ein Original im Voraus — ohne es als benutzt zu zaehlen.
    pub fn prefetch_original(&self, id: &str) {
        if let Some(downloader) = self.downloader.borrow().as_ref() {
            downloader.prefetch(id);
        }
    }

    /// Vermerkt einen Zugriff auf ein Foto.
    pub fn touch(&self, id: &str) {
        if let Err(error) = self.catalog.borrow().touch(id, chrono_now()) {
            tracing::error!(%error, "last_accessed_at konnte nicht aktualisiert werden");
        }
    }

    /// Loest eine Aktion auf einer Auswahl aus.
    pub fn run_action(&self, action: UiAction, items: Vec<export::Item>) {
        let handler = self.actions.borrow().clone();
        match handler {
            Some(handler) => handler(action, items),
            None => tracing::warn!("keine Aktionsstelle eingehaengt"),
        }
    }

    /// Der Betrachter ist geschlossen worden.
    ///
    /// Die Seite bleibt im Stack stehen; nur der Griff darauf wird
    /// losgelassen, damit der naechste Aufruf sie frisch aufbaut.
    pub fn close_viewer(&self) {
        self.viewer.borrow_mut().take();
    }

    /// Stellt das Raster auf die Position, die der Betrachter zuletzt zeigte.
    pub fn return_to_grid(&self, position: u32) {
        let Some(grid) = self.grid.borrow().clone() else {
            return;
        };
        if position < self.model.n_items() {
            grid.scroll_to(position, gtk::ListScrollFlags::FOCUS, None);
        }
        grid.grab_focus();
    }

    /// Traegt ein eingetroffenes Original ein — im Betrachter wie im
    /// laufenden Auftrag.
    fn deliver_original(&self, id: &str, path: &std::path::Path) {
        let viewer = self.viewer.borrow().clone();
        if let Some(viewer) = viewer {
            viewer.deliver(id, path);
        }
        if let Some(job) = self.job.borrow_mut().as_mut() {
            job.resolve(id, path);
        }
    }

    /// Meldet, dass ein Original nicht zu haben ist.
    fn deliver_failure(&self, id: &str) {
        let viewer = self.viewer.borrow().clone();
        if let Some(viewer) = viewer {
            viewer.fail(id);
        }
        if let Some(job) = self.job.borrow_mut().as_mut() {
            job.reject(id);
        }
    }

    /// Gibt das gemerkte Passwort heraus und vergisst es dabei.
    fn take_password(&self) -> Option<String> {
        self.pending_password.borrow_mut().take()
    }

    /// Nach einer abgelehnten Anmeldung: nichts Falsches weiterverwenden.
    fn reset_credentials(&self) {
        self.pending_password.borrow_mut().take();
        self.authenticated.set(false);
    }

    /// Verwirft das Modell und lädt von vorn — nach einem Sync.
    fn reload(&self) {
        let open = self.viewer_id();
        self.model.remove_all();
        self.loaded.set(0);
        self.exhausted.set(false);
        self.last_seen_count.set(0);
        self.load_next_page();
        self.relocate_viewer(open);
    }

    /// Die ID des Fotos, das der Betrachter gerade zeigt.
    fn viewer_id(&self) -> Option<String> {
        self.viewer
            .borrow()
            .as_ref()
            .and_then(|viewer| viewer.current_id())
    }

    /// Nach einem Umbau des Modells: den Betrachter wieder auf sein Foto
    /// setzen.
    ///
    /// Ein Sync schiebt Fotos mitten ins Raster; ohne dieses Nachziehen zeigte
    /// die nächste Pfeiltaste im Betrachter auf ein anderes Foto als erwartet.
    fn relocate_viewer(&self, id: Option<String>) {
        let (Some(id), Some(viewer)) = (id, self.viewer.borrow().clone()) else {
            return;
        };
        viewer.relocate(&id);
    }

    /// Zieht die sichtbaren Kacheln auf den aktuellen Katalogstand nach.
    ///
    /// Wird waehrend eines Syncs regelmaessig aufgerufen, damit Fotos einlaufen,
    /// statt erst am Ende auf einen Schlag zu erscheinen. Gibt zurueck, ob sich
    /// etwas geaendert hat.
    fn refresh_in_place(&self) -> bool {
        let total = self.catalog.borrow().count().unwrap_or(0);
        if total == self.last_seen_count.get() {
            return false;
        }
        self.last_seen_count.set(total);
        let open = self.viewer_id();

        // So viele Zeilen wie bisher geladen, mindestens eine Seite. Der
        // Katalog ist nach `added_date` sortiert, neue Fotos koennen also
        // mitten hinein fallen — anhaengen reicht nicht.
        let want = self.loaded.get().max(PAGE_SIZE);
        let photos = match self.catalog.borrow().page(0, want) {
            Ok(photos) => photos,
            Err(error) => {
                tracing::error!(%error, "Katalog waehrend des Syncs nicht lesbar");
                return false;
            }
        };

        let mut objects: Vec<PhotoObject> = photos.iter().map(PhotoObject::new).collect();
        self.loaded.set(objects.len() as u64);
        self.exhausted.set((objects.len() as u64) < want);

        objects.extend(self.placeholders(total, objects.len() as u64));
        self.model.splice(0, self.model.n_items(), &objects);
        self.relocate_viewer(open);
        true
    }

    /// Skelette fuer Fotos, die der Sync noch nicht geliefert hat.
    fn placeholders(&self, have: u64, shown: u64) -> Vec<PhotoObject> {
        if !self.syncing.get() {
            return Vec::new();
        }
        let expected = self.expected_total.get();
        let missing = expected.saturating_sub(have);
        // Nur auffuellen, solange die geladene Seite nicht ohnehin schon voll
        // ist — sonst schoeben Skelette die echten Fotos aus dem Blick.
        if missing == 0 || shown >= PAGE_SIZE {
            return Vec::new();
        }
        let count = missing.min(MAX_PLACEHOLDERS).min(PAGE_SIZE - shown);
        (0..count).map(|_| PhotoObject::placeholder()).collect()
    }
}

pub fn build(app: &adw::Application, state: Rc<AppState>) {
    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("iCloud Photos")
        .default_width(1100)
        .default_height(760)
        .build();

    let status = gtk::Label::builder()
        .label("")
        .css_classes(["dim-label"])
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .build();

    let sync_button = gtk::Button::builder()
        .icon_name("view-refresh-symbolic")
        .tooltip_text("Jetzt synchronisieren")
        .build();

    let header = adw::HeaderBar::new();
    header.pack_start(&sync_button);
    header.pack_end(&status);

    let grid = build_grid(&state, &window);

    let scroller = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .child(&grid)
        .build();

    // Mitlaufende Datumszeile. Ein GtkGridView kennt keine Abschnittsköpfe —
    // eine Zeile, die immer den Monat der obersten sichtbaren Reihe zeigt,
    // leistet dasselbe und lässt die Virtualisierung unangetastet.
    let month = gtk::Label::builder()
        .css_classes(["photo-date"])
        .halign(gtk::Align::Center)
        .valign(gtk::Align::Start)
        .margin_top(10)
        .visible(false)
        .build();
    // Sonst finge die Zeile Klicks ab, die den Kacheln darunter gelten.
    month.set_can_target(false);

    let photos = gtk::Overlay::new();
    photos.set_child(Some(&scroller));
    photos.add_overlay(&month);

    let adjustment = scroller.vadjustment();

    // Eine Stelle, die die Datumszeile nachführt — aufgerufen beim Scrollen,
    // beim Einlaufen neuer Fotos und nach einem Sprung.
    let refresh_month: Rc<dyn Fn()> = {
        let grid = grid.clone();
        let month = month.clone();
        let adjustment = adjustment.clone();
        Rc::new(move || update_month(&grid, &month, adjustment.value()))
    };

    // Nachladen, sobald das Ende in Sichtweite kommt.
    adjustment.connect_value_changed({
        let state = state.clone();
        let refresh_month = refresh_month.clone();
        move |adj| {
            let remaining = adj.upper() - (adj.value() + adj.page_size());
            if remaining < PREFETCH_MARGIN {
                state.load_next_page();
            }
            refresh_month();
        }
    });
    // Auch ohne Scrollen soll die Zeile stimmen, sobald Fotos da sind.
    state.model.connect_items_changed({
        let refresh_month = refresh_month.clone();
        move |_, _, _, _| refresh_month()
    });

    let empty = adw::StatusPage::builder()
        .icon_name("image-x-generic-symbolic")
        .title("Noch keine Fotos")
        .description("Starte einen Sync, um den Katalog zu füllen.")
        .build();

    let stack = gtk::Stack::new();
    stack.add_named(&photos, Some("grid"));
    stack.add_named(&empty, Some("empty"));

    let banner = SyncBanner::new();

    let library = gtk::Box::new(gtk::Orientation::Vertical, 0);
    library.append(&header);
    library.append(banner.widget());
    library.append(&stack);

    // Die beiden Ebenen des Fensters. Ein geöffnetes Foto ist keine zweite
    // Fensterebene, sondern diese Seite — auf einem Kachel-Compositor landete
    // ein eigenes Fenster sonst als Kachel neben der App statt über ihr.
    let screens = gtk::Stack::new();
    screens.set_transition_type(gtk::StackTransitionType::Crossfade);
    screens.add_named(&library, Some(viewer::LIBRARY_PAGE));

    // Rückmeldungen zu Sichern und Kopieren gehören nicht in einen Dialog, den
    // man wegklicken muss.
    let toasts = adw::ToastOverlay::new();
    toasts.set_child(Some(&screens));
    window.set_content(Some(&toasts));

    // Damit der Betrachter beim Schliessen dorthin zurückspringen kann, wo das
    // Foto im Raster liegt.
    *state.grid.borrow_mut() = Some(grid.clone());
    *state.screens.borrow_mut() = Some(screens.clone());

    let group = build_selection_actions(&state, &window, &header, &grid, &status, &toasts);

    // Das Raster behält die Tabulatortaste für seine Kacheln — die Kopfzeile
    // ist von dort aus also nicht erreichbar. Deshalb hat die Sprungliste ein
    // eigenes Kürzel.
    let jump = build_jump_button(&state, &grid, &month);
    header.pack_start(&jump);
    let show_jump = gio::SimpleAction::new("jump", None);
    show_jump.connect_activate({
        let jump = jump.clone();
        move |_, _| jump.popup()
    });
    group.add_action(&show_jump);
    app.set_accels_for_action("photos.jump", &["<Control>j"]);

    window.insert_action_group("photos", Some(&group));

    state.load_next_page();

    let trigger: Rc<dyn Fn()> = {
        let state = state.clone();
        let status = status.clone();
        let stack = stack.clone();
        let window = window.clone();
        let sync_button = sync_button.clone();
        let banner = banner.clone();
        Rc::new(move || {
            // Ohne Apple-ID gibt es nichts zu synchronisieren.
            if !state.sync_configured() {
                stack.set_visible_child_name("login");
                return;
            }
            // Laeuft schon einer, wird nicht nachgelegt. Zwei Sidecars auf
            // demselben Katalog waeren nur Doppelarbeit.
            if state.syncing.replace(true) {
                tracing::debug!("Sync laeuft bereits, Anforderung verworfen");
                return;
            }
            sync_button.set_sensitive(false);

            if !state.authenticated.get() {
                // Auch beim Start mit hinterlegtem Passwort: erst nach
                // `auth_ok` gilt die Anmeldung als durch.
                state.authenticating.set(true);
                state.login_busy(true);
            }
            start_sync(
                state.clone(),
                status.clone(),
                stack.clone(),
                window.clone(),
                sync_button.clone(),
                banner.clone(),
            );
        })
    };

    let login = build_login_page(&state, trigger.clone());
    stack.add_named(&login, Some("login"));

    install_skeleton_css();
    start_downloader(&state, &status, &stack, &window, &toasts);

    // Waehrend eines Syncs laufend nachziehen, statt erst am Ende umzuschalten.
    glib::timeout_add_local(LIVE_REFRESH, {
        let state = state.clone();
        let status = status.clone();
        let stack = stack.clone();
        move || {
            if state.syncing.get() && state.refresh_in_place() {
                update_status(&state, &status, &stack);
            }
            glib::ControlFlow::Continue
        }
    });

    sync_button.connect_clicked({
        let trigger = trigger.clone();
        move |_| trigger()
    });

    // Aktualisieren gehört auf die gewohnte Taste, nicht nur auf einen Knopf
    // in der Kopfzeile, den das Raster mit der Tabulatortaste nie erreicht.
    let refresh = gio::SimpleAction::new("sync", None);
    refresh.connect_activate({
        let trigger = trigger.clone();
        move |_, _| trigger()
    });
    group.add_action(&refresh);
    app.set_accels_for_action("photos.sync", &["<Control>r", "F5"]);

    update_status(&state, &status, &stack);
    window.present();

    // Ohne gesetzten Fokus landen Tastatureingaben auf der Werkzeugleiste —
    // Enter loest dann einen Sync aus, statt ein Foto zu oeffnen. Ein leeres
    // Raster kann den Fokus aber nicht halten, deshalb erst, sobald die ersten
    // Fotos da sind.
    let focus_taken = std::cell::Cell::new(false);
    state.model.connect_items_changed({
        let grid = grid.clone();
        move |model, _, _, _| {
            if !focus_taken.get() && model.n_items() > 0 {
                focus_taken.set(true);
                grid.grab_focus();
            }
        }
    });
    if state.model.n_items() > 0 {
        grid.grab_focus();
    }

    if state.sync_configured() {
        // Direkt beim Start einmal syncen, danach im konfigurierten Takt.
        trigger();

        let interval =
            std::time::Duration::from_secs(state.config.borrow().icloud.sync_interval_minutes * 60);
        glib::timeout_add_local(interval, move || {
            trigger();
            glib::ControlFlow::Continue
        });
    }
}

/// Anmeldemaske für den ersten Start: Apple-ID und Passwort.
///
/// Das Passwort geht nie in die Konfiguration — es wird nur bis zur ersten
/// Verwendung im Speicher gehalten und danach vom Sidecar im Keyring abgelegt.
fn build_login_page(state: &Rc<AppState>, trigger: Rc<dyn Fn()>) -> gtk::Widget {
    let username = adw::EntryRow::builder().title("Apple-ID").build();
    username.set_text(&state.config.borrow().icloud.username);

    let password = adw::PasswordEntryRow::builder().title("Passwort").build();

    let fields = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .css_classes(["boxed-list"])
        .build();
    fields.append(&username);
    fields.append(&password);

    // Kein Zeilenumbruch mit `\` im Literal: rustfmt zieht den String wieder
    // auf eine Zeile und die Einrueckung bleibt als Leerzeichenkette im Text.
    let hint = gtk::Label::builder()
        .label(concat!(
            "Das Passwort wird im System-Schlüsselbund gespeichert, nicht in einer Datei. ",
            "Bei aktivierter Zwei-Faktor-Authentifizierung fragt die App gleich danach ",
            "nach dem Code."
        ))
        .wrap(true)
        .max_width_chars(46)
        .justify(gtk::Justification::Center)
        .css_classes(["dim-label", "caption"])
        .build();

    let error = gtk::Label::builder()
        .wrap(true)
        .max_width_chars(46)
        .visible(false)
        .css_classes(["error", "caption"])
        .build();

    let submit = gtk::Button::builder()
        .label("Anmelden")
        .css_classes(["suggested-action", "pill"])
        .halign(gtk::Align::Center)
        .build();

    let form = gtk::Box::new(gtk::Orientation::Vertical, 18);
    form.set_margin_top(12);
    form.append(&fields);
    form.append(&error);
    form.append(&submit);
    form.append(&hint);

    // Eigene Zeile ausserhalb von `form`, damit sie sichtbar bleibt, waehrend
    // das Formular gesperrt ist.
    let spinner = gtk::Spinner::builder().spinning(true).build();
    let busy_label = gtk::Label::builder()
        .label("Anmeldung läuft …")
        .css_classes(["dim-label"])
        .build();
    let busy = gtk::Box::new(gtk::Orientation::Horizontal, 10);
    busy.set_halign(gtk::Align::Center);
    busy.set_visible(false);
    busy.append(&spinner);
    busy.append(&busy_label);

    let column = gtk::Box::new(gtk::Orientation::Vertical, 18);
    column.append(&form);
    column.append(&busy);

    *state.login.borrow_mut() = Some(LoginWidgets {
        form: form.clone(),
        error: error.clone(),
        busy: busy.clone(),
        username: username.clone(),
    });

    // Begrenzt die Formularbreite auf breiten Bildschirmen.
    let clamp = adw::Clamp::builder()
        .maximum_size(420)
        .tightening_threshold(360)
        .child(&column)
        .build();

    let page = adw::StatusPage::builder()
        .icon_name("avatar-default-symbolic")
        .title("Bei iCloud anmelden")
        .description("Nur Lesezugriff — die App lädt keine Fotos hoch und löscht nichts.")
        .child(&clamp)
        .build();

    let submit_action = {
        let state = state.clone();
        let username = username.clone();
        let password = password.clone();
        let error = error.clone();
        move || {
            let id = username.text().trim().to_string();
            if id.is_empty() {
                error.set_label("Bitte eine Apple-ID eingeben.");
                error.set_visible(true);
                return;
            }
            let secret = password.text().to_string();
            if secret.is_empty() {
                error.set_label("Bitte das Passwort eingeben.");
                error.set_visible(true);
                return;
            }

            if let Err(message) = state.set_username(&id) {
                error.set_label(&format!("Konfiguration nicht speicherbar: {message}"));
                error.set_visible(true);
                return;
            }
            *state.pending_password.borrow_mut() = Some(secret);
            // Das Feld sofort leeren — der Klartext soll nicht in einem
            // Widget stehen bleiben.
            password.set_text("");
            error.set_visible(false);

            // Bewusst kein Seitenwechsel: die Maske bleibt stehen, bis der
            // Sidecar `auth_ok` meldet. Sonst landet man im Katalog, obwohl
            // Passwort oder 2FA noch gar nicht bestaetigt sind.
            state.login_busy(true);
            trigger();
        }
    };

    submit.connect_clicked({
        let action = submit_action.clone();
        move |_| action()
    });
    // Enter im Passwortfeld meldet ebenfalls an.
    password.connect_entry_activated({
        let action = submit_action.clone();
        move |_| action()
    });
    // Enter in der Apple-ID springt weiter statt abzuschicken.
    username.connect_entry_activated({
        let password = password.clone();
        move |_| {
            password.grab_focus();
        }
    });

    // Ohne gesetzten Fokus landet die erste Eingabe irgendwo — bei Tab sogar
    // die Apple-ID im Passwortfeld.
    page.connect_map({
        let username = username.clone();
        move |_| {
            username.grab_focus();
        }
    });

    page.upcast()
}

/// Startet den Download-Thread und verarbeitet seine Meldungen in der UI.
fn start_downloader(
    state: &Rc<AppState>,
    status: &gtk::Label,
    stack: &gtk::Stack,
    window: &adw::ApplicationWindow,
    toasts: &adw::ToastOverlay,
) {
    let (downloader, mut messages) =
        downloader::spawn(&state.config.borrow(), sync_task::fake_count());
    *state.downloader.borrow_mut() = Some(downloader);

    let state = state.clone();
    let status = status.clone();
    let stack = stack.clone();
    let window = window.clone();
    let toasts = toasts.clone();

    glib::spawn_future_local(async move {
        while let Some(message) = messages.recv().await {
            match message {
                DownloadMessage::Ready { id, path } => {
                    tracing::info!(%id, path = %path.display(), "Original geladen");
                    state.deliver_original(&id, &path);
                    advance_job(&state, &window, &status, &toasts);
                }
                DownloadMessage::Failed { id, message } => {
                    tracing::warn!(%id, %message, "Original nicht ladbar");
                    state.deliver_failure(&id);
                    advance_job(&state, &window, &status, &toasts);
                }
                DownloadMessage::CacheChanged => {
                    // Die Belegung in der Kopfzeile stimmt nach einer Eviction
                    // nicht mehr.
                    update_status(&state, &status, &stack);
                }
                DownloadMessage::PasswordRequired { responder } => {
                    match ask_secret(
                        &window,
                        "Passwort für iCloud",
                        "Im Schlüsselbund liegt kein Passwort für diese Apple-ID.",
                        SecretKind::Password,
                    )
                    .await
                    {
                        Some(password) => {
                            let _ = responder.send(password);
                        }
                        None => drop(responder),
                    }
                }
                DownloadMessage::TwoFactorRequired { responder } => {
                    match ask_secret(
                        &window,
                        "Zwei-Faktor-Authentifizierung",
                        "Apple hat einen Bestätigungscode an deine Geräte geschickt.",
                        SecretKind::TwoFactor,
                    )
                    .await
                    {
                        Some(code) => {
                            let _ = responder.send(code);
                        }
                        None => drop(responder),
                    }
                }
            }
        }
    });
}

/// Haengt die Skelett-Optik in den Stil des Bildschirms.
fn install_skeleton_css() {
    let provider = gtk::CssProvider::new();
    provider.load_from_string(SKELETON_CSS);
    if let Some(display) = gdk::Display::default() {
        gtk::style_context_add_provider_for_display(
            &display,
            &provider,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    }
}

fn build_grid(state: &Rc<AppState>, window: &adw::ApplicationWindow) -> gtk::GridView {
    let factory = gtk::SignalListItemFactory::new();

    factory.connect_setup(|_, item| {
        let item = item.downcast_ref::<gtk::ListItem>().unwrap();

        let picture = gtk::Picture::builder()
            .content_fit(gtk::ContentFit::Cover)
            .width_request(THUMB_SIZE)
            .height_request(THUMB_SIZE)
            .build();
        picture.add_css_class("card");

        // Das Video-Kennzeichen liegt ueber der Kachel, nicht daneben: sonst
        // verschoebe es das Raster, sobald ein Video darin vorkommt.
        let badge = gtk::Image::builder()
            .icon_name("media-playback-start-symbolic")
            .halign(gtk::Align::End)
            .valign(gtk::Align::End)
            .margin_end(6)
            .margin_bottom(6)
            .css_classes(["photo-badge"])
            .visible(false)
            .build();

        // Die Kachel ist nur das Bild. Ein Dateiname darunter sagt über ein
        // Foto nichts, was man nicht sieht, kostet aber in jeder Reihe eine
        // Zeile Höhe — wer ihn braucht, findet ihn im Tooltip und im
        // Betrachter.
        let overlay = gtk::Overlay::new();
        overlay.set_child(Some(&picture));
        overlay.add_overlay(&badge);
        item.set_child(Some(&overlay));
    });

    factory.connect_bind(|_, item| {
        let item = item.downcast_ref::<gtk::ListItem>().unwrap();
        let Some(photo) = item.item().and_downcast::<PhotoObject>() else {
            return;
        };
        let Some(overlay) = item.child().and_downcast::<gtk::Overlay>() else {
            return;
        };
        let Some((picture, badge)) = tile_parts(&overlay) else {
            return;
        };

        // Die Datumszeile ueber dem Raster liest den Monat der obersten
        // sichtbaren Kachel hier ab.
        overlay.set_widget_name(&photo.month_label());

        if photo.needs_skeleton() {
            // Noch kein Bild da: Platzhalter statt leerer Flaeche, damit die
            // Ansicht von Anfang an ihre spaetere Form hat.
            picture.add_css_class("photo-skeleton");
            picture.set_paintable(gdk::Paintable::NONE);
            badge.set_visible(false);
            overlay.set_tooltip_text(None);
            return;
        }

        picture.remove_css_class("photo-skeleton");
        badge.set_visible(photo.is_video());
        overlay.set_tooltip_text(Some(&format!(
            "{}\n{}",
            photo.filename(),
            photo.captured_label()
        )));
        set_image(&picture, photo.thumb_path().as_deref());
    });

    // Freigeben, sonst hält jede recycelte Zelle ihre Textur fest.
    factory.connect_unbind(|_, item| {
        let item = item.downcast_ref::<gtk::ListItem>().unwrap();
        if let Some(overlay) = item.child().and_downcast::<gtk::Overlay>()
            && let Some((picture, badge)) = tile_parts(&overlay)
        {
            picture.set_paintable(gdk::Paintable::NONE);
            picture.remove_css_class("photo-skeleton");
            badge.set_visible(false);
            overlay.set_tooltip_text(None);
        }
    });

    // Mehrfachauswahl: Strg- und Umschalt-Klick gehören zu einer Bibliothek,
    // aus der man etwas herausholen will.
    let selection = gtk::MultiSelection::new(Some(state.model.clone()));
    let grid = gtk::GridView::builder()
        .model(&selection)
        .factory(&factory)
        .max_columns(12)
        .min_columns(2)
        .margin_top(12)
        .margin_bottom(12)
        .margin_start(12)
        .margin_end(12)
        .build();

    grid.connect_activate({
        let state = state.clone();
        let window = window.clone();
        move |grid, position| {
            let Some(photo) = grid
                .model()
                .and_then(|model| model.item(position))
                .and_downcast::<PhotoObject>()
            else {
                return;
            };
            // Ein Skelett hat noch kein Foto dahinter.
            if photo.is_placeholder() {
                return;
            }
            open_viewer(&state, &window, position);
        }
    });

    grid
}

/// Bild und Video-Kennzeichen einer Kachel.
fn tile_parts(overlay: &gtk::Overlay) -> Option<(gtk::Picture, gtk::Image)> {
    let picture = overlay.child().and_downcast::<gtk::Picture>()?;
    let badge = overlay.last_child().and_downcast::<gtk::Image>()?;
    Some((picture, badge))
}

/// Öffnet den Betrachter als Seite desselben Fensters.
fn open_viewer(state: &Rc<AppState>, parent: &adw::ApplicationWindow, position: u32) {
    let existing = state.viewer.borrow().clone();
    if let Some(viewer) = existing {
        viewer.show(position);
        return;
    }
    let Some(screens) = state.screens.borrow().clone() else {
        tracing::error!("Fensterebenen fehlen — Betrachter kann nicht öffnen");
        return;
    };
    let viewer = viewer::open(state, parent, &screens, position);
    *state.viewer.borrow_mut() = Some(viewer);
}

/// Lädt ein Bild von der Platte; fehlt oder bricht es, bleibt ein Platzhalter.
fn set_image(picture: &gtk::Picture, path: Option<&str>) {
    let texture = path.and_then(|path| match gdk::Texture::from_filename(path) {
        Ok(texture) => Some(texture),
        Err(error) => {
            tracing::debug!(path, %error, "Bild konnte nicht geladen werden");
            None
        }
    });
    match texture {
        Some(texture) => picture.set_paintable(Some(&texture)),
        None => picture.set_paintable(gdk::Paintable::NONE),
    }
}

fn chrono_now() -> chrono::DateTime<chrono::Utc> {
    chrono::Utc::now()
}

fn update_status(state: &Rc<AppState>, status: &gtk::Label, stack: &gtk::Stack) {
    let total = state.catalog.borrow().count().unwrap_or(0);
    let cached = state.catalog.borrow().cached_bytes().unwrap_or(0);
    let limit = state.config.borrow().cache.limit_bytes();

    // Bis zur abgeschlossenen Anmeldung bleibt die Maske stehen.
    stack.set_visible_child_name(state.current_page(total));

    // Den laufenden Fortschritt zeigt der Streifen; hier stehen die Angaben,
    // die auch dann stimmen, wenn gerade nichts passiert.
    let mut line = format!(
        "{} Fotos · Cache {} / {}",
        thousands(total),
        human_bytes(cached),
        human_bytes(limit)
    );
    if let Some(last) = last_sync(state) {
        line.push_str(&format!(" · {}", since(last, chrono_now())));
    }
    status.set_label(&line);
}

fn last_sync(state: &Rc<AppState>) -> Option<chrono::DateTime<chrono::Utc>> {
    state.catalog.borrow().sync_state().ok()?.last_sync_at
}

/// „vor 3 Min" statt eines Zeitstempels — die Frage dahinter ist immer, ob der
/// Stand frisch genug ist, nicht wann genau es war.
fn since(then: chrono::DateTime<chrono::Utc>, now: chrono::DateTime<chrono::Utc>) -> String {
    let minutes = (now - then).num_minutes();
    match minutes {
        // Auch eine Uhr, die kurz rueckwaerts lief, soll etwas Sinnvolles
        // ergeben.
        i64::MIN..=0 => "gerade aktualisiert".to_string(),
        1 => "vor 1 Minute".to_string(),
        2..=59 => format!("vor {minutes} Minuten"),
        60..=119 => "vor 1 Stunde".to_string(),
        120..=1439 => format!("vor {} Stunden", minutes / 60),
        1440..=2879 => "vor 1 Tag".to_string(),
        _ => format!("vor {} Tagen", minutes / 1440),
    }
}

/// Was bei einem Lauf herauskam, in einem Satz.
fn outcome(summary: &SyncSummary) -> String {
    if summary.unchanged {
        // Unveraendert heisst nur: keine neuen Fotos. Der Nachlauf fuer
        // fehlende Vorschauen kann trotzdem gearbeitet haben, und das war bei
        // einer grossen Bibliothek die halbe Minute davor.
        return match summary.thumbnails {
            0 => "Bibliothek unverändert".to_string(),
            count => format!(
                "Bibliothek unverändert · {} Vorschauen geholt",
                thousands(count)
            ),
        };
    }

    let mut parts = Vec::new();
    if summary.stored > 0 {
        parts.push(format!("{} Fotos", thousands(summary.stored)));
    }
    if summary.thumbnails > 0 {
        parts.push(format!("{} Vorschauen", thousands(summary.thumbnails)));
    }
    if summary.deleted > 0 {
        parts.push(format!("{} entfernt", thousands(summary.deleted)));
    }
    if parts.is_empty() {
        return "Abgleich fertig · nichts Neues".to_string();
    }
    format!("Abgleich fertig · {}", parts.join(", "))
}

fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1000.0 && unit < UNITS.len() - 1 {
        value /= 1000.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// Startet den Sync im Hintergrund und verarbeitet seine Meldungen in der UI.
fn start_sync(
    state: Rc<AppState>,
    status: gtk::Label,
    stack: gtk::Stack,
    window: adw::ApplicationWindow,
    sync_button: gtk::Button,
    banner: Rc<SyncBanner>,
) {
    let mut messages = sync_task::spawn(&state.config.borrow(), state.take_password());

    glib::spawn_future_local(async move {
        // Ab der Nachlauf-Meldung zaehlen Vorschauen gegen ihre eigene Menge,
        // nicht mehr gegen den Bibliotheksbestand.
        let mut backfilling = false;

        while let Some(message) = messages.recv().await {
            match message {
                SyncMessage::Started {
                    resuming,
                    full_pass,
                } => {
                    if let Some(offset) = resuming {
                        tracing::info!(offset, "setze abgebrochenen Abgleich fort");
                    }
                    banner.start(resuming, full_pass);
                }
                SyncMessage::Progress { stored } => banner.progress(stored),
                SyncMessage::Thumbnails { stored } => {
                    if backfilling {
                        banner.backfilled(stored);
                    } else {
                        // Waehrend des Abgleichs zaehlen Vorschauen dieselben
                        // Assets wie die Metadaten, nur feiner.
                        banner.progress(stored);
                    }
                }
                SyncMessage::BackfillStarted { missing } => {
                    backfilling = true;
                    banner.backfill(missing);
                }
                SyncMessage::Unchanged => {
                    tracing::debug!("Bibliothek unveraendert");
                }
                SyncMessage::LibraryCount { total } => {
                    // Ab jetzt kennt die Ansicht ihre spaetere Groesse — das
                    // Raster kann die Luecke mit Skeletten fuellen, der Balken
                    // einen Stand statt nur Bewegung zeigen.
                    state.expected_total.set(total);
                    banner.set_expected(total);
                    state.refresh_in_place();
                    update_status(&state, &status, &stack);
                }
                SyncMessage::Deleted { removed } => banner.deleting(removed),
                SyncMessage::Authenticated { trusted } => {
                    // Erst hier ist die Anmeldung wirklich durch — Passwort und
                    // 2FA sind bestaetigt. Jetzt darf die Maske weichen.
                    tracing::info!(trusted, "Anmeldung abgeschlossen");
                    state.authenticated.set(true);
                    state.authenticating.set(false);
                    state.login_busy(false);
                    banner.authenticated();
                    update_status(&state, &status, &stack);
                }
                SyncMessage::PasswordRequired { responder } => {
                    match ask_secret(
                        &window,
                        "Passwort für iCloud",
                        "Im Schlüsselbund liegt kein Passwort für diese Apple-ID.",
                        SecretKind::Password,
                    )
                    .await
                    {
                        Some(password) => {
                            let _ = responder.send(password);
                        }
                        None => drop(responder),
                    }
                }
                SyncMessage::CredentialsRejected { message } => {
                    tracing::warn!(%message, "Anmeldung abgelehnt");
                    state.reset_credentials();
                    state.login_error("Apple hat diese Zugangsdaten abgelehnt.");
                    banner.failed("Anmeldung fehlgeschlagen");
                    update_status(&state, &status, &stack);
                    show_error(
                        &window,
                        "Anmeldung fehlgeschlagen",
                        &format!("Apple hat die Anmeldung abgelehnt: {message}"),
                    );
                }
                SyncMessage::TwoFactorRequired { responder } => {
                    let code = ask_secret(
                        &window,
                        "Zwei-Faktor-Authentifizierung",
                        "Apple hat einen Bestätigungscode an deine Geräte geschickt.",
                        SecretKind::TwoFactor,
                    )
                    .await;
                    match code {
                        Some(code) => {
                            let _ = responder.send(code);
                        }
                        None => {
                            // Der Kanal wird verworfen; der Sync bricht mit
                            // TwoFactorAborted ab.
                            drop(responder);
                        }
                    }
                }
                SyncMessage::Finished(Ok(summary)) => {
                    tracing::info!(
                        stored = summary.stored,
                        thumbnails = summary.thumbnails,
                        failed = summary.thumbnails_failed,
                        "Sync fertig"
                    );
                    banner.finished(&outcome(&summary));
                    state.reload();
                    update_status(&state, &status, &stack);
                }
                SyncMessage::Finished(Err(error)) => {
                    tracing::error!(%error, "Sync fehlgeschlagen");
                    // Scheitert es vor `auth_ok` — abgebrochene 2FA, Netzfehler —
                    // muss die Maske wieder bedienbar sein, sonst haengt der
                    // Nutzer in einem gesperrten Formular fest.
                    if !state.authenticated.get() {
                        state.login_error("Anmeldung nicht abgeschlossen. Bitte erneut versuchen.");
                    }
                    banner.failed("Abgleich fehlgeschlagen");
                    show_error(&window, "Sync fehlgeschlagen", &error);
                    update_status(&state, &status, &stack);
                }
            }
        }
        // Der Kanal ist zu; falls der Sync ohne Meldung endete, nicht in einem
        // gesperrten Formular zurueckbleiben.
        if !state.authenticated.get() {
            state.authenticating.set(false);
            state.login_busy(false);
        }
        state.syncing.set(false);
        sync_button.set_sensitive(true);
    });
}

/// Welche Art Geheimnis abgefragt wird.
enum SecretKind {
    Password,
    TwoFactor,
}

/// Blockierender Dialog für Passwort oder 2FA-Code.
async fn ask_secret(
    parent: &adw::ApplicationWindow,
    heading: &str,
    body: &str,
    kind: SecretKind,
) -> Option<String> {
    let entry: gtk::Widget = match kind {
        SecretKind::Password => gtk::PasswordEntry::builder()
            .show_peek_icon(true)
            .activates_default(true)
            .build()
            .upcast(),
        SecretKind::TwoFactor => gtk::Entry::builder()
            .placeholder_text("6-stelliger Code")
            .input_purpose(gtk::InputPurpose::Digits)
            .max_length(6)
            .activates_default(true)
            .build()
            .upcast(),
    };

    let dialog = adw::AlertDialog::builder()
        .heading(heading)
        .body(body)
        .extra_child(&entry)
        .default_response("ok")
        .close_response("cancel")
        .build();
    dialog.add_response("cancel", "Abbrechen");
    dialog.add_response("ok", "Bestätigen");
    dialog.set_response_appearance("ok", adw::ResponseAppearance::Suggested);

    let response = dialog.choose_future(Some(parent)).await;
    if response != "ok" {
        return None;
    }

    let text = match entry.downcast::<gtk::PasswordEntry>() {
        // Passwoerter werden nicht getrimmt: Leerzeichen koennen dazugehoeren.
        Ok(password) => password.text().to_string(),
        Err(widget) => widget
            .downcast::<gtk::Entry>()
            .expect("Entry oder PasswordEntry")
            .text()
            .trim()
            .to_string(),
    };
    if text.is_empty() { None } else { Some(text) }
}

fn show_error(parent: &adw::ApplicationWindow, heading: &str, body: &str) {
    let dialog = adw::AlertDialog::builder()
        .heading(heading)
        .body(body)
        .build();
    dialog.add_response("ok", "OK");
    dialog.present(Some(parent));
}

// ---------------------------------------------------------------------------
// Datumszeile und Sprungliste
// ---------------------------------------------------------------------------

/// Führt die Datumszeile auf den Monat der obersten sichtbaren Reihe nach.
///
/// Die Kacheln tragen ihren Monat als Widget-Namen (siehe `connect_bind`) —
/// nur die gerade aufgebauten Kacheln existieren überhaupt, und genau die
/// stehen auch im Bild.
fn update_month(grid: &gtk::GridView, label: &gtk::Label, top: f64) {
    match visible_month(grid, top) {
        Some(month) => {
            label.set_label(&month);
            label.set_visible(true);
        }
        // Kein Treffer heisst fast immer: die Kacheln an dieser Stelle sind
        // noch nicht zugeteilt — nach einem Sprung etwa. Die zuletzt bekannte
        // Angabe stehenzulassen ist dann richtiger, als die Zeile wegzunehmen;
        // der nächste Scrollschritt bringt sie ohnehin auf den Stand.
        None => label.set_visible(!label.label().is_empty()),
    }
}

fn visible_month(grid: &gtk::GridView, top: f64) -> Option<String> {
    let mut child = grid.first_child();
    let mut best: Option<(f32, String)> = None;
    while let Some(cell) = child {
        // Angeschnittene Reihen zählen nicht: gesucht ist die oberste Kachel,
        // die wirklich im Bild steht.
        if let Some(bounds) = cell.compute_bounds(grid)
            && (bounds.y() + bounds.height()) as f64 > top + 1.0
            && best.as_ref().is_none_or(|(y, _)| bounds.y() < *y)
            && let Some(tile) = cell.first_child()
        {
            let name = tile.widget_name().to_string();
            if !name.is_empty() {
                best = Some((bounds.y(), name));
            }
        }
        child = cell.next_sibling();
    }
    best.map(|(_, month)| month)
}

/// Knopf mit der Monatsliste der Bibliothek.
fn build_jump_button(
    state: &Rc<AppState>,
    grid: &gtk::GridView,
    month_line: &gtk::Label,
) -> gtk::MenuButton {
    // Eine GtkListBox mit aktivierbaren Zeilen statt einer Liste von Knöpfen:
    // nur so lässt sich die Liste auch mit den Pfeiltasten bedienen.
    let list = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .css_classes(["navigation-sidebar"])
        .build();

    // Zu welcher Rasterposition und welchem Monat jede Zeile gehört.
    let offsets: Rc<RefCell<Vec<u64>>> = Rc::new(RefCell::new(Vec::new()));
    let months: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));

    let scroller = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .min_content_width(220)
        .max_content_height(420)
        .propagate_natural_height(true)
        .child(&list)
        .build();

    let popover = gtk::Popover::builder().child(&scroller).build();
    let button = gtk::MenuButton::builder()
        .icon_name("x-office-calendar-symbolic")
        .tooltip_text("Zu einem Monat springen (Strg+J)")
        .popover(&popover)
        .build();

    list.connect_row_activated({
        let state = state.clone();
        let grid = grid.clone();
        let popover = popover.clone();
        let offsets = offsets.clone();
        let months = months.clone();
        let month_line = month_line.clone();
        move |_, row| {
            let index = row.index().max(0) as usize;
            let Some(&offset) = offsets.borrow().get(index) else {
                return;
            };
            popover.popdown();
            jump_to(&state, &grid, offset);

            // Die Datumszeile liest sonst die sichtbaren Kacheln ab — die gibt
            // es am Ziel erst nach der nächsten Zuteilung. Woher der Sprung
            // kam, ist hier aber ohnehin bekannt.
            if let Some(month) = months.borrow().get(index) {
                month_line.set_label(month);
                month_line.set_visible(true);
            }
        }
    });

    // Erst beim Öffnen füllen: der Katalog wächst, solange ein Sync läuft.
    popover.connect_show({
        let state = state.clone();
        let list = list.clone();
        let months = months.clone();
        move |_| {
            while let Some(row) = list.first_child() {
                list.remove(&row);
            }
            let groups = match state.catalog.borrow().month_groups() {
                Ok(groups) => groups,
                Err(error) => {
                    tracing::error!(%error, "Monatsliste nicht lesbar");
                    return;
                }
            };
            *offsets.borrow_mut() = groups.iter().map(|group| group.offset).collect();
            *months.borrow_mut() = groups
                .iter()
                .map(|group| month_title(&group.month))
                .collect();

            for group in &groups {
                let label = gtk::Label::builder()
                    .label(format!("{}  ·  {}", month_title(&group.month), group.count))
                    .xalign(0.0)
                    .margin_top(8)
                    .margin_bottom(8)
                    .margin_start(12)
                    .margin_end(12)
                    .build();
                let row = gtk::ListBoxRow::builder().child(&label).build();
                list.append(&row);
            }

            // Ohne das bliebe die Liste nach Strg+J ohne Fokus — die
            // Pfeiltasten gingen dann ins Leere.
            if let Some(first) = list.row_at_index(0) {
                first.grab_focus();
            }
        }
    });

    button
}

/// Macht aus `2024-07` ein „Juli 2024".
fn month_title(month: &str) -> String {
    let Some((year, number)) = month.split_once('-') else {
        return month.to_string();
    };
    match number.parse::<u32>() {
        Ok(number) => format!("{} {year}", crate::photo_object::month_name(number)),
        Err(_) => month.to_string(),
    }
}

/// Scrollt an die Position im Raster, notfalls nach Nachladen.
fn jump_to(state: &Rc<AppState>, grid: &gtk::GridView, offset: u64) {
    // Der Sprung kann weit hinter dem liegen, was bisher geladen wurde.
    while (state.model.n_items() as u64) <= offset && state.load_next_page() {}
    let last = state.model.n_items().saturating_sub(1);
    let position = u32::try_from(offset).unwrap_or(last).min(last);
    grid.scroll_to(position, gtk::ListScrollFlags::FOCUS, None);
}

// ---------------------------------------------------------------------------
// Auswahl: sichern, kopieren, öffnen
// ---------------------------------------------------------------------------

/// Hängt die Aktionen für die Auswahl in die Kopfzeile und macht sie auch für
/// den Betrachter erreichbar.
fn build_selection_actions(
    state: &Rc<AppState>,
    window: &adw::ApplicationWindow,
    header: &adw::HeaderBar,
    grid: &gtk::GridView,
    status: &gtk::Label,
    toasts: &adw::ToastOverlay,
) -> gio::SimpleActionGroup {
    let save = gtk::Button::builder()
        .icon_name("document-save-symbolic")
        .tooltip_text("Auswahl sichern …")
        .sensitive(false)
        .build();
    let copy = gtk::Button::builder()
        .icon_name("edit-copy-symbolic")
        .tooltip_text("Auswahl kopieren")
        .sensitive(false)
        .build();
    let open = gtk::Button::builder()
        .icon_name("document-open-symbolic")
        .tooltip_text("Mit Standardanwendung öffnen")
        .sensitive(false)
        .build();

    let actions = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    actions.add_css_class("linked");
    for button in [&save, &copy, &open] {
        actions.append(button);
    }
    header.pack_end(&actions);

    // Die Aktionsstelle: der Betrachter greift über sie auf dieselbe
    // Warteschlange zu wie das Raster.
    let run: ActionHandler = {
        let state = state.clone();
        let window = window.clone();
        let status = status.clone();
        let toasts = toasts.clone();
        Rc::new(move |action, items| {
            start_action(&state, &window, &status, &toasts, action, items);
        })
    };
    *state.actions.borrow_mut() = Some(run.clone());

    // Dieselben drei Wege als Fensteraktionen, damit sie auch über die
    // gewohnten Tastenkürzel erreichbar sind.
    let group = gio::SimpleActionGroup::new();
    for (button, name, action, accelerator) in [
        (&save, "save", UiAction::Save, "<Control>s"),
        (&copy, "copy", UiAction::Clipboard, "<Control>c"),
        (&open, "open", UiAction::Open, "<Control>o"),
    ] {
        let entry = gio::SimpleAction::new(name, None);
        entry.connect_activate({
            let state = state.clone();
            let grid = grid.clone();
            move |_, _| {
                let items = selected_items(&grid);
                if !items.is_empty() {
                    state.run_action(action, items);
                }
            }
        });
        group.add_action(&entry);

        button.connect_clicked({
            let entry = entry.clone();
            move |_| entry.activate(None)
        });
        if let Some(app) = window.application() {
            app.set_accels_for_action(&format!("photos.{name}"), &[accelerator]);
        }
    }

    // Ohne Auswahl gibt es nichts zu sichern.
    if let Some(model) = grid.model() {
        model.connect_selection_changed({
            let save = save.clone();
            let copy = copy.clone();
            let open = open.clone();
            let grid = grid.clone();
            move |_, _, _| {
                let count = selected_items(&grid).len();
                save.set_sensitive(count > 0);
                copy.set_sensitive(count > 0);
                // Mehrere Fotos gleichzeitig zu öffnen ergibt keinen Sinn.
                open.set_sensitive(count == 1);
            }
        });
    }

    group
}

/// Die ausgewählten Fotos, Platzhalter ausgenommen.
fn selected_items(grid: &gtk::GridView) -> Vec<export::Item> {
    let Some(model) = grid.model() else {
        return Vec::new();
    };
    let selection = model.selection();
    let mut items = Vec::new();
    let mut add = |position: u32| {
        if let Some(photo) = model.item(position).and_downcast::<PhotoObject>()
            && !photo.is_placeholder()
        {
            items.push(export::Item {
                id: photo.id(),
                filename: photo.filename(),
            });
        }
    };
    if let Some((iter, first)) = gtk::BitsetIter::init_first(&selection) {
        add(first);
        for position in iter {
            add(position);
        }
    }
    items
}

/// Startet eine Aktion auf einer Auswahl.
fn start_action(
    state: &Rc<AppState>,
    window: &adw::ApplicationWindow,
    status: &gtk::Label,
    toasts: &adw::ToastOverlay,
    action: UiAction,
    items: Vec<export::Item>,
) {
    if items.is_empty() {
        return;
    }
    if state.job.borrow().is_some() {
        toasts.add_toast(adw::Toast::new("Es läuft bereits ein Auftrag."));
        return;
    }

    match action {
        UiAction::Save => {
            // Erst das Ziel, dann die Downloads: wer abbricht, soll nichts
            // angestoßen haben.
            let dialog = gtk::FileDialog::builder()
                .title("Fotos sichern")
                .accept_label("Hierhin sichern")
                .build();
            dialog.select_folder(
                Some(window),
                gio::Cancellable::NONE,
                glib::clone!(
                    #[strong]
                    state,
                    #[strong]
                    status,
                    #[strong]
                    toasts,
                    #[strong]
                    window,
                    move |result| {
                        let Ok(folder) = result else { return };
                        let Some(path) = folder.path() else { return };
                        begin_job(
                            &state,
                            &window,
                            &status,
                            &toasts,
                            export::Action::SaveTo(path),
                            items.clone(),
                        );
                    }
                ),
            );
        }
        UiAction::Clipboard => begin_job(
            state,
            window,
            status,
            toasts,
            export::Action::Clipboard,
            items,
        ),
        UiAction::Open => begin_job(state, window, status, toasts, export::Action::Open, items),
    }
}

/// Legt den Auftrag an und fordert die fehlenden Originale an.
fn begin_job(
    state: &Rc<AppState>,
    window: &adw::ApplicationWindow,
    status: &gtk::Label,
    toasts: &adw::ToastOverlay,
    action: export::Action,
    items: Vec<export::Item>,
) {
    let (job, wanted) = export::Job::new(action, items, |id| {
        state
            .catalog
            .borrow()
            .get(id)
            .ok()
            .flatten()
            .filter(|photo| photo.is_orig_cached)
            .and_then(|photo| photo.orig_path)
            .filter(|path| path.is_file())
    });
    *state.job.borrow_mut() = Some(job);

    // Ein Export ist eine Benutzung: diese Originale sollen in der LRU-Reihe
    // nach hinten rücken, nicht als Prefetch gelten.
    for id in &wanted {
        state.request_original(id);
    }
    advance_job(state, window, status, toasts);
}

/// Zeigt den Fortschritt an und schließt den Auftrag ab, sobald alle
/// Originale beisammen sind.
fn advance_job(
    state: &Rc<AppState>,
    window: &adw::ApplicationWindow,
    status: &gtk::Label,
    toasts: &adw::ToastOverlay,
) {
    let done = match state.job.borrow().as_ref() {
        Some(job) => {
            let (ready, total) = job.progress();
            if !job.is_done() {
                status.set_label(&format!("{} … {ready} / {total}", job.action.verb()));
            }
            job.is_done()
        }
        None => return,
    };
    if !done {
        return;
    }

    let Some(job) = state.job.borrow_mut().take() else {
        return;
    };
    let message = finish_job(window, &job);
    toasts.add_toast(adw::Toast::new(&message));
    status.set_label("");
}

/// Führt den fertigen Auftrag aus und beschreibt, was dabei herauskam.
fn finish_job(window: &adw::ApplicationWindow, job: &export::Job) -> String {
    let files = job.ready();
    if files.is_empty() {
        return "Kein Original verfügbar.".to_string();
    }
    let missing = job.failed();

    match &job.action {
        export::Action::SaveTo(dir) => {
            let (saved, errors) = export::save_into(dir, files);
            for error in &errors {
                tracing::warn!(%error, "Sichern fehlgeschlagen");
            }
            let mut message = match saved {
                1 => "1 Foto gesichert".to_string(),
                other => format!("{other} Fotos gesichert"),
            };
            if missing + errors.len() > 0 {
                message.push_str(&format!(" · {} nicht", missing + errors.len()));
            }
            message
        }
        export::Action::Clipboard => {
            let paths: Vec<gio::File> = files
                .iter()
                .map(|(_, path)| gio::File::for_path(path))
                .collect();
            let list = gdk::FileList::from_array(&paths);
            window
                .clipboard()
                .set_content(Some(&gdk::ContentProvider::for_value(&list.to_value())))
                .unwrap_or_else(
                    |error| tracing::warn!(%error, "Zwischenablage nicht beschreibbar"),
                );
            match files.len() {
                1 => "1 Foto kopiert".to_string(),
                other => format!("{other} Fotos kopiert"),
            }
        }
        export::Action::Open => {
            let (_, path) = &files[0];
            let launcher = gtk::FileLauncher::new(Some(&gio::File::for_path(path)));
            launcher.launch(Some(window), gio::Cancellable::NONE, |result| {
                if let Err(error) = result {
                    tracing::warn!(%error, "Öffnen fehlgeschlagen");
                }
            });
            "Wird geöffnet …".to_string()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::DateTime;

    fn state() -> Rc<AppState> {
        AppState::new(
            Config::default(),
            PathBuf::from("/nonexistent/config.toml"),
            Catalog::open_in_memory().unwrap(),
        )
    }

    #[test]
    fn stays_on_login_until_authentication_completed() {
        let state = state();

        // Frischer Start: noch keine Anmeldung, kein Foto.
        assert_eq!(state.current_page(0), "login");

        // Apple-ID eingetragen reicht nicht — sie ist nur behauptet, nicht geprüft.
        state.config.borrow_mut().icloud.username = "a@b.c".into();
        assert_eq!(state.current_page(0), "login");

        // Auch waehrend der Anmeldung bleibt die Maske stehen.
        state.authenticating.set(true);
        assert_eq!(state.current_page(0), "login");

        // Erst `auth_ok` gibt den Weg frei.
        state.authenticated.set(true);
        assert_eq!(state.current_page(0), "empty");
        assert_eq!(state.current_page(7), "grid");
    }

    #[test]
    fn syncing_shows_the_grid_instead_of_an_empty_page() {
        let state = state();
        state.authenticated.set(true);

        // Ohne Sync ist "noch keine Fotos" richtig.
        assert_eq!(state.current_page(0), "empty");

        // Waehrend eines Syncs stehen dort Skelette.
        state.syncing.set(true);
        assert_eq!(state.current_page(0), "grid");
    }

    #[test]
    fn placeholders_fill_the_gap_to_the_expected_total() {
        let state = state();
        state.syncing.set(true);
        state.expected_total.set(19_624);

        // 12 Fotos da, der Rest wird als Skelett angedeutet — aber gedeckelt.
        // Mehr als ein paar Bildschirme Skelette helfen niemandem.
        let placeholders = state.placeholders(12, 12);
        assert_eq!(placeholders.len() as u64, MAX_PLACEHOLDERS);
        assert!(placeholders.iter().all(|p| p.is_placeholder()));
        assert!(placeholders.iter().all(|p| p.needs_skeleton()));

        // Nah am Seitenende begrenzt stattdessen die Seitengroesse.
        let near_end = state.placeholders(PAGE_SIZE - 20, PAGE_SIZE - 20);
        assert_eq!(near_end.len() as u64, 20);
    }

    #[test]
    fn placeholders_stop_once_a_page_is_full() {
        let state = state();
        state.syncing.set(true);
        state.expected_total.set(19_624);

        // Eine volle Seite echter Fotos: Skelette wuerden sie nur verdraengen.
        assert!(state.placeholders(PAGE_SIZE, PAGE_SIZE).is_empty());
    }

    #[test]
    fn no_placeholders_outside_a_sync() {
        let state = state();
        state.expected_total.set(19_624);
        assert!(state.placeholders(12, 12).is_empty());
    }

    #[test]
    fn no_placeholders_once_everything_arrived() {
        let state = state();
        state.syncing.set(true);
        state.expected_total.set(10);
        assert!(state.placeholders(10, 10).is_empty());
    }

    #[test]
    fn existing_photos_are_shown_before_reauthentication() {
        // Wer beim letzten Mal schon synchronisiert hat, soll seine Fotos
        // sehen, waehrend der Hintergrund-Sync sich neu anmeldet.
        let state = state();
        assert_eq!(state.current_page(120), "grid");
    }

    #[test]
    fn rejected_credentials_send_the_user_back() {
        let state = state();
        state.authenticated.set(true);
        assert_eq!(state.current_page(0), "empty");

        state.reset_credentials();
        assert_eq!(state.current_page(0), "login");
    }

    #[test]
    fn describes_how_long_ago_a_sync_was() {
        let now = DateTime::parse_from_rfc3339("2026-09-12T12:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let ago = |minutes: i64| since(now - chrono::TimeDelta::minutes(minutes), now);

        assert_eq!(ago(0), "gerade aktualisiert");
        assert_eq!(ago(1), "vor 1 Minute");
        assert_eq!(ago(3), "vor 3 Minuten");
        assert_eq!(ago(59), "vor 59 Minuten");
        assert_eq!(ago(60), "vor 1 Stunde");
        assert_eq!(ago(150), "vor 2 Stunden");
        assert_eq!(ago(1_440), "vor 1 Tag");
        assert_eq!(ago(4_320), "vor 3 Tagen");

        // Eine rueckwaerts gestellte Uhr darf keine negative Angabe ergeben.
        assert_eq!(
            since(now + chrono::TimeDelta::minutes(5), now),
            "gerade aktualisiert"
        );
    }

    #[test]
    fn sums_up_what_a_run_did() {
        let mut summary = SyncSummary {
            unchanged: true,
            ..Default::default()
        };
        assert_eq!(outcome(&summary), "Bibliothek unverändert");

        // Nachgeholte Vorschauen sind Arbeit, auch wenn sich sonst nichts tat.
        summary.thumbnails = 300;
        assert_eq!(
            outcome(&summary),
            "Bibliothek unverändert · 300 Vorschauen geholt"
        );
        summary.thumbnails = 0;

        summary.unchanged = false;
        assert_eq!(outcome(&summary), "Abgleich fertig · nichts Neues");

        summary.stored = 1_234;
        summary.thumbnails = 12;
        summary.deleted = 2;
        assert_eq!(
            outcome(&summary),
            "Abgleich fertig · 1.234 Fotos, 12 Vorschauen, 2 entfernt"
        );
    }

    #[test]
    fn formats_each_unit_step() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(999), "999 B");
        assert_eq!(human_bytes(106_220), "106.2 KB");
        assert_eq!(human_bytes(5_000_000), "5.0 MB");
        assert_eq!(human_bytes(20_000_000), "20.0 MB");
        assert_eq!(human_bytes(5_000_000_000), "5.0 GB");
    }
}
