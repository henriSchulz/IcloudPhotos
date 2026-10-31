//! Hauptfenster: virtualisiertes Foto-Grid, Viewer und Sync-Steuerung.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet, VecDeque};
use std::rc::Rc;

use adw::prelude::*;
use gtk::{gdk, gio, glib};
use icloud_photos_cache::{Catalog, Scope};
use icloud_photos_core::Config;
use icloud_photos_core::model::Photo;
use std::path::PathBuf;

use crate::chrome::{self, PageHead, SyncRow};
use crate::collections::{Collections, Pick};
use crate::days::{self, DaysView};
use crate::downloader::{self, DownloadMessage, Downloader};
use crate::export;
use crate::geocode::{self, Geocoder};
use crate::home::Home;
use crate::icons::Weight;
use crate::map_view::{self, MapView};
use crate::menu;
use crate::overview::Overview as Overviews;
use crate::photo_object::PhotoObject;
use crate::sidebar::{Sidebar, Target, display_name};
use crate::sync_manager::{Overview, SyncManager, thousands};
use crate::sync_task::{self, SyncMessage};
use crate::viewer::{self, Viewer};
use icloud_photos_sync::SyncSummary;

/// So viele Katalogzeilen werden pro Nachladeschritt geholt. Der Katalog landet
/// dadurch nie vollständig im Speicher.
const PAGE_SIZE: u64 = 200;

/// Ab welcher Restscrollhöhe (in Pixeln) die nächste Seite geholt wird.
const PREFETCH_MARGIN: f64 = 600.0;

const THUMB_SIZE: i32 = 180;
/// Kachelgrößen, zwischen denen zwei Finger im Raster umschalten — wie in
/// Photos auf dem iPad: zusammenziehen zeigt mehr Fotos, spreizen größere.
/// `THUMB_SIZE` ist die mittlere.
const TILE_STEPS: [i32; 5] = [96, 132, THUMB_SIZE, 240, 320];

thread_local! {
    /// Die Kachelgröße als eigenes Stylesheet: so ändert sie sich für alle
    /// Kacheln auf einmal, auch für die, die das Raster gerade recycelt.
    static TILE_CSS: RefCell<Option<gtk::CssProvider>> = const { RefCell::new(None) };
    static TILE_SIZE: Cell<i32> = const { Cell::new(THUMB_SIZE) };
}

thread_local! {
    /// Die Favoriten (Album „Favorites") — für das Herz auf den Kacheln.
    static FAVORITES: RefCell<HashSet<String>> = RefCell::default();
    /// Das Suchfeld der Seitenleiste (der Prüfstand tippt hinein).
    static SEARCH: RefCell<Option<Rc<chrome::SearchField>>> = const { RefCell::new(None) };
    /// „Days", damit eine neue Kachelgröße auch dort ankommt.
    static DAYS_VIEW: RefCell<Option<std::rc::Weak<DaysView>>> = const { RefCell::new(None) };
}

/// Ob ein Foto Favorit ist.
pub fn is_favorite(id: &str) -> bool {
    FAVORITES.with_borrow(|set| set.contains(id))
}

/// Setzt die Kachelgröße des Rasters.
fn set_tile_size(size: i32) {
    TILE_SIZE.set(size);
    if let Some(days) = DAYS_VIEW.with_borrow(|weak| weak.as_ref().and_then(|w| w.upgrade())) {
        days.set_tile_size(size);
    }
    TILE_CSS.with_borrow_mut(|slot| {
        let provider = slot.get_or_insert_with(|| {
            let provider = gtk::CssProvider::new();
            if let Some(display) = gdk::Display::default() {
                gtk::style_context_add_provider_for_display(
                    &display,
                    &provider,
                    gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
                );
            }
            provider
        });
        provider.load_from_string(&format!(
            "picture.photo-tile {{ min-width: {size}px; min-height: {size}px; }}"
        ));
    });
}

/// Die Stufe, die einer Pinch-Geste am nächsten liegt: `start` mal `scale`,
/// gemessen auf logarithmischer Skala (eine Verdopplung ist überall gleich weit).
fn tile_step_for(start: i32, scale: f64) -> i32 {
    let want = (start as f64 * scale).max(1.0).ln();
    *TILE_STEPS
        .iter()
        .min_by(|a, b| {
            let da = ((**a as f64).ln() - want).abs();
            let db = ((**b as f64).ln() - want).abs();
            da.total_cmp(&db)
        })
        .unwrap_or(&THUMB_SIZE)
}

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
    /// Welcher Ausschnitt im Raster steht: die ganze Bibliothek oder ein Album.
    scope: RefCell<Scope>,
    /// Was rechts neben der Seitenleiste steht: das Raster oder eine der
    /// Übersichten.
    content: Cell<Content>,
    loaded: Cell<u64>,
    exhausted: Cell<bool>,
    syncing: Cell<bool>,
    /// Bestand laut iCloud, sobald der Sidecar ihn gemeldet hat. Bezugsgroesse
    /// fuer Skelette und Fortschrittsanzeige.
    expected_total: Cell<u64>,
    /// Katalogstand beim letzten Nachziehen. Um so viel darf das geladene
    /// Fenster wachsen — neue Fotos fallen mitten hinein.
    last_seen_count: Cell<u64>,
    /// Der Sync hat seit dem letzten Nachziehen etwas geschrieben.
    model_dirty: Cell<bool>,
    /// Laeuft im Hintergrund und holt Originale auf Abruf.
    downloader: RefCell<Option<Downloader>>,
    /// Loest Koordinaten in Ortsnamen auf — erst, wenn ein Foto offen ist.
    geocoder: RefCell<Option<Geocoder>>,
    /// Der offene Betrachter, damit ein eintreffendes Original dorthin findet.
    viewer: RefCell<Option<Rc<Viewer>>>,
    /// Die beiden Fensterebenen — Bibliothek und Betrachter.
    screens: RefCell<Option<gtk::Stack>>,
    /// Das Raster, um nach dem Schliessen des Betrachters dorthin
    /// zurueckzufinden, wo das Foto lag.
    grid: RefCell<Option<gtk::GridView>>,
    /// Scrollstand des Rasters. Ob jemand oben steht oder weiter unten liest,
    /// entscheidet, ob ein laufender Sync das Raster umbauen darf.
    scroll: RefCell<Option<gtk::Adjustment>>,
    /// Alles ueber die Synchronisation — getrennt von der Bibliothek.
    sync_manager: RefCell<Option<Rc<SyncManager>>>,
    /// Seitenleiste und Album-Übersicht — beide lesen die Alben aus dem
    /// Katalog und müssen nach einem Album-Sync neu lesen.
    sidebar: RefCell<Option<Rc<Sidebar>>>,
    collections: RefCell<Option<Rc<Collections>>>,
    map_view: RefCell<Option<Rc<MapView>>>,
    /// Ein Ortsname für den Titel eines Kartenausschnitts ist angefragt:
    /// Anfrage-ID und der Titel, in den er gehört.
    map_title: RefCell<Option<(String, adw::WindowTitle)>>,
    /// Das zuletzt gewaehlte Format beim Sichern.
    save_format: Cell<export::Format>,
    /// Ein laufender Sicherungs-, Kopier- oder Oeffnen-Auftrag.
    job: RefCell<Option<export::Job>>,
    /// Loest Sichern, Kopieren und Oeffnen aus. Vom Hauptfenster eingehaengt,
    /// weil nur dort Dialog und Fortschrittsanzeige sitzen.
    actions: RefCell<Option<ActionHandler>>,

    // --- Rahmen nach Vorschlag A (Home) ---------------------------------
    head: RefCell<Option<Rc<PageHead>>>,
    home: RefCell<Option<Rc<Home>>>,
    overview: RefCell<Option<Rc<Overviews>>>,
    /// Welche Übersicht zuletzt gebaut wurde und bei welchem Bestand.
    overview_built: Cell<Option<(Content, u64)>>,
    stack: RefCell<Option<gtk::Stack>>,
    action_bar: RefCell<Option<gtk::Box>>,
    sync_row: RefCell<Option<Rc<SyncRow>>>,
    /// Titel eines Kartenausschnitts (kann aus dem Netz nachkommen).
    place_title: RefCell<Option<adw::WindowTitle>>,
    subset: RefCell<Option<Subset>>,
    zoom: Cell<Zoom>,
    zoom_toggles: RefCell<Vec<(Zoom, gtk::ToggleButton)>>,
    /// Auswahlmodus: ein Klick nimmt ein Foto dazu oder heraus.
    selecting: Cell<bool>,
    /// Der Betrachter kam von der Startseite — zu führt er dorthin zurück.
    home_return: Cell<bool>,
    /// Beschriftung des Zurück-Knopfs im Betrachter („Library", Albumname …).
    pub viewer_back: RefCell<String>,
    context_menu: RefCell<Option<Rc<menu::Menu>>>,
    days_menu: RefCell<Option<Rc<menu::Menu>>>,
    days: RefCell<Option<Rc<DaysView>>>,
    /// Bestand, mit dem „Days" zuletzt gebaut wurde.
    days_built: Cell<Option<u64>>,
    /// Die Favorit-Knöpfe (Aktionsleiste), umbeschriftet je Auswahl.
    favorite_buttons: RefCell<Vec<gtk::Button>>,
    /// Anker für Umschalt-Klick in „Days".
    days_anchor: Cell<Option<u32>>,
}

/// Was statt der Anmeldung im Inhaltsbereich steht.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Content {
    /// Die Startseite mit den Karten.
    Home,
    /// Das Raster — Bibliothek, Album oder ein Ort von der Karte.
    Grid,
    /// Die Bibliothek nach Tagen, mit Kopf je Tag.
    Days,
    /// Die Bibliothek nach Jahren.
    Years,
    /// Die Bibliothek nach Monaten.
    Months,
    /// Die Übersicht „Collections".
    Collections,
    /// Die Karte.
    Map,
}

/// Welche Stufe die Bibliothek zeigt (Years · Months · All im Kopf).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Zoom {
    Years,
    Months,
    Days,
    All,
}

/// Was ein Ausschnitt fester Fotos (`Scope::Photos`) ist — davon hängt der
/// Kopf ab.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Subset {
    /// Eine Stelle der Karte; der Titel kommt aus `place_title`.
    Place,
    /// Treffer einer Suche.
    Search(String),
    /// Alle Fotos eines Tages (von der Startseite).
    Day(chrono::NaiveDate),
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
    /// An Retouch übergeben.
    Retouch,
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
        // Der Stand, gegen den `sync_model` Zuwachs misst. Mit 0 hielte der
        // erste Abgleich den ganzen Katalog fuer neu und laede ihn komplett ins
        // Modell — und jeder weitere Abgleich danach ebenso.
        let known = catalog.count().unwrap_or(0);
        Rc::new(Self {
            config: RefCell::new(config),
            config_path,
            catalog: RefCell::new(catalog),
            model: gio::ListStore::new::<PhotoObject>(),
            pending_password: RefCell::new(None),
            authenticated: Cell::new(false),
            authenticating: Cell::new(false),
            login: RefCell::new(None),
            scope: RefCell::new(Scope::Library),
            content: Cell::new(Content::Home),
            loaded: Cell::new(0),
            exhausted: Cell::new(false),
            syncing: Cell::new(false),
            expected_total: Cell::new(0),
            last_seen_count: Cell::new(known),
            model_dirty: Cell::new(false),
            downloader: RefCell::new(None),
            geocoder: RefCell::new(None),
            viewer: RefCell::new(None),
            screens: RefCell::new(None),
            grid: RefCell::new(None),
            scroll: RefCell::new(None),
            sync_manager: RefCell::new(None),
            sidebar: RefCell::new(None),
            collections: RefCell::new(None),
            map_view: RefCell::new(None),
            map_title: RefCell::new(None),
            save_format: Cell::new(export::Format::Original),
            job: RefCell::new(None),
            actions: RefCell::new(None),
            head: RefCell::new(None),
            home: RefCell::new(None),
            overview: RefCell::new(None),
            overview_built: Cell::new(None),
            stack: RefCell::new(None),
            action_bar: RefCell::new(None),
            sync_row: RefCell::new(None),
            place_title: RefCell::new(None),
            subset: RefCell::new(None),
            zoom: Cell::new(Zoom::Days),
            zoom_toggles: RefCell::new(Vec::new()),
            selecting: Cell::new(false),
            home_return: Cell::new(false),
            viewer_back: RefCell::new(String::new()),
            context_menu: RefCell::new(None),
            days_menu: RefCell::new(None),
            days: RefCell::new(None),
            days_built: Cell::new(None),
            favorite_buttons: RefCell::new(Vec::new()),
            days_anchor: Cell::new(None),
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

    /// Wechselt den Ausschnitt des Rasters — Bibliothek oder ein Album.
    ///
    /// Das Modell wird dabei neu gefüllt, nicht abgeglichen: ein anderes Album
    /// hat mit dem bisherigen Inhalt nichts gemein, ein Diff wäre nur teurer.
    fn set_scope(&self, scope: Scope) {
        if *self.scope.borrow() == scope {
            return;
        }
        let count = self.catalog.borrow().count_in(&scope).unwrap_or(0);
        *self.scope.borrow_mut() = scope;
        self.model.remove_all();
        self.loaded.set(0);
        self.exhausted.set(false);
        self.model_dirty.set(false);
        self.last_seen_count.set(count);
        self.load_next_page();
        if let Some(adjustment) = self.scroll.borrow().as_ref() {
            adjustment.set_value(0.0);
        }
    }

    /// Liest die Alben neu — nach einem Album-Sync und nach jedem Lauf, weil
    /// sich mit neuen Fotos auch die Zahlen in den Alben ändern.
    fn refresh_albums(&self) {
        self.reload_favorites();
        let albums = match self.catalog.borrow().albums() {
            Ok(albums) => albums,
            Err(error) => {
                tracing::error!(%error, "Alben nicht lesbar");
                return;
            }
        };
        if let Some(collections) = self.collections.borrow().as_ref() {
            collections.set_albums(albums.clone());
        }
        // Zuletzt: verschwindet das gewählte Album, wählt die Seitenleiste
        // die Bibliothek und ruft dabei zurück in diesen Zustand.
        let sidebar = self.sidebar.borrow().clone();
        if let Some(sidebar) = sidebar {
            sidebar.set_albums(albums);
        }
    }

    /// Liest die Fotos mit Ort neu und zeigt sie auf der Karte.
    ///
    /// Nur wenn die Karte zu sehen ist; sonst holt ihr nächstes Öffnen das
    /// ohnehin nach.
    fn refresh_map(&self) {
        if self.content.get() != Content::Map {
            return;
        }
        let Some(map) = self.map_view.borrow().clone() else {
            return;
        };
        match self.catalog.borrow().located_photos() {
            Ok(photos) => map.show(photos),
            Err(error) => tracing::error!(%error, "Fotos mit Ort nicht lesbar"),
        }
    }

    /// Lädt die nächste Seite in das Modell. Gibt zurück, ob etwas dazukam.
    fn load_next_page(&self) -> bool {
        if self.exhausted.get() {
            return false;
        }
        let offset = self.loaded.get();
        let scope = self.scope.borrow().clone();
        let photos = match self.catalog.borrow().page_in(&scope, offset, PAGE_SIZE) {
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
        self.loaded.set(offset + photos.len() as u64);

        // Hat ein laufender Sync oben Fotos eingefuegt, die das Raster noch
        // nicht zeigt, verschiebt sich der Katalog gegen das Modell: die neue
        // Seite beginnt dann mit Fotos, die schon im Raster stehen.
        let shown: HashSet<String> = if self.model_dirty.get() {
            (0..self.model.n_items())
                .filter_map(|i| self.model.item(i).and_downcast::<PhotoObject>())
                .map(|object| object.id())
                .collect()
        } else {
            HashSet::new()
        };
        let objects: Vec<PhotoObject> = photos
            .iter()
            .filter(|photo| !shown.contains(photo.id()))
            .map(PhotoObject::new)
            .collect();

        // Vor den Skeletten einfuegen, nicht dahinter.
        let placeholders = (0..self.model.n_items())
            .rev()
            .take_while(|&i| {
                self.model
                    .item(i)
                    .and_downcast::<PhotoObject>()
                    .is_some_and(|o| o.is_placeholder())
            })
            .count() as u32;
        self.model
            .splice(self.model.n_items() - placeholders, 0, &objects);
        !objects.is_empty()
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
        match self.content.get() {
            // Eine leere Bibliothek hat keine Startseite — nur den Hinweis.
            Content::Home if total == 0 && !self.syncing.get() => return "empty",
            Content::Home => return "home",
            Content::Days if total == 0 && !self.syncing.get() => return "empty",
            Content::Days => return "days",
            Content::Years | Content::Months => return "overview",
            Content::Collections => return "collections",
            Content::Map => return "map",
            Content::Grid => {}
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

    /// Der schon bekannte Ortsname zu einer Koordinate, falls einer im
    /// Katalog liegt.
    pub fn known_place(&self, lat: f64, lon: f64) -> Option<String> {
        self.catalog
            .borrow()
            .place(&icloud_photos_cache::place_cell(lat, lon))
            .ok()
            .flatten()
            .flatten()
    }

    /// Lässt einen Ortsnamen nachschlagen.
    pub fn request_place(&self, id: &str, lat: f64, lon: f64) {
        if let Some(geocoder) = self.geocoder.borrow().as_ref() {
            geocoder.request(id, lat, lon);
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
        self.viewer_back.borrow_mut().clear();
        // Von der Startseite geöffnet: dorthin zurück, nicht ins Raster.
        if self.home_return.replace(false) {
            let sidebar = self.sidebar.borrow().clone();
            if let Some(sidebar) = sidebar {
                glib::idle_add_local_once(move || sidebar.select(&Target::Home));
            }
        }
    }

    /// Stellt das Raster auf die Position, die der Betrachter zuletzt zeigte.
    pub fn return_to_grid(&self, position: u32) {
        let Some(grid) = self.grid.borrow().clone() else {
            return;
        };
        if self.content.get() == Content::Days {
            if let Some(days) = self.days.borrow().as_ref() {
                days.scroll_to_offset(u64::from(position));
            }
            return;
        }
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

    /// Bringt das Modell auf den aktuellen Katalogstand — waehrend und nach
    /// einem Sync.
    ///
    /// Frueher wurde dafuer das ganze Modell ersetzt. Damit verlor das Raster
    /// alle zwei Sekunden seinen Halt: jede Kachel wurde neu gebaut, die
    /// Scrollposition sprang, die Auswahl war weg. Jetzt werden nur die
    /// tatsaechlichen Aenderungen eingespielt — ein neues Foto wird an seiner
    /// Stelle eingefuegt, ein geloeschtes herausgenommen, eine eingetroffene
    /// Vorschau am vorhandenen Objekt gesetzt. Alles andere bleibt dasselbe
    /// Objekt, und GTK haelt den Blick auf dem Foto, das gerade oben steht.
    fn sync_model(&self) {
        let scope = self.scope.borrow().clone();
        let total = self.catalog.borrow().count_in(&scope).unwrap_or(0);
        let open = self.viewer_id();
        let was_at_top = self.at_top();

        // Das geladene Fenster darf um die neu hinzugekommenen Fotos wachsen,
        // sonst fielen am Ende Fotos heraus, die gerade noch zu sehen waren.
        let growth = total.saturating_sub(self.last_seen_count.get());
        let want = (self.loaded.get() + growth).max(PAGE_SIZE);
        self.last_seen_count.set(total);
        self.model_dirty.set(false);

        let photos = match self.catalog.borrow().page_in(&scope, 0, want) {
            Ok(photos) => photos,
            Err(error) => {
                tracing::error!(%error, "Katalog nicht lesbar");
                return;
            }
        };

        let real = apply_diff(&self.model, &photos);
        self.loaded.set(real);
        self.exhausted.set(real < want);

        // Skelette stehen immer am Ende; nur ihre Zahl wird angeglichen.
        let wanted = self.placeholders(total, real).len() as u32;
        let current = self.model.n_items() - real as u32;
        if wanted > current {
            let extra: Vec<PhotoObject> = (current..wanted)
                .map(|_| PhotoObject::placeholder())
                .collect();
            self.model.splice(self.model.n_items(), 0, &extra);
        } else if wanted < current {
            self.model.splice(
                real as u32 + wanted,
                current - wanted,
                &[] as &[PhotoObject],
            );
        }

        // GTK haelt beim Einfuegen das bisher oberste Foto fest und scrollt
        // ihm hinterher. Wer oben stand, will aber oben bleiben und die neuen
        // Fotos sehen.
        if was_at_top
            && self.model.n_items() > 0
            && let Some(grid) = self.grid.borrow().as_ref()
        {
            grid.scroll_to(0, gtk::ListScrollFlags::NONE, None);
        }

        self.relocate_viewer(open);
    }

    /// Ob die Ansicht am Anfang der Bibliothek steht.
    fn at_top(&self) -> bool {
        self.scroll
            .borrow()
            .as_ref()
            .is_none_or(|adjustment| adjustment.value() < 1.0)
    }

    /// Nachziehen waehrend eines Syncs, ohne dem Nutzer das Raster unter den
    /// Augen umzubauen.
    ///
    /// Neue Fotos fallen mitten in die sortierte Bibliothek — beim Erstaufbau
    /// sogar fast alle ganz nach oben, weil iCloud alt vor neu liefert. GTK
    /// haelt beim Einfuegen zwar das oberste Foto fest, im Raster fliesst aber
    /// alles um so viele Spalten weiter, wie oben dazukam. Wer weiter unten
    /// liest, saehe die Kacheln alle zwei Sekunden seitwaerts springen.
    ///
    /// Deshalb: Steht die Ansicht oben, laufen neue Fotos live ein und die
    /// Ansicht bleibt oben — wie ein Feed. Steht sie weiter unten, fuellen nur
    /// eintreffende Vorschauen ihre Kacheln; der Umbau wartet, bis der Nutzer
    /// wieder oben ist oder der Sync endet.
    fn refresh_live(&self) {
        if self.at_top() {
            self.sync_model();
            return;
        }
        let window = self.loaded.get().max(PAGE_SIZE);
        let scope = self.scope.borrow().clone();
        let photos = match self.catalog.borrow().page_in(&scope, 0, window) {
            Ok(photos) => photos,
            Err(error) => {
                tracing::error!(%error, "Katalog nicht lesbar");
                return;
            }
        };
        update_in_place(&self.model, &photos);
    }

    // --- Rahmen nach Vorschlag A ------------------------------------------

    /// Seite, Kopf, Seitenleiste und Sync-Stand auf den Zustand bringen.
    pub fn refresh_ui(self: &Rc<Self>) {
        let stack = self.stack.borrow().clone();
        if let Some(stack) = stack {
            update_page(self, &stack);
        }
    }

    /// Baut Years oder Months, wenn sie zu sehen sind und sich der Bestand
    /// geändert hat.
    fn refresh_overview(&self) {
        let content = self.content.get();
        if !matches!(content, Content::Years | Content::Months) {
            return;
        }
        let total = self.catalog.borrow().count().unwrap_or(0);
        if self.overview_built.get() == Some((content, total)) {
            return;
        }
        let Some(overview) = self.overview.borrow().clone() else {
            return;
        };
        let catalog = self.catalog.borrow();
        let groups = catalog.month_groups().unwrap_or_default();
        if content == Content::Years {
            overview.show_years(&catalog, &groups);
        } else {
            overview.show_months(&catalog, &groups);
        }
        self.overview_built.set(Some((content, total)));
    }

    /// Der Kopf der sichtbaren Seite: Titel, Unterzeile, Brotkrume, Knöpfe.
    pub fn refresh_head(&self) {
        let Some(head) = self.head.borrow().clone() else {
            return;
        };
        let toggles = self.zoom_toggles.borrow().clone();
        for (zoom, toggle) in &toggles {
            let on = *zoom == self.zoom.get();
            if toggle.is_active() != on {
                toggle.set_active(on);
            }
        }
        if self.selecting.get() {
            let count = self
                .grid
                .borrow()
                .as_ref()
                .map(|grid| selected_items(grid).len())
                .unwrap_or(0);
            let title = match count {
                0 => "Select Photos".to_string(),
                1 => "1 Photo Selected".to_string(),
                n => format!("{} Photos Selected", thousands(n as u64)),
            };
            head.set(&title, "Click to add or remove · Shift-click for a range");
            head.set_crumb(None);
            head.show_controls("select");
            return;
        }
        let catalog = self.catalog.borrow();
        let library_line = || {
            let summary = catalog.summary_in(&Scope::Library).unwrap_or_default();
            format!(
                "{} · {}",
                crate::home::photos_word(summary.items.saturating_sub(summary.videos)),
                crate::home::videos_word(summary.videos)
            )
        };
        let back = |label: &str, target: Target| -> Option<(String, Rc<dyn Fn()>)> {
            let sidebar = self.sidebar.borrow().clone()?;
            Some((label.to_string(), Rc::new(move || sidebar.select(&target))))
        };
        let mut crumb: Option<(String, Rc<dyn Fn()>)> = None;
        let controls;
        match self.content.get() {
            Content::Home => {
                let today = chrono::Local::now().date_naive();
                let total = catalog.count().unwrap_or(0);
                head.set(
                    "Photos",
                    &format!(
                        "{} · {} {}",
                        crate::home::day_title(today, None)
                            .rsplit_once(", ")
                            .map(|(day, _)| day.to_string())
                            .unwrap_or_default(),
                        thousands(total),
                        if total == 1 {
                            "photo or video"
                        } else {
                            "photos and videos"
                        }
                    ),
                );
                controls = "home";
            }
            Content::Days | Content::Years | Content::Months => {
                head.set("Library", &library_line());
                controls = "library";
            }
            Content::Collections => {
                let albums = catalog.albums().unwrap_or_default();
                let own = albums
                    .iter()
                    .filter(|a| a.kind == icloud_photos_core::model::AlbumKind::Album)
                    .count();
                let folders = albums
                    .iter()
                    .filter(|a| a.kind == icloud_photos_core::model::AlbumKind::Folder)
                    .count();
                let mut line = format!("{own} {}", if own == 1 { "album" } else { "albums" });
                if folders > 0 {
                    line.push_str(&format!(
                        " · {folders} {}",
                        if folders == 1 { "folder" } else { "folders" }
                    ));
                }
                head.set("Collections", &line);
                controls = "simple";
            }
            Content::Map => {
                let located = catalog.located_photos().map(|l| l.len()).unwrap_or(0) as u64;
                head.set(
                    "Map",
                    &format!(
                        "{} with location information",
                        crate::home::photos_word(located)
                    ),
                );
                controls = "simple";
            }
            Content::Grid => {
                let scope = self.scope.borrow().clone();
                match &scope {
                    Scope::Library => {
                        head.set("Library", &library_line());
                        controls = "library";
                    }
                    Scope::Album(id) => {
                        let albums = self
                            .sidebar
                            .borrow()
                            .as_ref()
                            .map(|s| s.albums())
                            .unwrap_or_default();
                        let album = albums.iter().find(|a| &a.id == id);
                        let name = album.map(display_name).unwrap_or_default();
                        let summary = catalog.summary_in(&scope).unwrap_or_default();
                        head.set(&name, &summary_line(&summary));
                        // Eigene Alben führen zurück zu ihrem Ordner oder zu
                        // Collections; intelligente nach Hause.
                        crumb = match album.and_then(|a| a.parent.clone()) {
                            Some(parent) => {
                                let parent_name = albums
                                    .iter()
                                    .find(|a| a.id == parent)
                                    .map(display_name)
                                    .unwrap_or_else(|| "Albums".to_string());
                                back(&parent_name, Target::Folder(parent))
                            }
                            None if album.is_some_and(|a| {
                                a.kind == icloud_photos_core::model::AlbumKind::Smart
                            }) =>
                            {
                                back("Pinned", Target::Home)
                            }
                            None => back("Albums", Target::Collections),
                        };
                        controls = "album";
                    }
                    Scope::Photos(ids) => {
                        let count = ids.len() as u64;
                        match self.subset.borrow().clone() {
                            Some(Subset::Search(query)) => {
                                head.set(
                                    "Search",
                                    &match count {
                                        0 => format!("No results for “{query}”"),
                                        1 => format!("1 result for “{query}”"),
                                        n => format!("{} results for “{query}”", thousands(n)),
                                    },
                                );
                            }
                            Some(Subset::Day(day)) => {
                                head.set(
                                    &crate::home::long_date(day),
                                    &crate::home::items_word(count),
                                );
                                crumb = back("Photos", Target::Home);
                            }
                            _ => {
                                let title = self
                                    .place_title
                                    .borrow()
                                    .as_ref()
                                    .map(|t| t.title().to_string())
                                    .unwrap_or_default();
                                head.set(&title, &crate::home::photos_word(count));
                                crumb = back("Map", Target::Map);
                            }
                        }
                        controls = "album";
                    }
                }
            }
        }
        match &crumb {
            Some((label, action)) => head.set_crumb(Some((label.as_str(), action.clone()))),
            None => head.set_crumb(None),
        }
        head.show_controls(controls);
    }

    /// Liest die Startseite neu aus dem Katalog.
    pub fn refresh_home(&self) {
        let Some(home) = self.home.borrow().clone() else {
            return;
        };
        let today = chrono::Local::now().date_naive();
        let window = crate::velum::photos("onThisDayWindow", 3.0) as u32;
        let strip = crate::velum::photos("strip", 6.0) as u64;
        let days = crate::velum::photos("recentDays", 3.0) as u64;
        let per_day = crate::velum::photos("recentStrip", 5.0) as u64;
        let data = {
            let catalog = self.catalog.borrow();
            let albums = catalog.albums().unwrap_or_default();
            let located = catalog.located_photos().unwrap_or_default();
            let favorites = if albums.iter().any(|a| a.id == "Favorites") {
                catalog
                    .covers_in(&Scope::Album("Favorites".to_string()), 4)
                    .unwrap_or_default()
            } else {
                Vec::new()
            };
            let video_cover = albums
                .iter()
                .find(|a| a.id == "Videos")
                .and_then(|a| a.cover.clone());
            crate::home::Data {
                today: Some(today),
                memory: catalog
                    .on_this_day(today, window, strip + 1)
                    .unwrap_or(None),
                recent: catalog.recent_days(days, per_day).unwrap_or_default(),
                favorites,
                map_cover: located.iter().find_map(|l| l.thumb_path.clone()),
                located: located.len() as u64,
                video_cover,
                videos: catalog.count_videos().unwrap_or(0),
                albums,
            }
        };
        home.show(&data, &self.cloud());
    }

    /// Der Stand für die iCloud-Karte.
    fn cloud(&self) -> crate::home::Cloud {
        let catalog = self.catalog.borrow();
        let used = catalog.cached_bytes().unwrap_or(0);
        let limit = self.config.borrow().cache.limit_bytes();
        let now = chrono_now();
        crate::home::Cloud {
            status: self.sync_manager.borrow().as_ref().map(|m| m.status()),
            last_sync: catalog
                .sync_state()
                .ok()
                .and_then(|s| s.last_sync_at)
                .map(|then| since(then, now)),
            on_device: catalog.count().unwrap_or(0),
            in_icloud: Some(self.expected_total.get()).filter(|total| *total > 0),
            cache_used: used,
            cache_limit: limit,
            cache_label: format!("{} of {}", human_bytes(used), human_bytes(limit)),
        }
    }

    /// Sync-Zeile und iCloud-Karte auf den Stand bringen.
    pub fn refresh_sync_status(&self) {
        let cloud = self.cloud();
        if let (Some(row), Some(status)) = (self.sync_row.borrow().as_ref(), &cloud.status) {
            row.show(status, cloud.last_sync.as_deref());
        }
        if self.content.get() == Content::Home
            && let Some(home) = self.home.borrow().as_ref()
        {
            home.show_cloud(&cloud);
        }
    }

    /// Auswahlmodus an oder aus.
    pub fn set_selecting(&self, on: bool) {
        let was = self.selecting.replace(on);
        if let Some(grid) = self.grid.borrow().as_ref() {
            if on {
                grid.add_css_class("selecting");
            } else {
                grid.remove_css_class("selecting");
                if was && let Some(model) = grid.model() {
                    model.unselect_all();
                }
            }
        }
        if let Some(days) = self.days.borrow().as_ref() {
            days.set_selecting(on);
        }
        self.refresh_action_bar();
        if was != on {
            self.refresh_head();
        }
    }

    /// Liest die Favoriten neu aus dem Katalog.
    fn reload_favorites(&self) {
        let ids = self.catalog.borrow().favorite_ids().unwrap_or_default();
        FAVORITES.with_borrow_mut(|set| *set = ids);
    }

    /// Favorit an oder aus für diese Fotos: sind alle schon Favoriten, gehen
    /// sie heraus, sonst kommen alle hinein — wie in Photos. Sofort im
    /// Katalog und auf dem Bildschirm, dann in iCloud; lehnt iCloud ab, nimmt
    /// `favorite_failed` es zurück.
    pub fn toggle_favorite(self: &Rc<Self>, ids: Vec<String>) {
        if ids.is_empty() {
            return;
        }
        let value = !ids.iter().all(|id| is_favorite(id));
        self.apply_favorite(&ids, value);
        if let Some(downloader) = self.downloader.borrow().as_ref() {
            for id in &ids {
                downloader.favorite(id, value);
            }
        }
    }

    /// iCloud hat abgelehnt: den alten Stand zurück.
    fn favorite_failed(self: &Rc<Self>, id: &str, value: bool) {
        self.apply_favorite(&[id.to_string()], !value);
    }

    fn apply_favorite(self: &Rc<Self>, ids: &[String], value: bool) {
        for id in ids {
            if let Err(error) = self.catalog.borrow().set_favorite(id, value) {
                tracing::error!(%id, %error, "Favorit nicht im Katalog vermerkt");
            }
        }
        FAVORITES.with_borrow_mut(|set| {
            for id in ids {
                if value {
                    set.insert(id.clone());
                } else {
                    set.remove(id);
                }
            }
        });
        // Kacheln des Rasters: über die Eigenschaft, an der ihre Bindung hängt.
        let wanted: HashSet<&str> = ids.iter().map(String::as_str).collect();
        for index in 0..self.model.n_items() {
            if let Some(object) = self.model.item(index).and_downcast::<PhotoObject>()
                && wanted.contains(object.id().as_str())
            {
                object.notify("thumb-path");
            }
        }
        self.refresh_days_marks();
        self.refresh_albums();
        self.refresh_favorite_buttons();
        if self.content.get() == Content::Home {
            self.refresh_home();
        }
        let viewer = self.viewer.borrow().clone();
        if let Some(viewer) = viewer {
            viewer.refresh_favorite();
        }
        // Ein Favoriten-Album im Blick zählt neu.
        if matches!(&*self.scope.borrow(), Scope::Album(id) if id == "Favorites") {
            let state = self.clone();
            glib::idle_add_local_once(move || {
                state.sync_model();
                state.refresh_ui();
            });
        }
    }

    /// „Favorite" oder „Unfavorite" je nach Auswahl.
    fn refresh_favorite_buttons(&self) {
        let ids: Vec<String> = self
            .grid
            .borrow()
            .as_ref()
            .map(|grid| {
                selected_items(grid)
                    .into_iter()
                    .map(|item| item.id)
                    .collect()
            })
            .unwrap_or_default();
        let all = !ids.is_empty() && ids.iter().all(|id| is_favorite(id));
        let label = if all { "Unfavorite" } else { "Favorite" };
        let weight = if all { Weight::Fill } else { Weight::Regular };
        let mut buttons = self.favorite_buttons.borrow().clone();
        for menu in [
            self.context_menu.borrow().clone(),
            self.days_menu.borrow().clone(),
        ]
        .into_iter()
        .flatten()
        {
            buttons.extend(
                menu.big
                    .iter()
                    .filter(|b| b.action_name().as_deref() == Some("photos.favorite"))
                    .cloned(),
            );
        }
        for button in buttons {
            menu::relabel(
                &button,
                "heart",
                weight,
                crate::icons::fallback("heart"),
                label,
            );
        }
    }

    /// Ringe und Herzen der sichtbaren Kacheln in „Days".
    fn refresh_days_marks(&self) {
        let Some(days) = self.days.borrow().clone() else {
            return;
        };
        let selection = self.grid.borrow().as_ref().and_then(|grid| grid.model());
        let selected = |position: u32| {
            selection
                .as_ref()
                .is_some_and(|model| position < model.n_items() && model.is_selected(position))
        };
        days.refresh_marks(&selected, &is_favorite);
    }

    /// Baut „Days", wenn es zu sehen ist und sich der Bestand geändert hat.
    fn refresh_days(&self) {
        if self.content.get() != Content::Days {
            return;
        }
        let total = self.catalog.borrow().count().unwrap_or(0);
        if self.days_built.get() == Some(total) {
            return;
        }
        let Some(days) = self.days.borrow().clone() else {
            return;
        };
        let groups = self
            .catalog
            .borrow()
            .day_groups_in(&Scope::Library)
            .unwrap_or_default();
        days.show(groups);
        self.days_built.set(Some(total));
    }

    /// Lädt das Modell bis zu dieser Rasterposition — „Days" zeigt Fotos, die
    /// das Raster noch nicht geladen hat.
    fn load_until(&self, position: u32) {
        while self.model.n_items() <= position && self.load_next_page() {}
    }

    /// Die Aktionsleiste steht im Auswahlmodus und bei mehreren gewählten
    /// Fotos da (Strg-/Umschalt-Klick).
    fn refresh_action_bar(&self) {
        let count = self
            .grid
            .borrow()
            .as_ref()
            .map(|grid| selected_items(grid).len())
            .unwrap_or(0);
        if let Some(bar) = self.action_bar.borrow().as_ref() {
            let in_grid = matches!(self.content.get(), Content::Grid | Content::Days);
            bar.set_visible(in_grid && (self.selecting.get() || count >= 2));
            bar.set_sensitive(count > 0);
        }
    }

    /// Skelette fuer Fotos, die der Sync noch nicht geliefert hat.
    fn placeholders(&self, have: u64, shown: u64) -> Vec<PhotoObject> {
        // Skelette stehen fuer Fotos der Bibliothek, die noch kommen. In einem
        // Album weiss niemand, wie viele davon hinein gehoeren.
        if !self.syncing.get() || *self.scope.borrow() != Scope::Library {
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

/// Setzt neue Vorschauen an vorhandenen Objekten, ohne das Modell zu
/// veraendern — keine Kachel verschiebt sich.
fn update_in_place(model: &gio::ListStore, photos: &[Photo]) {
    let by_id: HashMap<&str, &Photo> = photos.iter().map(|p| (p.id(), p)).collect();
    for index in 0..model.n_items() {
        let Some(object) = model.item(index).and_downcast::<PhotoObject>() else {
            continue;
        };
        if object.is_placeholder() {
            break;
        }
        if let Some(photo) = by_id.get(object.id().as_str()) {
            object.update_from(photo);
        }
    }
}

/// Spielt die Unterschiede zwischen Modell und `photos` als einzelne
/// Einfuege- und Entfernschritte ein. Gibt die Zahl der echten Fotos zurueck.
///
/// Beide Seiten sind gleich sortiert. Ein Foto, dessen Platz sich geaendert
/// hat — ein korrigiertes Aufnahmedatum —, wird an der alten Stelle
/// entfernt und an der neuen eingefuegt. Skelette am Ende bleiben liegen.
fn apply_diff(model: &gio::ListStore, photos: &[Photo]) -> u64 {
    let object_at = |index: u32| model.item(index).and_downcast::<PhotoObject>();

    let mut real = (0..model.n_items())
        .take_while(|&index| object_at(index).is_some_and(|o| !o.is_placeholder()))
        .count() as u32;

    let wanted: HashSet<&str> = photos.iter().map(|p| p.id()).collect();
    let mut present: HashSet<String> = (0..real)
        .filter_map(|index| object_at(index).map(|o| o.id()))
        .collect();

    let mut index = 0u32;
    let mut next = 0usize;
    while next < photos.len() {
        let photo = &photos[next];
        let current = (index < real).then(|| object_at(index)).flatten();
        match current {
            Some(object) if object.id() == photo.id() => {
                object.update_from(photo);
                index += 1;
                next += 1;
            }
            // Geloescht, oder aus dem geladenen Fenster gefallen.
            Some(object) if !wanted.contains(object.id().as_str()) => {
                present.remove(&object.id());
                model.remove(index);
                real -= 1;
            }
            Some(_) if !present.contains(photo.id()) => {
                // Ein Lauf neuer Fotos auf einmal: ein Schritt statt vieler.
                let run: Vec<PhotoObject> = photos[next..]
                    .iter()
                    .take_while(|p| !present.contains(p.id()))
                    .map(PhotoObject::new)
                    .collect();
                let count = run.len() as u32;
                model.splice(index, 0, &run);
                index += count;
                real += count;
                next += count as usize;
            }
            Some(object) => {
                // Beide gibt es, aber in anderer Reihenfolge: das alte Objekt
                // raus, es kommt an seiner neuen Stelle wieder hinein.
                present.remove(&object.id());
                model.remove(index);
                real -= 1;
            }
            None => {
                let rest: Vec<PhotoObject> = photos[next..].iter().map(PhotoObject::new).collect();
                model.splice(index, 0, &rest);
                index += rest.len() as u32;
                real += rest.len() as u32;
                next = photos.len();
            }
        }
    }
    // Was danach noch an echten Fotos steht, gibt es im Fenster nicht mehr.
    if index < real {
        model.splice(index, real - index, &[] as &[PhotoObject]);
    }
    index as u64
}

pub fn build(app: &adw::Application, state: Rc<AppState>) {
    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("iCloud Photos")
        .default_width(1100)
        .default_height(760)
        .build();
    crate::velum::apply(&window);
    crate::velum::follow_tablet(&window);

    // Fortschritt eines Auftrags (Sichern, Kopieren …) — klein im Kopf.
    let status = gtk::Label::builder()
        .label("")
        .css_classes(["dim-label", "job-status"])
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .valign(gtk::Align::Center)
        .build();

    // Die Bibliothek zeigt nur, wie viele Fotos da sind. Wie die
    // Synchronisation steht, steht im Sync-Manager hinter der Sync-Zeile.
    let manager = SyncManager::new();
    *state.sync_manager.borrow_mut() = Some(manager.clone());

    // Der Kopf jeder Seite: großer Titel, Unterzeile, rechts die Knöpfe.
    let head = PageHead::new();
    *state.head.borrow_mut() = Some(head.clone());
    // Im Tablet-Modus läuft jede App im Vollbild; Fensterknöpfe gibt es auf
    // dem iPad nicht.
    crate::velum::on_tablet({
        let head = head.clone();
        move |on| head.window_controls.set_visible(!on)
    });

    let grid = build_grid(&state, &window);

    let scroller = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .child(&grid)
        .css_classes(["photos-pane", "grid-pane"])
        .build();

    // Zwei Finger ändern die Kachelgröße (Touchscreen und Touchpad). Sie
    // springt zwischen festen Stufen, schon während der Geste — ein Raster,
    // das jeden Zwischenwert neu umbricht, flackert nur.
    crate::velum::on_tablet(|on| {
        let want = if on {
            crate::velum::photos("tileTouch", 180.0)
        } else {
            crate::velum::photos("tile", 132.0)
        };
        set_tile_size(tile_step_for(want as i32, 1.0));
    });
    add_pinch(&scroller);

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
    *state.scroll.borrow_mut() = Some(adjustment.clone());

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
            // Zurueck oben: was der Sync inzwischen gebracht hat, jetzt
            // einspielen. Nicht mitten im Signal — das Einspielen verschiebt
            // selbst den Scrollstand.
            if adj.value() < 1.0 && state.syncing.get() && state.model_dirty.get() {
                let state = state.clone();
                glib::idle_add_local_once(move || state.sync_model());
            }
            refresh_month();
        }
    });
    // Auch ohne Scrollen soll die Zeile stimmen, sobald Fotos da sind. Erst
    // im Leerlauf: waehrend der Meldung tragen die Kacheln noch die Monate
    // ihrer alten Belegung, die Zeile zeigte sonst kurz einen falschen.
    state.model.connect_items_changed({
        let refresh_month = refresh_month.clone();
        move |_, _, _, _| {
            let refresh_month = refresh_month.clone();
            glib::idle_add_local_once(move || refresh_month());
        }
    });

    let empty = adw::StatusPage::builder()
        .icon_name("image-x-generic-symbolic")
        .title("No Photos Yet")
        .description("Start a sync to fill your library.")
        .build();

    let collections = Collections::new();
    *state.collections.borrow_mut() = Some(collections.clone());
    let map_view = MapView::new();
    *state.map_view.borrow_mut() = Some(map_view.clone());

    // Years und Months: ein Klick springt im Raster dorthin.
    let overview = Overviews::new({
        let state = state.clone();
        move |offset| {
            // Aus einem Jahr oder Monat hinein in die Tage, dort wo er beginnt.
            state.zoom.set(Zoom::Days);
            state.content.set(Content::Days);
            state.refresh_ui();
            if let Some(days) = state.days.borrow().as_ref() {
                days.scroll_to_offset(offset);
            }
        }
    });
    *state.overview.borrow_mut() = Some(overview.clone());

    // „Days": Tage mit Kopf; Positionen und Auswahl sind die des Rasters.
    let days_view = build_days(&state, &window, &grid);
    DAYS_VIEW.with_borrow_mut(|slot| *slot = Some(Rc::downgrade(&days_view)));
    add_pinch(&days_view.widget);

    let stack = gtk::Stack::builder()
        .transition_type(gtk::StackTransitionType::Crossfade)
        .transition_duration(crate::velum::duration("fast"))
        .build();
    stack.add_named(&photos, Some("grid"));
    stack.add_named(&empty, Some("empty"));
    stack.add_named(&collections.widget, Some("collections"));
    stack.add_named(&map_view.widget, Some("map"));
    stack.add_named(&overview.widget, Some("overview"));
    stack.add_named(&days_view.widget, Some("days"));
    *state.stack.borrow_mut() = Some(stack.clone());

    // Die schwebende Aktionsleiste der Auswahl, unten in der Mitte.
    let action_bar = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .halign(gtk::Align::Center)
        .valign(gtk::Align::End)
        .css_classes(["photos-actionbar"])
        .visible(false)
        .build();
    for (icon, label, tooltip, action) in [
        (
            "share",
            "Export…",
            "Export Selection… (Ctrl+S)",
            "photos.save",
        ),
        (
            "duplicate",
            "Copy",
            "Copy Selection (Ctrl+C)",
            "photos.copy",
        ),
        (
            "open.external",
            "Open",
            "Open in Default App (Ctrl+O)",
            "photos.open",
        ),
        ("heart", "Favorite", "Favorite", "photos.favorite"),
    ] {
        let button = chrome::bar_button(icon, label, tooltip, action);
        if action == "photos.favorite" {
            state.favorite_buttons.borrow_mut().push(button.clone());
        }
        action_bar.append(&button);
    }
    if crate::retouch::available() {
        action_bar.append(&chrome::bar_button(
            "editor",
            "Retouch",
            "Edit in Retouch",
            "photos.retouch",
        ));
    }
    action_bar.append(&gtk::Separator::new(gtk::Orientation::Vertical));
    let delete_button = chrome::bar_button(
        "trash",
        "Delete…",
        "Delete Selection (Del)",
        "photos.delete",
    );
    delete_button.add_css_class("danger");
    action_bar.append(&delete_button);
    *state.action_bar.borrow_mut() = Some(action_bar.clone());

    let content = gtk::Overlay::builder()
        .child(&stack)
        .vexpand(true)
        .css_classes(["photos-content"])
        .build();
    content.add_overlay(&action_bar);

    let main = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .css_classes(["photos-main"])
        .build();
    main.append(&head.widget);
    main.append(&content);
    // Schmale Seite (`surface.photos.compactBelow`): kleinerer Kopf, Leiste
    // ohne Beschriftung. Gemessen wird die Seite, nicht das Fenster — wie die
    // Container-Abfragen im Lab.
    let main_bin = adw::BreakpointBin::builder()
        .child(&main)
        .width_request(320)
        .height_request(240)
        .build();
    let compact = adw::Breakpoint::new(adw::BreakpointCondition::new_length(
        adw::BreakpointConditionLengthType::MaxWidth,
        crate::velum::photos("compactBelow", 640.0),
        adw::LengthUnit::Px,
    ));
    compact.connect_apply({
        let main = main.clone();
        let head = head.clone();
        move |_| {
            main.add_css_class("compact");
            head.set_compact(true);
        }
    });
    compact.connect_unapply({
        let main = main.clone();
        let head = head.clone();
        move |_| {
            main.remove_css_class("compact");
            head.set_compact(false);
        }
    });
    main_bin.add_breakpoint(compact);

    // Die Seitenleiste: Suche, die Ziele, unten der Stand der Synchronisation.
    let sidebar = Sidebar::new();
    *state.sidebar.borrow_mut() = Some(sidebar.clone());
    let search = chrome::SearchField::new("Search Photos");
    SEARCH.with_borrow_mut(|slot| *slot = Some(search.clone()));
    // In der schmalen Leiste ein Knopf; die Suche öffnet sich dann darüber.
    let search_button =
        crate::icons::menu_button("search", "system-search-symbolic", "Search Photos");
    search_button.add_css_class("flat");
    search_button.add_css_class("rail-search");
    search_button.set_halign(gtk::Align::Center);
    search_button.set_visible(false);
    let rail_search = chrome::SearchField::new("Search Photos");
    rail_search.widget.set_width_request(220);
    search_button.set_popover(Some(
        &gtk::Popover::builder().child(&rail_search.widget).build(),
    ));
    let sync_row = chrome::SyncRow::new(
        {
            let window = window.clone();
            Rc::new(move || {
                let _ = WidgetExt::activate_action(&window, "photos.sync-manager", None);
            })
        },
        {
            let window = window.clone();
            Rc::new(move || {
                let _ = WidgetExt::activate_action(&window, "photos.sync", None);
            })
        },
    );
    *state.sync_row.borrow_mut() = Some(sync_row.clone());
    let panel = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .css_classes(["photos-sidebar"])
        .build();
    panel.append(&search.widget);
    panel.append(&search_button);
    panel.append(&sidebar.widget);
    panel.append(&sync_row.widget);

    // Aufbau wie in Photos: links die Seitenleiste, rechts was sie gewählt
    // hat. Eine OverlaySplitView und keine NavigationSplitView: nur sie lässt
    // sich auch im breiten Fenster ausblenden. Auf schmalen Fenstern legt sie
    // sich über den Inhalt, statt ihn zu verdrängen. Die Breite gibt das
    // Stylesheet vor (`.photos-sidebar` min-width aus Velum).
    let split = adw::OverlaySplitView::builder()
        .sidebar(&panel)
        .content(&main_bin)
        .min_sidebar_width(0.0)
        .max_sidebar_width(1000.0)
        .sidebar_width_fraction(0.0)
        .css_classes(["photos-split"])
        .build();

    // Schmaleres Fenster: erst nur Symbole (`railBelow`), dann eine
    // Schublade, die über die Seite gleitet (`drawerBelow`).
    let set_rail = {
        let sidebar = sidebar.clone();
        let sync_row = sync_row.clone();
        let panel = panel.clone();
        let search = search.clone();
        let search_button = search_button.clone();
        move |rail: bool| {
            sidebar.set_rail(rail);
            sync_row.set_rail(rail);
            search.widget.set_visible(!rail);
            search_button.set_visible(rail);
            if rail {
                panel.add_css_class("rail");
            } else {
                panel.remove_css_class("rail");
            }
        }
    };
    let rail = adw::Breakpoint::new(adw::BreakpointCondition::new_length(
        adw::BreakpointConditionLengthType::MaxWidth,
        crate::velum::photos("railBelow", 900.0),
        adw::LengthUnit::Px,
    ));
    rail.connect_apply({
        let set_rail = set_rail.clone();
        move |_| set_rail(true)
    });
    rail.connect_unapply({
        let set_rail = set_rail.clone();
        move |_| set_rail(false)
    });
    window.add_breakpoint(rail);
    let drawer = adw::Breakpoint::new(adw::BreakpointCondition::new_length(
        adw::BreakpointConditionLengthType::MaxWidth,
        crate::velum::photos("drawerBelow", 620.0),
        adw::LengthUnit::Px,
    ));
    drawer.add_setter(&split, "collapsed", Some(&true.to_value()));
    drawer.add_setter(&split, "show-sidebar", Some(&false.to_value()));
    drawer.add_setter(&head.nav, "visible", Some(&true.to_value()));
    // Die Schublade ist schmaler als die Seite (`surface.photos.drawer`,
    // höchstens `drawerShare` davon); eingeklappt nähme sie sonst alles.
    let drawer_width = crate::velum::photos("drawer", 270.0).min(
        crate::velum::photos("drawerBelow", 620.0) * crate::velum::photos("drawerShare", 0.84),
    );
    drawer.add_setter(&split, "max-sidebar-width", Some(&drawer_width.to_value()));
    drawer.add_setter(&split, "sidebar-width-fraction", Some(&1.0f64.to_value()));
    drawer.connect_apply({
        let set_rail = set_rail.clone();
        let panel = panel.clone();
        move |_| {
            set_rail(false);
            panel.add_css_class("drawer");
        }
    });
    drawer.connect_unapply({
        let panel = panel.clone();
        move |_| panel.remove_css_class("drawer")
    });
    window.add_breakpoint(drawer);
    head.nav.connect_clicked({
        let split = split.clone();
        move |_| split.set_show_sidebar(true)
    });

    // Die beiden Ebenen des Fensters. Ein geöffnetes Foto ist keine zweite
    // Fensterebene, sondern diese Seite — auf einem Kachel-Compositor landete
    // ein eigenes Fenster sonst als Kachel neben der App statt über ihr.
    let screens = gtk::Stack::new();
    screens.set_transition_type(gtk::StackTransitionType::Crossfade);
    screens.add_named(&split, Some(viewer::LIBRARY_PAGE));

    // Der Titel eines Kartenausschnitts kommt womöglich erst aus dem Netz;
    // dieses Objekt hält ihn, der Kopf liest ihn ab.
    let title = adw::WindowTitle::new("", "");
    title.connect_title_notify({
        let state = state.clone();
        move |_| state.refresh_head()
    });
    *state.place_title.borrow_mut() = Some(title.clone());

    sidebar.connect_select({
        let state = state.clone();
        let split = split.clone();
        let empty = empty.clone();
        let search = search.clone();
        move |target| {
            state.map_title.borrow_mut().take();
            *state.subset.borrow_mut() = None;
            state.set_selecting(false);
            if !search.text().is_empty() {
                search.set_text("");
            }
            state.content.set(match target {
                Target::Home => Content::Home,
                Target::Collections | Target::Folder(_) => Content::Collections,
                Target::Map => Content::Map,
                Target::Library => match state.zoom.get() {
                    Zoom::Years => Content::Years,
                    Zoom::Months => Content::Months,
                    Zoom::Days => Content::Days,
                    Zoom::All => Content::Grid,
                },
                Target::Album(_) => Content::Grid,
            });
            match target {
                Target::Home => state.refresh_home(),
                Target::Library => state.set_scope(Scope::Library),
                Target::Album(id) => state.set_scope(Scope::Album(id.clone())),
                Target::Collections => {
                    if let Some(collections) = state.collections.borrow().as_ref() {
                        collections.open_folder(None);
                    }
                }
                Target::Folder(id) => {
                    if let Some(collections) = state.collections.borrow().as_ref() {
                        collections.open_folder(Some(id.clone()));
                    }
                }
                Target::Map => state.refresh_map(),
            }
            if matches!(target, Target::Album(_)) {
                empty.set_title("No Photos in This Album");
                empty.set_description(Some("Photos you add to it in iCloud appear here."));
            } else {
                empty.set_title("No Photos Yet");
                empty.set_description(Some("Start a sync to fill your library."));
            }
            state.refresh_ui();
            // Eingeklappt liegt die Seitenleiste über dem Inhalt; nach der
            // Wahl soll man sehen, was man gewählt hat.
            if split.is_collapsed() {
                split.set_show_sidebar(false);
            }
        }
    });
    collections.connect_pick({
        let sidebar = sidebar.clone();
        move |pick| match pick {
            Pick::Album(id) => sidebar.select(&Target::Album(id)),
            Pick::Folder(id) => sidebar.expand(&id),
        }
    });

    map_view.connect_pick({
        let state = state.clone();
        let title = title.clone();
        let grid = grid.clone();
        move |pick| show_place(&state, &title, &grid, pick)
    });

    // Suche: Dateiname, Ort, Monat oder Jahr — als eigener Ausschnitt.
    let run_search: Rc<dyn Fn(String)> = {
        let state = state.clone();
        let grid = grid.clone();
        Rc::new(move |query: String| {
            let query = query.trim().to_string();
            if query.is_empty() {
                if matches!(*state.subset.borrow(), Some(Subset::Search(_))) {
                    let sidebar = state.sidebar.borrow().clone();
                    if let Some(sidebar) = sidebar {
                        sidebar.select(&Target::Library);
                    }
                }
                return;
            }
            let ids = state
                .catalog
                .borrow()
                .search_ids(&query, SEARCH_LIMIT)
                .unwrap_or_default();
            state.set_selecting(false);
            *state.subset.borrow_mut() = Some(Subset::Search(query));
            state.content.set(Content::Grid);
            state.set_scope(Scope::Photos(ids.into()));
            if let Some(sidebar) = state.sidebar.borrow().as_ref() {
                sidebar.unhighlight();
            }
            state.refresh_ui();
            let _ = &grid;
        })
    };
    search.connect_changed({
        let run_search = run_search.clone();
        move |text| run_search(text)
    });
    rail_search.connect_changed({
        let run_search = run_search.clone();
        move |text| run_search(text)
    });
    search.connect_activate({
        let grid = grid.clone();
        move || {
            grid.grab_focus();
        }
    });

    // Rückmeldungen zu Sichern und Kopieren gehören nicht in einen Dialog, den
    // man wegklicken muss.
    let toasts = adw::ToastOverlay::new();
    toasts.set_child(Some(&screens));
    window.set_content(Some(&toasts));

    // Damit der Betrachter beim Schliessen dorthin zurückspringen kann, wo das
    // Foto im Raster liegt.
    *state.grid.borrow_mut() = Some(grid.clone());
    *state.screens.borrow_mut() = Some(screens.clone());

    let group = build_selection_actions(&state, &window, &grid, &status, &toasts);

    // Die Startseite. Ihre Fotos öffnen sich im Betrachter der Bibliothek; zu
    // ist man danach wieder hier.
    let home = Home::new({
        let state = state.clone();
        let window = window.clone();
        let grid = grid.clone();
        move |event| home_event(&state, &window, &grid, event)
    });
    stack.add_named(&home.widget, Some("home"));
    *state.home.borrow_mut() = Some(home.clone());

    // Das Raster behält die Tabulatortaste für seine Kacheln — die Kopfzeile
    // ist von dort aus also nicht erreichbar. Deshalb hat die Sprungliste ein
    // eigenes Kürzel.
    let jump = build_jump_button(&state, &grid, &month);
    let show_jump = gio::SimpleAction::new("jump", None);
    show_jump.connect_activate({
        let jump = jump.clone();
        move |_, _| jump.popup()
    });
    group.add_action(&show_jump);
    app.set_accels_for_action("photos.jump", &["<Control>j"]);

    let toggle_sidebar = gio::SimpleAction::new("toggle-sidebar", None);
    toggle_sidebar.connect_activate({
        let split = split.clone();
        move |_, _| split.set_show_sidebar(!split.shows_sidebar())
    });
    group.add_action(&toggle_sidebar);
    app.set_accels_for_action("photos.toggle-sidebar", &["F9"]);

    // Die Knöpfe der Köpfe, je Seite ein Satz.
    build_head_controls(&state, &head, &status, &jump);

    window.insert_action_group("photos", Some(&group));

    state.load_next_page();

    let trigger: Rc<dyn Fn()> = {
        let state = state.clone();
        let status = status.clone();
        let stack = stack.clone();
        let window = window.clone();
        let manager = manager.clone();
        let toasts = toasts.clone();
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
                manager.clone(),
                toasts.clone(),
            );
        })
    };

    let login = build_login_page(&state, trigger.clone());
    stack.add_named(&login, Some("login"));

    install_skeleton_css();
    start_downloader(&state, &status, &stack, &window, &toasts);
    start_geocoder(&state);

    // Waehrend eines Syncs laufend nachziehen, statt erst am Ende umzuschalten.
    glib::timeout_add_local(LIVE_REFRESH, {
        let state = state.clone();
        let status = status.clone();
        let stack = stack.clone();
        move || {
            // Solange der Betrachter offen ist, sieht niemand das Raster. Ein
            // Umbau blockierte dann nur den Hauptthread, und ein laufendes
            // Video stockte bei jedem. `model_dirty` bleibt stehen, der
            // naechste Takt nach dem Schliessen holt es nach.
            let viewing = state.viewer.borrow().is_some();
            if state.syncing.get() && state.model_dirty.get() && !viewing {
                state.refresh_live();
                update_status(&state, &status, &stack);
            }
            glib::ControlFlow::Continue
        }
    });
    // „vor 4 Minuten" altert — Sync-Zeile und iCloud-Karte jede Minute frisch.
    glib::timeout_add_seconds_local(60, {
        let state = state.clone();
        move || {
            state.refresh_sync_status();
            glib::ControlFlow::Continue
        }
    });

    manager.set_on_sync(trigger.clone());
    manager.connect_status({
        let state = state.clone();
        move |_| state.refresh_sync_status()
    });

    // Beim Oeffnen frisch: „vor 3 Minuten" altert, waehrend der Manager zu ist.
    let open_manager: Rc<dyn Fn()> = {
        let state = state.clone();
        let status = status.clone();
        let stack = stack.clone();
        let manager = manager.clone();
        let window = window.clone();
        Rc::new(move || {
            update_status(&state, &status, &stack);
            manager.present(&window);
        })
    };
    let show_manager = gio::SimpleAction::new("sync-manager", None);
    show_manager.connect_activate({
        let open_manager = open_manager.clone();
        move |_, _| open_manager()
    });
    group.add_action(&show_manager);
    app.set_accels_for_action("photos.sync-manager", &["<Control><Shift>r"]);
    manager.button().connect_clicked(move |_| open_manager());

    // Aktualisieren gehört auf die gewohnte Taste, nicht nur auf einen Knopf
    // in der Kopfzeile, den das Raster mit der Tabulatortaste nie erreicht.
    let refresh = gio::SimpleAction::new("sync", None);
    refresh.connect_activate({
        let trigger = trigger.clone();
        move |_, _| trigger()
    });
    group.add_action(&refresh);
    app.set_accels_for_action("photos.sync", &["<Control>r", "F5"]);

    state.refresh_albums();
    sidebar.select(&Target::Home);
    update_status(&state, &status, &stack);
    // „open-photo“ mit der Katalog-ID — `icloud-photos --open <id>` (das
    // Foto-Widget der Mitteilungszentrale). Läuft die App schon, kommt die
    // Aktion per D-Bus von der zweiten Instanz.
    let open_photo_action = gio::SimpleAction::new("open-photo", Some(glib::VariantTy::STRING));
    open_photo_action.connect_activate({
        let state = state.clone();
        let window = window.clone();
        move |_, parameter| {
            if let Some(id) = parameter.and_then(|p| p.get::<String>()) {
                open_photo(&state, &window, &id);
            }
        }
    });
    app.add_action(&open_photo_action);

    // Prüfstand: eine Seite oder ein Zustand direkt beim Start
    // (`ICLOUD_PHOTOS_SHOW=library|select|album|map|sync|menu|info|search:…|years|months`),
    // damit sich jeder Zustand ohne Maus fotografieren lässt.
    // Messung: `ICLOUD_PHOTOS_BENCH=days|grid` scrollt die Ansicht Bild für
    // Bild durch und schreibt die Abstände der Bilder ins Log.
    if let Some(which) = std::env::var("ICLOUD_PHOTOS_BENCH")
        .ok()
        .filter(|which| !which.is_empty())
    {
        let state = state.clone();
        glib::timeout_add_local_once(std::time::Duration::from_millis(2500), move || {
            bench_scroll(&state, &which);
        });
    }

    if let Ok(show) = std::env::var("ICLOUD_PHOTOS_SHOW") {
        let state = state.clone();
        let window = window.clone();
        glib::timeout_add_local_once(std::time::Duration::from_millis(600), move || {
            show_for_rig(&state, &window, &show);
        });
    }

    window.present();

    // Ohne gesetzten Fokus landen Tastatureingaben auf der Werkzeugleiste —
    // Enter loest dann einen Sync aus, statt ein Foto zu oeffnen. Ein leeres
    // Raster kann den Fokus aber nicht halten, deshalb erst, sobald die ersten
    // Fotos da sind — und nur, wenn das Raster zu sehen ist.
    let focus_taken = std::cell::Cell::new(false);
    state.model.connect_items_changed({
        let grid = grid.clone();
        let state = state.clone();
        move |model, _, _, _| {
            if !focus_taken.get() && model.n_items() > 0 && state.content.get() == Content::Grid {
                focus_taken.set(true);
                grid.grab_focus();
            }
        }
    });

    // Eine Demo gehört zu keinem Konto: „Sync Now" geht, von selbst nichts.
    if state.sync_configured() && !crate::demo::active() {
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

/// Scrollt „Days" oder das Raster mit fester Geschwindigkeit (je Bild ein
/// Schritt) und misst, wie gleichmäßig die Bilder kommen: Mittel, 95. Perzentil
/// und längster Abstand. Ein Bild über 2 × 16,7 ms ist ein sichtbarer Ruck.
fn bench_scroll(state: &Rc<AppState>, which: &str) {
    let scroller: Option<gtk::ScrolledWindow> = match which {
        "grid" => state
            .grid
            .borrow()
            .as_ref()
            .and_then(|grid| grid.parent())
            .and_downcast::<gtk::ScrolledWindow>(),
        _ => state.days.borrow().as_ref().map(|days| days.widget.clone()),
    };
    let Some(scroller) = scroller else {
        tracing::warn!(which, "Messung: Ansicht fehlt");
        return;
    };
    let step: f64 = std::env::var("ICLOUD_PHOTOS_BENCH_STEP")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(40.0);
    let frames: usize = 900;
    let times: Rc<RefCell<Vec<i64>>> = Rc::default();
    let which = which.to_string();
    let state = state.clone();
    scroller.add_tick_callback(move |widget, clock| {
        let mut times = times.borrow_mut();
        times.push(clock.frame_time());
        let adjustment = widget.vadjustment();
        let end = adjustment.upper() - adjustment.page_size();
        adjustment.set_value((adjustment.value() + step).min(end));
        if times.len() < frames && adjustment.value() < end {
            return glib::ControlFlow::Continue;
        }
        let mut gaps: Vec<f64> = times
            .windows(2)
            .map(|w| (w[1] - w[0]) as f64 / 1000.0)
            .collect();
        gaps.sort_by(|a, b| a.total_cmp(b));
        let n = gaps.len().max(1);
        let mean = gaps.iter().sum::<f64>() / n as f64;
        let p95 = gaps.get(n * 95 / 100).copied().unwrap_or(0.0);
        let max = gaps.last().copied().unwrap_or(0.0);
        let janky = gaps.iter().filter(|gap| **gap > 33.4).count();
        let (tiles, images) = state
            .days
            .borrow()
            .as_ref()
            .map(|d| days::live_tiles(d))
            .unwrap_or((0, 0));
        let (bind_us, load_us, binds) = days::COST.with(|cost| cost.get());
        tracing::info!(
            which,
            bind_ms = bind_us / 1000,
            load_ms = load_us / 1000,
            binds,
            frames = times.len(),
            scrolled = adjustment.value(),
            mean_ms = format!("{mean:.1}"),
            p95_ms = format!("{p95:.1}"),
            max_ms = format!("{max:.1}"),
            janky,
            live_tiles = tiles,
            with_image = images,
            "BENCH"
        );
        glib::ControlFlow::Break
    });
}

/// So viele Treffer zeigt eine Suche höchstens.
const SEARCH_LIMIT: u64 = 5_000;

/// Was ein Klick auf der Startseite auslöst.
fn home_event(
    state: &Rc<AppState>,
    window: &adw::ApplicationWindow,
    grid: &gtk::GridView,
    event: crate::home::Event,
) {
    use crate::home::Event;
    let sidebar = state.sidebar.borrow().clone();
    match event {
        Event::Photo(id) => {
            // Zurück aus dem Betrachter geht es wieder hierher.
            state.home_return.set(true);
            *state.viewer_back.borrow_mut() = "Photos".to_string();
            open_photo(state, window, &id);
        }
        Event::Day(day) => {
            let ids: Vec<String> = state
                .catalog
                .borrow()
                .photos_of_day(day, SEARCH_LIMIT)
                .unwrap_or_default()
                .iter()
                .map(|photo| photo.id().to_string())
                .collect();
            state.set_selecting(false);
            *state.subset.borrow_mut() = Some(Subset::Day(day));
            state.content.set(Content::Grid);
            state.set_scope(Scope::Photos(ids.into()));
            if let Some(sidebar) = &sidebar {
                sidebar.unhighlight();
            }
            state.refresh_ui();
            grid.grab_focus();
        }
        Event::Library => {
            state.zoom.set(Zoom::Days);
            if let Some(sidebar) = &sidebar {
                sidebar.select(&Target::Library);
            }
        }
        Event::Album(id) => {
            if let Some(sidebar) = &sidebar {
                sidebar.select(&Target::Album(id));
            }
        }
        Event::Folder(id) => {
            if let Some(sidebar) = &sidebar {
                sidebar.select(&Target::Folder(id));
            }
        }
        Event::Map => {
            if let Some(sidebar) = &sidebar {
                sidebar.select(&Target::Map);
            }
        }
        Event::Collections => {
            if let Some(sidebar) = &sidebar {
                sidebar.select(&Target::Collections);
            }
        }
        Event::SyncNow => {
            let _ = WidgetExt::activate_action(window, "photos.sync", None);
        }
        Event::SyncDetails => {
            let _ = WidgetExt::activate_action(window, "photos.sync-manager", None);
        }
        Event::AddPhotos => {
            let _ = WidgetExt::activate_action(window, "photos.add", None);
        }
    }
}

/// Die Knopfsätze der Köpfe: Home, Bibliothek (mit Zoomstufen und
/// Monatssprung), Album, schlicht, und die Auswahl.
fn build_head_controls(
    state: &Rc<AppState>,
    head: &Rc<PageHead>,
    status: &gtk::Label,
    jump: &gtk::MenuButton,
) {
    // ⋯: Velums Menü mit Symbolen (siehe `menu.rs`), je Knopf ein eigenes —
    // ein Popover hat genau einen Elternteil.
    let more = || {
        let entries = [
            menu::item(
                "restart",
                "view-refresh-symbolic",
                "Sync Now",
                "photos.sync",
                Some("Ctrl+R"),
            ),
            menu::item(
                "cloud",
                "emblem-synchronizing-symbolic",
                "Sync Status…",
                "photos.sync-manager",
                Some("Shift+Ctrl+R"),
            ),
            menu::Entry::Separator,
            menu::item(
                "plus",
                "list-add-symbolic",
                "Add Photos…",
                "photos.add",
                Some("Shift+Ctrl+I"),
            ),
            menu::item(
                "calendar",
                "x-office-calendar-symbolic",
                "Jump to a Month…",
                "photos.jump",
                Some("Ctrl+J"),
            ),
            menu::item(
                "check",
                "object-select-symbolic",
                "Select Photos",
                "photos.select-mode",
                None,
            ),
            menu::Entry::Separator,
            menu::item(
                "sidebar",
                "sidebar-show-symbolic",
                "Show Sidebar",
                "photos.toggle-sidebar",
                Some("F9"),
            ),
        ];
        let built = menu::build(&[], &entries);
        let button = crate::icons::menu_button("more", "view-more-symbolic", "More");
        button.set_popover(Some(&built.popover));
        button
    };
    let add = || {
        let button = chrome::button(Some("plus"), None, "Add Photos… (Ctrl+Shift+I)");
        button.set_action_name(Some("photos.add"));
        button
    };
    let select = || {
        let button = chrome::button(None, Some("Select"), "Select Photos");
        button.set_action_name(Some("photos.select-mode"));
        button
    };

    // Home: „Add Photos" mit Text, dann Mehr.
    let add_text = chrome::button(
        Some("plus"),
        Some("Add Photos"),
        "Add Photos… (Ctrl+Shift+I)",
    );
    add_text.set_action_name(Some("photos.add"));
    head.add_controls(
        "home",
        &chrome::controls(&[add_text.upcast_ref(), more().upcast_ref()]),
    );

    // Bibliothek: Years · Months · All, Monatssprung, Select, Add, Mehr.
    let segment = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .css_classes(["photos-segment"])
        .valign(gtk::Align::Center)
        .build();
    let mut first: Option<gtk::ToggleButton> = None;
    let mut toggles = Vec::new();
    for (zoom, label) in [
        (Zoom::Years, "Years"),
        (Zoom::Months, "Months"),
        (Zoom::Days, "Days"),
        (Zoom::All, "All"),
    ] {
        let toggle = gtk::ToggleButton::builder().label(label).build();
        if let Some(first) = &first {
            toggle.set_group(Some(first));
        } else {
            first = Some(toggle.clone());
        }
        toggle.connect_toggled({
            let state = state.clone();
            move |toggle| {
                if !toggle.is_active() || state.zoom.get() == zoom {
                    return;
                }
                state.zoom.set(zoom);
                state.content.set(match zoom {
                    Zoom::Years => Content::Years,
                    Zoom::Months => Content::Months,
                    Zoom::Days => Content::Days,
                    Zoom::All => Content::Grid,
                });
                state.set_scope(Scope::Library);
                state.refresh_ui();
            }
        });
        segment.append(&toggle);
        toggles.push((zoom, toggle));
    }
    *state.zoom_toggles.borrow_mut() = toggles;
    jump.add_css_class("wide-only");
    head.add_controls(
        "library",
        &chrome::controls(&[
            status.upcast_ref(),
            segment.upcast_ref(),
            jump.upcast_ref(),
            select().upcast_ref(),
            add().upcast_ref(),
            more().upcast_ref(),
        ]),
    );
    head.add_controls(
        "album",
        &chrome::controls(&[
            select().upcast_ref(),
            add().upcast_ref(),
            more().upcast_ref(),
        ]),
    );
    head.add_controls(
        "simple",
        &chrome::controls(&[add().upcast_ref(), more().upcast_ref()]),
    );

    // Auswahl: alles wählen, fertig.
    let all = chrome::button(None, Some("Select All"), "Select All (Ctrl+A)");
    all.connect_clicked({
        let state = state.clone();
        move |_| {
            // Alles heißt alles, nicht nur die geladenen Seiten.
            while state.load_next_page() {}
            if let Some(model) = state.grid.borrow().as_ref().and_then(|grid| grid.model()) {
                model.select_all();
            }
        }
    });
    let done = chrome::button(None, Some("Done"), "Done (Esc)");
    done.add_css_class("suggested-action");
    done.set_action_name(Some("photos.select-mode"));
    head.add_controls(
        "select",
        &chrome::controls(&[all.upcast_ref(), done.upcast_ref()]),
    );
}

/// Für den Prüfstand: einen Zustand ohne Maus herstellen (siehe
/// `ICLOUD_PHOTOS_SHOW`). Ein Wort pro Zustand, mehrere mit Komma.
fn show_for_rig(state: &Rc<AppState>, window: &adw::ApplicationWindow, show: &str) {
    // Ein Schritt nach dem anderen, mit Luft dazwischen: ein Menü braucht ein
    // Fenster, das schon steht.
    let steps: Vec<String> = show.split(',').map(str::to_string).collect();
    for (index, step) in steps.into_iter().enumerate() {
        let state = state.clone();
        let window = window.clone();
        glib::timeout_add_local_once(
            std::time::Duration::from_millis(500 * index as u64),
            move || show_step(&state, &window, &step),
        );
    }
}

fn show_step(state: &Rc<AppState>, window: &adw::ApplicationWindow, step: &str) {
    let sidebar = state.sidebar.borrow().clone();
    let grid = state.grid.borrow().clone();
    {
        let (word, arg) = step.split_once(':').unwrap_or((step, ""));
        match word {
            "library" | "years" | "months" | "all" => {
                state.zoom.set(match word {
                    "years" => Zoom::Years,
                    "months" => Zoom::Months,
                    "all" => Zoom::All,
                    _ => Zoom::Days,
                });
                if let Some(sidebar) = &sidebar {
                    sidebar.select(&Target::Library);
                }
            }
            "album" => {
                let id = if arg.is_empty() { "demo-lisbon" } else { arg };
                if let Some(sidebar) = &sidebar {
                    sidebar.select(&Target::Album(id.to_string()));
                }
            }
            "map" => {
                if let Some(sidebar) = &sidebar {
                    sidebar.select(&Target::Map);
                }
            }
            "collections" => {
                if let Some(sidebar) = &sidebar {
                    sidebar.select(&Target::Collections);
                }
            }
            "search" => {
                if let Some(field) = SEARCH.with_borrow(|slot| slot.clone()) {
                    field.set_text(arg);
                }
            }
            "select" => {
                let _ = WidgetExt::activate_action(window, "photos.select-mode", None);
                if let Some(model) = grid.as_ref().and_then(|grid| grid.model()) {
                    for position in [7u32, 8, 9, 10] {
                        model.select_item(position, false);
                    }
                }
            }
            "menu" => {
                // Das Menü der sichtbaren Ansicht: „Days" oder das Raster.
                let days = state.content.get() == Content::Days;
                let (parent, menu): (Option<gtk::Widget>, _) = if days {
                    (
                        state
                            .days
                            .borrow()
                            .as_ref()
                            .map(|d| d.list.clone().upcast()),
                        state.days_menu.borrow().clone(),
                    )
                } else {
                    (
                        grid.clone().map(|g| g.upcast()),
                        state.context_menu.borrow().clone(),
                    )
                };
                if let (Some(parent), Some(menu), Some(grid)) = (parent, menu, &grid) {
                    state.load_until(9);
                    if let Some(model) = grid.model() {
                        model.select_item(9, true);
                    }
                    let x = parent.width() as f64 * 0.55;
                    let y = 200.0;
                    menu.popover
                        .set_pointing_to(Some(&gdk::Rectangle::new(x as i32, y as i32, 1, 1)));
                    state.refresh_favorite_buttons();
                    menu.popover.popup();
                }
            }
            "more" => {
                let head = state.head.borrow().clone();
                if let Some(button) = head
                    .and_then(|head| head.controls.visible_child())
                    .and_then(|row| row.last_child())
                    .and_downcast::<gtk::MenuButton>()
                {
                    button.popup();
                }
            }
            "viewermenu" => {
                let viewer = state.viewer.borrow().clone();
                if let Some(viewer) = viewer {
                    viewer.popup_menu();
                }
            }
            "favorite" => {
                let id = if arg.is_empty() { "demo-0001" } else { arg };
                state.toggle_favorite(vec![id.to_string()]);
            }
            "sync" => {
                let _ = WidgetExt::activate_action(window, "photos.sync-manager", None);
            }
            "viewer" | "info" => {
                let id = if arg.is_empty() { "demo-0030" } else { arg };
                if word == "info" {
                    crate::viewer::set_info_open(true);
                }
                open_photo(state, window, id);
            }
            "nav" => {
                let _ = WidgetExt::activate_action(window, "photos.toggle-sidebar", None);
            }
            _ => {}
        }
    }
}

/// Anmeldemaske für den ersten Start: Apple-ID und Passwort.
///
/// Das Passwort geht nie in die Konfiguration — es wird nur bis zur ersten
/// Verwendung im Speicher gehalten und danach vom Sidecar im Keyring abgelegt.
fn build_login_page(state: &Rc<AppState>, trigger: Rc<dyn Fn()>) -> gtk::Widget {
    let username = adw::EntryRow::builder().title("Apple Account").build();
    username.set_text(&state.config.borrow().icloud.username);

    let password = adw::PasswordEntryRow::builder().title("Password").build();

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
            "Your password is stored in the system keyring, never in a file. ",
            "If two-factor authentication is on, the app asks for the code next."
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
        .label("Sign In")
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
        .label("Signing In…")
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
        .title("Sign In to iCloud")
        .description("Read-only — this app never uploads or deletes anything.")
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
                error.set_label("Enter your Apple Account.");
                error.set_visible(true);
                return;
            }
            let secret = password.text().to_string();
            if secret.is_empty() {
                error.set_label("Enter your password.");
                error.set_visible(true);
                return;
            }

            if let Err(message) = state.set_username(&id) {
                error.set_label(&format!("Could not save the configuration: {message}"));
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

/// Startet den Geocoder und trägt eintreffende Ortsnamen in den Betrachter ein.
fn start_geocoder(state: &Rc<AppState>) {
    let db_path = state.config.borrow().paths.db_path.clone();
    let (geocoder, mut answers) = geocode::spawn(db_path);
    *state.geocoder.borrow_mut() = Some(geocoder);

    let state = state.clone();
    glib::spawn_future_local(async move {
        while let Some(resolved) = answers.recv().await {
            let viewer = state.viewer.borrow().clone();
            if let Some(viewer) = viewer {
                viewer.deliver_place(&resolved.id, resolved.place.as_deref());
            }
            let pending = state.map_title.borrow().clone();
            if let (Some((wanted, title)), Some(place)) = (pending, resolved.place.as_deref())
                && wanted == resolved.id
            {
                title.set_title(place);
            }
        }
    });
}

/// Zeigt die Fotos einer Stelle der Karte im Raster.
///
/// Der Titel ist der Ortsname, falls der schon im Katalog liegt. Sonst wird
/// er für genau diese eine Stelle nachgeschlagen — nie für alle Gruppen der
/// Karte: Nominatim erlaubt eine Anfrage pro Sekunde, und jede verrät einen
/// Ort, an dem jemand war.
fn show_place(
    state: &Rc<AppState>,
    title: &adw::WindowTitle,
    grid: &gtk::GridView,
    pick: map_view::Pick,
) {
    let count = pick.ids.len() as u64;
    let (anchor, lat, lon) = pick.anchor;
    state.content.set(Content::Grid);
    *state.subset.borrow_mut() = Some(Subset::Place);
    state.set_scope(Scope::Photos(pick.ids));
    match state.known_place(lat, lon) {
        Some(place) => {
            state.map_title.borrow_mut().take();
            title.set_title(&place);
        }
        None => {
            title.set_title(&format!(
                "{} {}",
                thousands(count),
                if count == 1 { "Photo" } else { "Photos" }
            ));
            // Eigene ID, damit der Betrachter die Antwort nicht für sein
            // Foto hält.
            let request = format!("map:{anchor}");
            *state.map_title.borrow_mut() = Some((request.clone(), title.clone()));
            state.request_place(&request, lat, lon);
        }
    }
    state.refresh_ui();
    grid.grab_focus();
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
    TOASTS.with_borrow_mut(|slot| *slot = Some(toasts.clone()));

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
                DownloadMessage::Deleted { id } => {
                    tracing::info!(%id, "in iCloud geloescht");
                    state.sync_model();
                    update_status(&state, &status, &stack);
                    let (pending, done) = DELETES.get();
                    let (pending, done) = (pending.saturating_sub(1), done + 1);
                    DELETES.set((pending, done));
                    if pending == 0 {
                        DELETES.set((0, 0));
                        toast(&format!("Deleted {}", photos_word(done)));
                    }
                }
                DownloadMessage::DeleteFailed { id, message } => {
                    tracing::warn!(%id, %message, "Loeschen gescheitert");
                    let (pending, done) = DELETES.get();
                    DELETES.set((pending.saturating_sub(1), done));
                    toast(&format!("A photo couldn’t be deleted: {message}"));
                }
                DownloadMessage::Favorited { id, value } => {
                    tracing::info!(%id, value, "Favorit in iCloud gesetzt");
                }
                DownloadMessage::FavoriteFailed { id, value, message } => {
                    state.favorite_failed(&id, value);
                    toast(&format!(
                        "Couldn’t {} Favorites: {message}",
                        if value {
                            "add the photo to"
                        } else {
                            "remove the photo from"
                        }
                    ));
                }
                DownloadMessage::Uploaded { path } => {
                    tracing::info!(path = %path.display(), "hochgeladen");
                    let (pending, done) = UPLOADS.get();
                    let (pending, done) = (pending.saturating_sub(1), done + 1);
                    UPLOADS.set((pending, done));
                    if pending == 0 {
                        UPLOADS.set((0, 0));
                        toast(&format!("Added {}", photos_word(done)));
                        // Der Sync bringt die neuen Fotos samt Vorschau.
                        let _ = WidgetExt::activate_action(&window, "photos.sync", None);
                    }
                }
                DownloadMessage::UploadFailed { path, message } => {
                    tracing::warn!(path = %path.display(), %message, "Upload gescheitert");
                    let (pending, done) = UPLOADS.get();
                    let pending = pending.saturating_sub(1);
                    UPLOADS.set((pending, done));
                    let name = path
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    toast(&format!("“{name}” couldn’t be added: {message}"));
                    if pending == 0 && done > 0 {
                        UPLOADS.set((0, 0));
                        let _ = WidgetExt::activate_action(&window, "photos.sync", None);
                    }
                }
                DownloadMessage::CacheChanged => {
                    // Die Belegung in der Kopfzeile stimmt nach einer Eviction
                    // nicht mehr.
                    update_status(&state, &status, &stack);
                }
                DownloadMessage::PasswordRequired { responder } => {
                    match ask_secret(
                        &window,
                        "iCloud Password",
                        "There is no password for this Apple Account in the keyring.",
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
                DownloadMessage::TermsRequired(request) => {
                    let accepted = ask_terms(&window, &request.url).await;
                    let _ = request.responder.send(accepted);
                }
                DownloadMessage::TwoFactorRequired { responder } => {
                    match ask_secret(
                        &window,
                        "Two-Factor Authentication",
                        "Apple sent a verification code to your devices.",
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
    // Mehrfachauswahl: Strg- und Umschalt-Klick gehören zu einer Bibliothek,
    // aus der man etwas herausholen will.
    let selection = gtk::MultiSelection::new(Some(state.model.clone()));
    // Wo die letzte Auswahl im Auswahlmodus lag — Anker für Umschalt-Klick.
    let anchor: Rc<Cell<Option<u32>>> = Rc::default();

    factory.connect_setup({
        let state = state.clone();
        let selection = selection.clone();
        move |_, item| {
            let item = item.downcast_ref::<gtk::ListItem>().unwrap();

            let picture = gtk::Picture::builder()
                .content_fit(gtk::ContentFit::Cover)
                .build();
            picture.add_css_class("card");
            picture.add_css_class("photo-tile");

            // Das Video-Kennzeichen liegt ueber der Kachel, nicht daneben: sonst
            // verschoebe es das Raster, sobald ein Video darin vorkommt.
            let badge = crate::icons::icon("play", crate::icons::Weight::Fill);
            badge.set_halign(gtk::Align::End);
            badge.set_valign(gtk::Align::End);
            badge.add_css_class("photo-badge");
            badge.add_css_class("photo-mark");
            badge.set_visible(false);
            badge.set_can_target(false);
            // Das Herz der Favoriten unten links, wie im Lab.
            let heart = crate::icons::icon("heart", crate::icons::Weight::Fill);
            heart.set_halign(gtk::Align::Start);
            heart.set_valign(gtk::Align::End);
            heart.add_css_class("photo-heart");
            heart.set_visible(false);
            heart.set_can_target(false);

            // Der Ring der Auswahl oben rechts — nur im Auswahlmodus zu sehen
            // (das Stylesheet blendet ihn über `gridview.selecting` ein).
            let check = crate::icons::icon("check", crate::icons::Weight::Bold);
            check.set_halign(gtk::Align::End);
            check.set_valign(gtk::Align::Start);
            check.add_css_class("photo-check");
            check.set_can_target(false);

            // Die Kachel ist nur das Bild. Ein Dateiname darunter sagt über ein
            // Foto nichts, was man nicht sieht, kostet aber in jeder Reihe eine
            // Zeile Höhe — wer ihn braucht, findet ihn im Tooltip und im
            // Betrachter.
            let overlay = gtk::Overlay::new();
            overlay.set_child(Some(&picture));
            overlay.add_overlay(&badge);
            overlay.add_overlay(&heart);
            overlay.add_overlay(&check);
            item.set_child(Some(&overlay));

            // Im Auswahlmodus nimmt ein Klick das Foto dazu oder heraus
            // (Umschalt: alles bis zum letzten); rechts öffnet das Menü. In
            // der Erfassungsphase, vor der eigenen Klickbehandlung des Rasters.
            let click = gtk::GestureClick::builder()
                .button(0)
                .propagation_phase(gtk::PropagationPhase::Capture)
                .build();
            let weak_item = item.downgrade();
            click.connect_pressed({
                let state = state.clone();
                let selection = selection.clone();
                let anchor = anchor.clone();
                let overlay = overlay.downgrade();
                move |gesture, _, x, y| {
                    let (Some(item), Some(overlay)) = (weak_item.upgrade(), overlay.upgrade())
                    else {
                        return;
                    };
                    let position = item.position();
                    if position == gtk::INVALID_LIST_POSITION {
                        return;
                    }
                    let is_placeholder = item
                        .item()
                        .and_downcast::<PhotoObject>()
                        .is_none_or(|photo| photo.is_placeholder());
                    if is_placeholder {
                        return;
                    }
                    match gesture.current_button() {
                        gdk::BUTTON_SECONDARY => {
                            gesture.set_state(gtk::EventSequenceState::Claimed);
                            if !selection.is_selected(position) {
                                selection.select_item(position, true);
                            }
                            show_context_menu(&state, &overlay, x, y);
                        }
                        gdk::BUTTON_PRIMARY if state.selecting.get() => {
                            gesture.set_state(gtk::EventSequenceState::Claimed);
                            let shift = gesture
                                .current_event_state()
                                .contains(gdk::ModifierType::SHIFT_MASK);
                            match (shift, anchor.get()) {
                                (true, Some(from)) => {
                                    let (start, end) = (from.min(position), from.max(position));
                                    selection.select_range(start, end - start + 1, false);
                                }
                                _ if selection.is_selected(position) => {
                                    selection.unselect_item(position);
                                }
                                _ => {
                                    selection.select_item(position, false);
                                }
                            }
                            anchor.set(Some(position));
                        }
                        _ => {}
                    }
                }
            });
            overlay.add_controller(click);
        }
    });

    // Eine eintreffende Vorschau meldet sich am Objekt; die Kachel, die es
    // gerade zeigt, laedt dann nur sich selbst neu. Die Anmeldung haengt an
    // der Kachel und muss beim Loesen wieder ab, sonst zeichnete eine
    // recycelte Kachel das Bild eines fremden Fotos.
    let watchers: Rc<RefCell<HashMap<gtk::ListItem, (PhotoObject, glib::SignalHandlerId)>>> =
        Rc::default();

    factory.connect_bind({
        let watchers = watchers.clone();
        move |_, item| {
            let item = item.downcast_ref::<gtk::ListItem>().unwrap();
            let Some(photo) = item.item().and_downcast::<PhotoObject>() else {
                return;
            };
            let Some(overlay) = item.child().and_downcast::<gtk::Overlay>() else {
                return;
            };
            show_tile(&overlay, &photo);

            let handler = photo.connect_thumb_path_notify({
                let overlay = overlay.downgrade();
                move |photo| {
                    if let Some(overlay) = overlay.upgrade() {
                        show_tile(&overlay, photo);
                    }
                }
            });
            if let Some((old, id)) = watchers.borrow_mut().insert(item.clone(), (photo, handler)) {
                old.disconnect(id);
            }
        }
    });

    // Freigeben, sonst hält jede recycelte Zelle ihre Textur fest.
    factory.connect_unbind(move |_, item| {
        let item = item.downcast_ref::<gtk::ListItem>().unwrap();
        if let Some((photo, id)) = watchers.borrow_mut().remove(item) {
            photo.disconnect(id);
        }
        if let Some(overlay) = item.child().and_downcast::<gtk::Overlay>()
            && let Some((picture, badge)) = tile_parts(&overlay)
        {
            picture.set_paintable(gdk::Paintable::NONE);
            // Ein noch laufendes Dekodieren darf hier nichts mehr setzen.
            picture.set_widget_name("");
            picture.remove_css_class("photo-skeleton");
            badge.set_visible(false);
            overlay.set_tooltip_text(None);
        }
    });

    let grid = gtk::GridView::builder()
        .model(&selection)
        .factory(&factory)
        .max_columns(12)
        .min_columns(2)
        .css_classes(["photo-grid"])
        .build();
    build_context_menu(state, &grid);

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
            if photo.is_placeholder() || state.selecting.get() {
                return;
            }
            open_viewer(&state, &window, position);
        }
    });

    grid
}

/// Zwei Finger ändern die Kachelgröße (Touchscreen und Touchpad). Sie
/// springt zwischen festen Stufen, schon während der Geste — ein Raster,
/// das jeden Zwischenwert neu umbricht, flackert nur.
fn add_pinch(widget: &impl IsA<gtk::Widget>) {
    let pinch = gtk::GestureZoom::new();
    let pinch_start = Rc::new(Cell::new(THUMB_SIZE));
    pinch.connect_begin({
        let pinch_start = pinch_start.clone();
        move |_, _| pinch_start.set(TILE_SIZE.get())
    });
    pinch.connect_scale_changed({
        let pinch_start = pinch_start.clone();
        move |_, scale| {
            let step = tile_step_for(pinch_start.get(), scale);
            if step != TILE_SIZE.get() {
                set_tile_size(step);
            }
        }
    });
    widget.add_controller(pinch);
}

/// Das Kontextmenü der Kacheln wie im Finder (Lab, Vorschlag C, für alle
/// übernommen): oben Export · Copy · Favorite · Delete als große Knöpfe,
/// darunter die Liste. Wirkt auf die Auswahl.
fn context_menu(parent: &impl IsA<gtk::Widget>) -> Rc<menu::Menu> {
    let big = [
        menu::Big {
            icon: "share",
            fallback: crate::icons::fallback("share"),
            label: "Export",
            action: "photos.save",
            danger: false,
        },
        menu::Big {
            icon: "duplicate",
            fallback: crate::icons::fallback("duplicate"),
            label: "Copy",
            action: "photos.copy",
            danger: false,
        },
        menu::Big {
            icon: "heart",
            fallback: crate::icons::fallback("heart"),
            label: "Favorite",
            action: "photos.favorite",
            danger: false,
        },
        menu::Big {
            icon: "trash",
            fallback: crate::icons::fallback("trash"),
            label: "Delete",
            action: "photos.delete",
            danger: true,
        },
    ];
    let mut entries = vec![
        menu::item(
            "fullscreen",
            "view-fullscreen-symbolic",
            "Open",
            "photos.view",
            Some("Enter"),
        ),
        menu::item(
            "open.external",
            "document-open-symbolic",
            "Open in Default App",
            "photos.open",
            Some("Ctrl+O"),
        ),
        menu::Entry::Separator,
    ];
    if crate::retouch::available() {
        entries.push(menu::item(
            "editor",
            "document-edit-symbolic",
            "Edit in Retouch",
            "photos.retouch",
            None,
        ));
    }
    entries.push(menu::item(
        "callout",
        "dialog-information-symbolic",
        "Show Info",
        "photos.info",
        None,
    ));
    entries.push(menu::Entry::Separator);
    entries.push(menu::item(
        "check",
        "object-select-symbolic",
        "Select",
        "photos.select-mode",
        None,
    ));
    entries.push(menu::item(
        "downloads",
        "document-save-as-symbolic",
        "Export As…",
        "photos.save",
        Some("Ctrl+S"),
    ));
    let built = menu::build(&big, &entries);
    built.popover.add_css_class("photos-context");
    // Wie auf dem Mac: das Menü beginnt am Zeiger, nicht mittig darunter.
    built.popover.set_halign(gtk::Align::Start);
    built.popover.set_parent(parent);
    built
}

fn build_context_menu(state: &Rc<AppState>, grid: &gtk::GridView) {
    *state.context_menu.borrow_mut() = Some(context_menu(grid));
}

/// Öffnet ein Kontextmenü an der Stelle des Klicks auf einer Kachel.
fn popup_menu(
    state: &Rc<AppState>,
    menu: &menu::Menu,
    parent: &gtk::Widget,
    tile: &gtk::Widget,
    x: f64,
    y: f64,
) {
    state.refresh_favorite_buttons();
    let point = tile
        .compute_point(parent, &gtk::graphene::Point::new(x as f32, y as f32))
        .unwrap_or_else(|| gtk::graphene::Point::new(x as f32, y as f32));
    menu.popover.set_pointing_to(Some(&gdk::Rectangle::new(
        point.x() as i32,
        point.y() as i32,
        1,
        1,
    )));
    menu.popover.popup();
}

/// Öffnet das Kontextmenü des Rasters an der Stelle des Klicks.
fn show_context_menu(state: &Rc<AppState>, tile: &gtk::Overlay, x: f64, y: f64) {
    let (Some(menu), Some(grid)) = (
        state.context_menu.borrow().clone(),
        state.grid.borrow().clone(),
    ) else {
        return;
    };
    popup_menu(state, &menu, grid.upcast_ref(), tile.upcast_ref(), x, y);
}

/// „Days" mit seinen Haken in Katalog, Auswahl und Favoriten.
fn build_days(
    state: &Rc<AppState>,
    window: &adw::ApplicationWindow,
    grid: &gtk::GridView,
) -> Rc<DaysView> {
    let selection = grid.model();
    let hooks = days::Hooks {
        click: Rc::new({
            let state = state.clone();
            let window = window.clone();
            let selection = selection.clone();
            move |click, tile| {
                let Some(model) = selection.as_ref() else {
                    return;
                };
                match click {
                    days::Click::Select {
                        position,
                        toggle,
                        range,
                    } => {
                        state.load_until(position);
                        match (range, state.days_anchor.get()) {
                            (true, Some(from)) => {
                                state.load_until(from.max(position));
                                let (start, end) = (from.min(position), from.max(position));
                                model.select_range(start, end - start + 1, !toggle);
                            }
                            _ if toggle && model.is_selected(position) => {
                                model.unselect_item(position);
                            }
                            _ => {
                                model.select_item(position, !toggle);
                            }
                        }
                        state.days_anchor.set(Some(position));
                    }
                    days::Click::Open(position) => {
                        state.load_until(position);
                        model.select_item(position, true);
                        open_viewer(&state, &window, position);
                    }
                    days::Click::Menu { position, x, y } => {
                        state.load_until(position);
                        if !model.is_selected(position) {
                            model.select_item(position, true);
                        }
                        let menu = state.days_menu.borrow().clone();
                        let days = state.days.borrow().clone();
                        if let (Some(menu), Some(days)) = (menu, days) {
                            popup_menu(&state, &menu, days.list.upcast_ref(), tile, x, y);
                        }
                    }
                    days::Click::Day { offset, count } => {
                        if count == 0 {
                            return;
                        }
                        state.load_until(offset + count - 1);
                        model.select_range(offset, count, false);
                    }
                }
            }
        }),
        load: Rc::new({
            let state = state.clone();
            move |offset, count| {
                state
                    .catalog
                    .borrow()
                    .page_in(&Scope::Library, offset, count)
                    .unwrap_or_default()
            }
        }),
        selected: Rc::new({
            let selection = selection.clone();
            move |position| {
                selection
                    .as_ref()
                    .is_some_and(|model| position < model.n_items() && model.is_selected(position))
            }
        }),
        favorite: Rc::new(is_favorite),
        selecting: Rc::new({
            let state = state.clone();
            move || state.selecting.get()
        }),
        today: Rc::new(|| chrono::Local::now().date_naive()),
    };
    let view = DaysView::new(hooks, TILE_SIZE.get());
    *state.days_menu.borrow_mut() = Some(context_menu(&view.list));
    *state.days.borrow_mut() = Some(view.clone());
    view
}

/// Bild und Video-Kennzeichen einer Kachel.
fn tile_parts(overlay: &gtk::Overlay) -> Option<(gtk::Picture, gtk::Widget)> {
    let picture = overlay.child().and_downcast::<gtk::Picture>()?;
    let badge = picture.next_sibling()?;
    Some((picture, badge))
}

/// Zeigt genau dieses Foto im Betrachter — die App-Aktion „open-photo“.
///
/// Der Betrachter blättert im Modell des Rasters. Deshalb stellt die
/// Bibliothek den Ausschnitt, und das Raster wird bis zur Position des Fotos
/// nachgeladen, wie beim Sprung zu einem Monat.
fn open_photo(state: &Rc<AppState>, window: &adw::ApplicationWindow, id: &str) {
    let position = state.catalog.borrow().position_of(id);
    let position = match position {
        Ok(Some(position)) => position,
        Ok(None) => {
            tracing::warn!(id, "Foto nicht im Katalog");
            window.present();
            return;
        }
        Err(error) => {
            tracing::error!(%error, "Position des Fotos nicht lesbar");
            window.present();
            return;
        }
    };
    if *state.scope.borrow() != Scope::Library || state.content.get() != Content::Grid {
        let sidebar = state.sidebar.borrow().clone();
        match sidebar {
            Some(sidebar) => sidebar.select(&Target::Library),
            None => state.set_scope(Scope::Library),
        }
    }
    let Some(grid) = state.grid.borrow().clone() else {
        return;
    };
    jump_to(state, &grid, position);
    let count = state.model.n_items();
    if count == 0 {
        return;
    }
    // Ein laufender Sync kann Fotos vor dieses geschoben haben, die das
    // Modell noch nicht zeigt — dann zählt, wo es im Modell wirklich steht.
    let in_model = (0..count).find(|&index| {
        state
            .model
            .item(index)
            .and_downcast::<PhotoObject>()
            .is_some_and(|photo| photo.id() == id)
    });
    let position =
        in_model.unwrap_or_else(|| u32::try_from(position).unwrap_or(count - 1).min(count - 1));
    tracing::info!(id, position, "Foto per Aktion geöffnet");
    open_viewer(state, window, position);
    window.present();
}

/// Öffnet den Betrachter als Seite desselben Fensters.
fn open_viewer(state: &Rc<AppState>, parent: &adw::ApplicationWindow, position: u32) {
    // Der Zurück-Knopf nennt die Seite, von der man kam.
    if state.viewer_back.borrow().is_empty() {
        let title = state
            .head
            .borrow()
            .as_ref()
            .map(|head| head.title())
            .filter(|title| !title.is_empty() && !state.selecting.get())
            .unwrap_or_else(|| "Library".to_string());
        *state.viewer_back.borrow_mut() = title;
    }
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
/// Zeichnet eine Kachel fuer `photo` — Skelett oder Vorschau.
fn show_tile(overlay: &gtk::Overlay, photo: &PhotoObject) {
    let Some((picture, badge)) = tile_parts(overlay) else {
        return;
    };

    // Die Datumszeile ueber dem Raster liest den Monat der obersten
    // sichtbaren Kachel hier ab.
    overlay.set_widget_name(&photo.month_label());

    let Some(path) = photo.thumb_path().filter(|_| !photo.is_placeholder()) else {
        // Noch kein Bild da: Platzhalter statt leerer Flaeche, damit die
        // Ansicht von Anfang an ihre spaetere Form hat.
        picture.add_css_class("photo-skeleton");
        picture.set_paintable(gdk::Paintable::NONE);
        picture.set_widget_name("");
        badge.set_visible(false);
        if let Some(heart) = badge.next_sibling() {
            heart.set_visible(false);
        }
        overlay.set_tooltip_text(None);
        return;
    };

    badge.set_visible(photo.is_video());
    if let Some(heart) = badge.next_sibling() {
        heart.set_visible(is_favorite(&photo.id()));
    }
    overlay.set_tooltip_text(Some(&format!(
        "{}\n{}",
        photo.filename(),
        photo.captured_label()
    )));
    set_image(&picture, &path);
}

/// So viele dekodierte Vorschauen haelt die App im Speicher — zwei, drei
/// Bildschirme. Eine Vorschau hat 360×480 Pixel, dekodiert also rund 0,7 MB;
/// mehr als ~140 MB soll das nicht werden.
const TEXTURE_CACHE: usize = 200;

thread_local! {
    /// Zuletzt benutzte Vorschauen. Wer zurueckscrollt, sieht sie sofort,
    /// statt jede noch einmal von der Platte zu dekodieren.
    static TEXTURES: RefCell<(HashMap<String, gdk::Texture>, VecDeque<String>)> =
        RefCell::default();
}

fn cached_texture(path: &str) -> Option<gdk::Texture> {
    TEXTURES.with_borrow(|(map, _)| map.get(path).cloned())
}

fn remember_texture(path: &str, texture: &gdk::Texture) {
    TEXTURES.with_borrow_mut(|(map, order)| {
        if map.insert(path.to_string(), texture.clone()).is_none() {
            order.push_back(path.to_string());
        }
        while order.len() > TEXTURE_CACHE {
            if let Some(oldest) = order.pop_front() {
                map.remove(&oldest);
            }
        }
    });
}

/// Setzt die Vorschau. Liegt sie nicht schon dekodiert vor, geschieht das
/// Dekodieren in einem Hilfsthread — im UI-Thread liess es jedes Scrollen
/// ruckeln, und waehrend eines Syncs erst recht.
pub(crate) fn set_image(picture: &gtk::Picture, path: &str) {
    if let Some(texture) = cached_texture(path) {
        picture.remove_css_class("photo-skeleton");
        picture.set_paintable(Some(&texture));
        picture.set_widget_name(path);
        return;
    }

    // Die Kachel merkt sich, welches Bild sie erwartet. Ist sie bis zum Ende
    // des Dekodierens an ein anderes Foto gewandert, verfaellt das Ergebnis.
    // Bis dahin bleibt stehen, was die Kachel zeigt — kein Aufblitzen.
    if picture.widget_name() != path {
        picture.set_paintable(gdk::Paintable::NONE);
        picture.add_css_class("photo-skeleton");
    }
    picture.set_widget_name(path);

    let picture = picture.downgrade();
    let path = path.to_string();
    glib::spawn_future_local(async move {
        let source = path.clone();
        let decoded = gio::spawn_blocking(move || gdk::Texture::from_filename(&source)).await;
        let texture = match decoded {
            Ok(Ok(texture)) => texture,
            Ok(Err(error)) => {
                tracing::debug!(%path, %error, "Vorschau nicht ladbar");
                return;
            }
            Err(_) => return,
        };
        remember_texture(&path, &texture);
        let Some(picture) = picture.upgrade() else {
            return;
        };
        if picture.widget_name() == path {
            picture.remove_css_class("photo-skeleton");
            picture.set_paintable(Some(&texture));
        }
    });
}

fn chrono_now() -> chrono::DateTime<chrono::Utc> {
    chrono::Utc::now()
}

fn update_status(state: &Rc<AppState>, _status: &gtk::Label, stack: &gtk::Stack) {
    update_page(state, stack);
}

/// Welche Seite steht, und alles, was an ihr hängt: Kopf, Zahl neben
/// „Library", Übersichten, Sync-Stand, Aktionsleiste, Sync-Manager.
fn update_page(state: &Rc<AppState>, stack: &gtk::Stack) {
    let total = state.catalog.borrow().count().unwrap_or(0);
    let scope = state.scope.borrow().clone();
    let shown = if scope == Scope::Library {
        total
    } else {
        state.catalog.borrow().count_in(&scope).unwrap_or(0)
    };

    // Bis zur abgeschlossenen Anmeldung bleibt die Maske stehen. Ein leeres
    // Album ist leer, nicht "noch nicht synchronisiert".
    let page = match state.current_page(total) {
        "grid" if shown == 0 && !state.syncing.get() => "empty",
        page => page,
    };
    if stack.child_by_name(page).is_some() {
        stack.set_visible_child_name(page);
    }

    if let Some(sidebar) = state.sidebar.borrow().as_ref() {
        sidebar.set_library_count(total);
    }
    state.refresh_overview();
    state.refresh_days();
    state.refresh_head();
    state.refresh_sync_status();
    state.refresh_action_bar();

    if let Some(manager) = state.sync_manager.borrow().as_ref() {
        let now = chrono_now();
        let sync_state = state.catalog.borrow().sync_state().ok();
        let cached = state.catalog.borrow().cached_bytes().unwrap_or(0);
        let limit = state.config.borrow().cache.limit_bytes();
        manager.show_overview(&Overview {
            photos: total,
            apple_id: state.config.borrow().icloud.username.clone(),
            last_sync: last_sync(state).map(|then| since(then, now)),
            last_full_pass: sync_state
                .and_then(|s| s.last_baseline_at)
                .map(|then| since(then, now)),
            cache: format!("{} of {}", human_bytes(cached), human_bytes(limit)),
            thumbnails: {
                let (bytes, count) = state.catalog.borrow().thumbnail_usage().unwrap_or((0, 0));
                format!("{} · {} thumbnails", human_bytes(bytes), thousands(count))
            },
            thumbnail_dir: state.config.borrow().paths.thumbnail_dir.clone(),
            full_res_dir: state.config.borrow().paths.full_res_dir.clone(),
        });
    }
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
        i64::MIN..=0 => "just now".to_string(),
        1 => "1 minute ago".to_string(),
        2..=59 => format!("{minutes} minutes ago"),
        60..=119 => "1 hour ago".to_string(),
        120..=1439 => format!("{} hours ago", minutes / 60),
        1440..=2879 => "1 day ago".to_string(),
        _ => format!("{} days ago", minutes / 1440),
    }
}

/// Was bei einem Lauf herauskam, in einem Satz.
fn outcome(summary: &SyncSummary) -> String {
    if summary.unchanged {
        // Unveraendert heisst nur: keine neuen Fotos. Der Nachlauf fuer
        // fehlende Vorschauen kann trotzdem gearbeitet haben, und das war bei
        // einer grossen Bibliothek die halbe Minute davor.
        return match summary.thumbnails {
            0 => "Library unchanged".to_string(),
            count => format!(
                "Library unchanged · {} thumbnails fetched",
                thousands(count)
            ),
        };
    }

    let mut parts = Vec::new();
    if summary.stored > 0 {
        parts.push(format!("{} photos", thousands(summary.stored)));
    }
    if summary.thumbnails > 0 {
        parts.push(format!("{} thumbnails", thousands(summary.thumbnails)));
    }
    if summary.deleted > 0 {
        parts.push(format!("{} removed", thousands(summary.deleted)));
    }
    if parts.is_empty() {
        return "Sync complete · nothing new".to_string();
    }
    format!("Sync complete · {}", parts.join(", "))
}

/// „62 photos · 4 videos · October 10 – 12, 2026"
fn summary_line(summary: &icloud_photos_cache::Summary) -> String {
    let photos = summary.items.saturating_sub(summary.videos);
    let mut line = crate::home::photos_word(photos);
    if summary.videos > 0 {
        line.push_str(" · ");
        line.push_str(&crate::home::videos_word(summary.videos));
    }
    if let (Some(first), Some(last)) = (summary.first, summary.last) {
        line.push_str(" · ");
        line.push_str(&date_span(first, last));
    }
    line
}

/// „October 10 – 12, 2026", „September 21 – October 12, 2026",
/// „December 24, 2025 – October 12, 2026".
fn date_span(first: chrono::NaiveDate, last: chrono::NaiveDate) -> String {
    use chrono::Datelike;
    let month = |day: chrono::NaiveDate| crate::photo_object::month_name(day.month());
    if first == last {
        crate::home::long_date(first)
    } else if first.year() != last.year() {
        format!(
            "{} – {}",
            crate::home::long_date(first),
            crate::home::long_date(last)
        )
    } else if first.month() != last.month() {
        format!(
            "{} {} – {} {}, {}",
            month(first),
            first.day(),
            month(last),
            last.day(),
            last.year()
        )
    } else {
        format!(
            "{} {} – {}, {}",
            month(first),
            first.day(),
            last.day(),
            last.year()
        )
    }
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
    manager: Rc<SyncManager>,
    toasts: adw::ToastOverlay,
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
                    manager.start(resuming, full_pass);
                }
                SyncMessage::Progress { stored } => {
                    state.model_dirty.set(true);
                    manager.progress(stored);
                }
                SyncMessage::Thumbnails { stored } => {
                    state.model_dirty.set(true);
                    if backfilling {
                        manager.backfilled(stored);
                    } else {
                        // Waehrend des Abgleichs zaehlen Vorschauen dieselben
                        // Assets wie die Metadaten, nur feiner.
                        manager.progress(stored);
                    }
                }
                SyncMessage::AlbumsStarted => {
                    manager.albums();
                }
                SyncMessage::AlbumsProgress { done, total } => {
                    manager.albums_loaded(done, total);
                }
                SyncMessage::AlbumsChanged => {
                    state.refresh_albums();
                }
                SyncMessage::BackfillStarted { missing } => {
                    backfilling = true;
                    manager.backfill(missing);
                }
                SyncMessage::Unchanged => {
                    tracing::debug!("Bibliothek unveraendert");
                }
                SyncMessage::LibraryCount { total } => {
                    // Ab jetzt kennt die Ansicht ihre spaetere Groesse — das
                    // Raster kann die Luecke mit Skeletten fuellen, der Balken
                    // einen Stand statt nur Bewegung zeigen.
                    state.expected_total.set(total);
                    manager.set_expected(total);
                    state.sync_model();
                    update_status(&state, &status, &stack);
                }
                SyncMessage::Deleted { removed } => {
                    state.model_dirty.set(true);
                    manager.deleting(removed);
                }
                SyncMessage::Authenticated { trusted } => {
                    // Erst hier ist die Anmeldung wirklich durch — Passwort und
                    // 2FA sind bestaetigt. Jetzt darf die Maske weichen.
                    tracing::info!(trusted, "Anmeldung abgeschlossen");
                    state.authenticated.set(true);
                    state.authenticating.set(false);
                    state.login_busy(false);
                    manager.authenticated();
                    update_status(&state, &status, &stack);
                }
                SyncMessage::PasswordRequired { responder } => {
                    match ask_secret(
                        &window,
                        "iCloud Password",
                        "There is no password for this Apple Account in the keyring.",
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
                    state.login_error("Apple rejected these credentials.");
                    manager.failed("Sign-in failed", &message);
                    update_status(&state, &status, &stack);
                    show_error(
                        &window,
                        "Sign-In Failed",
                        &format!("Apple rejected the sign-in: {message}"),
                    );
                }
                SyncMessage::TermsRequired(request) => {
                    let accepted = ask_terms(&window, &request.url).await;
                    let _ = request.responder.send(accepted);
                }
                SyncMessage::TwoFactorRequired { responder } => {
                    let code = ask_secret(
                        &window,
                        "Two-Factor Authentication",
                        "Apple sent a verification code to your devices.",
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
                    manager.finished(&outcome(&summary));
                    // Vor dem Nachziehen: sonst blieben die Skelette stehen.
                    state.syncing.set(false);
                    state.sync_model();
                    // Neue oder geloeschte Fotos aendern auch die Zahlen in den
                    // Alben — auch wenn die Alben selbst gleich blieben.
                    if summary.stored > 0 || summary.deleted > 0 {
                        state.refresh_albums();
                        state.refresh_map();
                    }
                    update_status(&state, &status, &stack);
                }
                SyncMessage::Finished(Err(error)) => {
                    tracing::error!(%error, "Sync fehlgeschlagen");
                    // Scheitert es vor `auth_ok` — abgebrochene 2FA, Netzfehler —
                    // muss die Maske wieder bedienbar sein, sonst haengt der
                    // Nutzer in einem gesperrten Formular fest.
                    if !state.authenticated.get() {
                        state.login_error("Sign-in did not complete. Please try again.");
                    }
                    manager.failed("Sync failed", &error);
                    // Was bis zum Abbruch ankam, soll stehen bleiben — nur die
                    // Skelette fuer den Rest nicht.
                    state.syncing.set(false);
                    state.sync_model();
                    // Kein Dialog mitten ins Betrachten: ein Hinweis, der
                    // von selbst verschwindet, und die Einzelheiten im Manager.
                    let toast = adw::Toast::builder()
                        .title("Sync failed")
                        .button_label("Details")
                        .action_name("photos.sync-manager")
                        .build();
                    toasts.add_toast(toast);
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
            .placeholder_text("6-digit code")
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
    dialog.add_response("cancel", "Cancel");
    dialog.add_response("ok", "Verify");
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

/// Fragt, ob Apples geaenderte Nutzungsbedingungen akzeptiert werden.
///
/// Die Zustimmung gilt fuer das ganze Apple-Konto, nicht nur fuer diese App.
/// Deshalb kein Haken im Hintergrund, sondern eine Entscheidung mit Verweis
/// auf den Text — und Ablehnen als Vorgabe beim Schliessen.
async fn ask_terms(parent: &adw::ApplicationWindow, url: &str) -> bool {
    let link = gtk::LinkButton::with_label(url, "Read the Terms and Conditions");
    let dialog = adw::AlertDialog::builder()
        .heading("New iCloud Terms and Conditions")
        .body(
            "Apple has changed the iCloud Terms and Conditions. iCloud won't let \
             you sign in until you agree.\n\nAgreeing applies to your whole \
             Apple Account, just as if you had agreed on icloud.com.",
        )
        .extra_child(&link)
        .close_response("decline")
        .build();
    dialog.add_response("decline", "Decline");
    dialog.add_response("accept", "Agree");
    dialog.set_response_appearance("accept", adw::ResponseAppearance::Suggested);
    dialog.choose_future(Some(parent)).await == "accept"
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
    let button = crate::icons::menu_button(
        "calendar",
        "x-office-calendar-symbolic",
        "Jump to a Month (Ctrl+J)",
    );
    button.set_popover(Some(&popover));

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
            let scope = state.scope.borrow().clone();
            let groups = match state.catalog.borrow().month_groups_in(&scope) {
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
// Löschen und Hinzufügen
// ---------------------------------------------------------------------------

thread_local! {
    /// Wohin Meldungen über Löschen und Hochladen gehen.
    static TOASTS: RefCell<Option<adw::ToastOverlay>> = const { RefCell::new(None) };
    /// Laufende Aufträge und was davon geklappt hat — für eine Meldung pro
    /// Stapel statt einer pro Foto.
    static DELETES: Cell<(u32, u32)> = const { Cell::new((0, 0)) };
    static UPLOADS: Cell<(u32, u32)> = const { Cell::new((0, 0)) };
}

fn toast(text: &str) {
    TOASTS.with_borrow(|toasts| {
        if let Some(toasts) = toasts {
            toasts.add_toast(adw::Toast::new(text));
        }
    });
}

fn photos_word(n: u32) -> String {
    if n == 1 {
        "1 photo".into()
    } else {
        format!("{} photos", thousands(n as u64))
    }
}

/// Fragt nach und löscht dann in iCloud. Wie in Photos: das Foto verschwindet
/// auf allen Geräten und liegt 30 Tage unter „Zuletzt gelöscht". `confirmed`
/// läuft nach dem Ja, bevor die Aufträge rausgehen (der Betrachter schließt).
pub fn delete_photos(
    state: &Rc<AppState>,
    parent: &impl IsA<gtk::Widget>,
    ids: Vec<String>,
    confirmed: impl FnOnce() + 'static,
) {
    if ids.is_empty() {
        return;
    }
    let n = ids.len() as u32;
    let (heading, body) = if n == 1 {
        (
            "Delete Photo?".to_string(),
            "This photo will be deleted from iCloud Photos on all your devices. \
             It stays in Recently Deleted for 30 days."
                .to_string(),
        )
    } else {
        (
            format!("Delete {}?", photos_word(n).replace("photos", "Photos")),
            "These photos will be deleted from iCloud Photos on all your devices. \
             They stay in Recently Deleted for 30 days."
                .to_string(),
        )
    };
    let dialog = adw::AlertDialog::new(Some(&heading), Some(&body));
    dialog.add_response("cancel", "Cancel");
    dialog.add_response("delete", "Delete");
    dialog.set_response_appearance("delete", adw::ResponseAppearance::Destructive);
    dialog.set_default_response(Some("cancel"));
    dialog.set_close_response("cancel");
    let state = state.clone();
    let parent = parent.as_ref().clone();
    glib::spawn_future_local(async move {
        if dialog.choose_future(Some(&parent)).await != "delete" {
            return;
        }
        confirmed();
        let Some(downloader) = state.downloader.borrow().as_ref().map(|d| d.clone_handle()) else {
            return;
        };
        let (pending, done) = DELETES.get();
        DELETES.set((pending + n, done));
        for id in &ids {
            downloader.delete(id);
        }
    });
}

/// Lädt Dateien in die Mediathek; danach holt ein Sync sie samt Vorschau.
pub fn upload_files(state: &Rc<AppState>, files: Vec<std::path::PathBuf>) {
    let files: Vec<_> = files.into_iter().filter(|f| f.is_file()).collect();
    if files.is_empty() {
        return;
    }
    let Some(downloader) = state.downloader.borrow().as_ref().map(|d| d.clone_handle()) else {
        return;
    };
    let n = files.len() as u32;
    let (pending, done) = UPLOADS.get();
    UPLOADS.set((pending + n, done));
    toast(&format!("Adding {}…", photos_word(n)));
    for file in &files {
        downloader.upload(file);
    }
}

/// Dateiauswahl für Fotos und Videos.
fn choose_uploads(state: &Rc<AppState>, window: &adw::ApplicationWindow) {
    let filter = gtk::FileFilter::new();
    filter.set_name(Some("Photos and Videos"));
    filter.add_mime_type("image/*");
    filter.add_mime_type("video/*");
    let filters = gio::ListStore::new::<gtk::FileFilter>();
    filters.append(&filter);
    let dialog = gtk::FileDialog::builder()
        .title("Add to iCloud Photos")
        .accept_label("Add")
        .filters(&filters)
        .default_filter(&filter)
        .build();
    let state = state.clone();
    dialog.open_multiple(Some(window), gio::Cancellable::NONE, move |result| {
        let Ok(list) = result else {
            return;
        };
        let files = (0..list.n_items())
            .filter_map(|i| list.item(i).and_downcast::<gio::File>())
            .filter_map(|file| file.path())
            .collect();
        upload_files(&state, files);
    });
}

// ---------------------------------------------------------------------------
// Auswahl: sichern, kopieren, öffnen
// ---------------------------------------------------------------------------

/// Hängt die Aktionen für die Auswahl in die Kopfzeile und macht sie auch für
/// den Betrachter erreichbar.
fn build_selection_actions(
    state: &Rc<AppState>,
    window: &adw::ApplicationWindow,
    grid: &gtk::GridView,
    status: &gtk::Label,
    toasts: &adw::ToastOverlay,
) -> gio::SimpleActionGroup {
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

    // Sichern, Kopieren, Öffnen und Retouch als Fensteraktionen — für die
    // Aktionsleiste, das Kontextmenü und die gewohnten Tastenkürzel.
    let group = gio::SimpleActionGroup::new();
    let mut entries = Vec::new();
    for (name, action, accelerator) in [
        ("save", UiAction::Save, Some("<Control>s")),
        ("copy", UiAction::Clipboard, Some("<Control>c")),
        ("open", UiAction::Open, Some("<Control>o")),
        ("retouch", UiAction::Retouch, None),
    ] {
        let entry = gio::SimpleAction::new(name, None);
        entry.set_enabled(false);
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
        if let (Some(app), Some(accelerator)) = (window.application(), accelerator) {
            app.set_accels_for_action(&format!("photos.{name}"), &[accelerator]);
        }
        entries.push((name, entry));
    }

    // Öffnen im Betrachter und „Show Info" — das erste gewählte Foto.
    for (name, info) in [("view", false), ("info", true)] {
        let entry = gio::SimpleAction::new(name, None);
        entry.connect_activate({
            let state = state.clone();
            let grid = grid.clone();
            let window = window.clone();
            move |_, _| {
                let Some(model) = grid.model() else { return };
                let selection = model.selection();
                if selection.is_empty() {
                    return;
                }
                if info {
                    crate::viewer::set_info_open(true);
                }
                open_viewer(&state, &window, selection.minimum());
            }
        });
        group.add_action(&entry);
    }

    // Favorit an/aus für die Auswahl (Kontextmenü, Aktionsleiste).
    let favorite = gio::SimpleAction::new("favorite", None);
    favorite.set_enabled(false);
    favorite.connect_activate({
        let state = state.clone();
        let grid = grid.clone();
        move |_, _| {
            let ids = selected_items(&grid)
                .into_iter()
                .map(|item| item.id)
                .collect();
            state.toggle_favorite(ids);
        }
    });
    group.add_action(&favorite);

    // Auswahlmodus: „Select" im Kopf schaltet ein, „Done" oder Esc aus.
    let select_mode = gio::SimpleAction::new("select-mode", None);
    select_mode.connect_activate({
        let state = state.clone();
        move |_, _| {
            if !matches!(state.content.get(), Content::Grid | Content::Days) {
                state.zoom.set(Zoom::Days);
                state.content.set(Content::Days);
                state.set_scope(Scope::Library);
                state.refresh_ui();
            }
            state.set_selecting(!state.selecting.get());
        }
    });
    group.add_action(&select_mode);
    let escape = gtk::EventControllerKey::new();
    escape.connect_key_pressed({
        let state = state.clone();
        move |_, key, _, _| {
            let viewing = state.viewer.borrow().is_some();
            if key == gdk::Key::Escape && state.selecting.get() && !viewing {
                state.set_selecting(false);
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
        }
    });
    window.add_controller(escape);

    // Löschen (Entf) mit Rückfrage, Hinzufügen (Strg+Umschalt+I) über die
    // Dateiauswahl — wie „Import" in Photos.
    let delete_action = gio::SimpleAction::new("delete", None);
    delete_action.set_enabled(false);
    delete_action.connect_activate({
        let state = state.clone();
        let grid = grid.clone();
        let window = window.clone();
        move |_, _| {
            let ids = selected_items(&grid)
                .into_iter()
                .map(|item| item.id)
                .collect();
            delete_photos(&state, &window, ids, || {});
        }
    });
    group.add_action(&delete_action);
    let add_action = gio::SimpleAction::new("add", None);
    add_action.connect_activate({
        let state = state.clone();
        let window = window.clone();
        move |_, _| choose_uploads(&state, &window)
    });
    group.add_action(&add_action);
    if let Some(app) = window.application() {
        app.set_accels_for_action("photos.delete", &["Delete"]);
        app.set_accels_for_action("photos.add", &["<Control><Shift>i"]);
    }

    // Dateien ins Fenster ziehen fügt sie hinzu.
    let drop = gtk::DropTarget::new(gdk::FileList::static_type(), gdk::DragAction::COPY);
    drop.connect_drop({
        let state = state.clone();
        move |_, value, _, _| {
            let Ok(list) = value.get::<gdk::FileList>() else {
                return false;
            };
            let files: Vec<_> = list.files().iter().filter_map(|file| file.path()).collect();
            let any = !files.is_empty();
            upload_files(&state, files);
            any
        }
    });
    window.add_controller(drop);

    // Ohne Auswahl gibt es nichts zu sichern.
    if let Some(model) = grid.model() {
        model.connect_selection_changed({
            let delete = delete_action.clone();
            let favorite = favorite.clone();
            let grid = grid.clone();
            let state = state.clone();
            move |_, _, _| {
                let count = selected_items(&grid).len();
                delete.set_enabled(count > 0);
                favorite.set_enabled(count > 0);
                state.refresh_favorite_buttons();
                state.refresh_days_marks();
                for (name, entry) in &entries {
                    // Mehrere Fotos gleichzeitig zu öffnen ergibt keinen Sinn.
                    entry.set_enabled(if *name == "open" {
                        count == 1
                    } else {
                        count > 0
                    });
                }
                state.refresh_action_bar();
                if state.selecting.get() {
                    state.refresh_head();
                }
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
/// Fragt, in welchem Format gesichert wird. Vorausgewaehlt ist die letzte
/// Wahl — wer einmal JPEG will, will es meist wieder.
async fn ask_format(
    parent: &adw::ApplicationWindow,
    last: export::Format,
    count: usize,
) -> Option<export::Format> {
    let list = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .css_classes(["boxed-list"])
        .build();
    let mut group: Option<gtk::CheckButton> = None;
    let mut choices = Vec::new();
    for format in export::Format::ALL {
        let check = gtk::CheckButton::builder()
            .valign(gtk::Align::Center)
            .active(format == last)
            .build();
        if let Some(first) = &group {
            check.set_group(Some(first));
        } else {
            group = Some(check.clone());
        }
        let row = adw::ActionRow::builder()
            .title(format.label())
            .subtitle(format.description())
            .activatable_widget(&check)
            .build();
        row.add_prefix(&check);
        list.append(&row);
        choices.push((format, check));
    }

    let dialog = adw::AlertDialog::builder()
        .heading(if count == 1 {
            "Export Photo".to_string()
        } else {
            format!("Export {} Photos", thousands(count as u64))
        })
        .body("Which format? Videos are always exported as originals.")
        .extra_child(&list)
        .default_response("choose")
        .close_response("cancel")
        .build();
    dialog.add_response("cancel", "Cancel");
    dialog.add_response("choose", "Choose Folder…");
    dialog.set_response_appearance("choose", adw::ResponseAppearance::Suggested);

    if dialog.choose_future(Some(parent)).await != "choose" {
        return None;
    }
    choices
        .into_iter()
        .find(|(_, check)| check.is_active())
        .map(|(format, _)| format)
}

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
        toasts.add_toast(adw::Toast::new("Another task is already running."));
        return;
    }

    match action {
        UiAction::Save => {
            // Erst Format und Ziel, dann die Downloads: wer abbricht, soll
            // nichts angestoßen haben.
            let state = state.clone();
            let window = window.clone();
            let status = status.clone();
            let toasts = toasts.clone();
            glib::spawn_future_local(async move {
                let count = items.len();
                let Some(format) = ask_format(&window, state.save_format.get(), count).await else {
                    return;
                };
                state.save_format.set(format);

                let dialog = gtk::FileDialog::builder()
                    .title("Export Photos")
                    .accept_label("Export Here")
                    .build();
                let Ok(folder) = dialog.select_folder_future(Some(&window)).await else {
                    return;
                };
                let Some(path) = folder.path() else { return };
                begin_job(
                    &state,
                    &window,
                    &status,
                    &toasts,
                    export::Action::SaveTo(path, format),
                    items,
                );
            });
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
        UiAction::Retouch => begin_job(
            state,
            window,
            status,
            toasts,
            export::Action::Retouch,
            items,
        ),
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
                status.set_label(&format!("{}… {ready} / {total}", job.action.verb()));
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

    // Sichern kann umwandeln, und das dauert: im Hilfsthread, sonst stuende
    // die App bei hundert HEICs sekundenlang.
    if let export::Action::SaveTo(dir, format) = &job.action
        && !job.ready().is_empty()
    {
        let dir = dir.clone();
        let format = *format;
        let files = job.ready().to_vec();
        let missing = job.failed();
        status.set_label(&format!("Exporting as {}…", format.label()));
        let status = status.clone();
        let toasts = toasts.clone();
        glib::spawn_future_local(async move {
            let saved = gio::spawn_blocking(move || export::save_into(&dir, &files, format))
                .await
                .unwrap_or_else(|_| export::Saved {
                    errors: vec!["Export aborted".to_string()],
                    ..Default::default()
                });
            for error in &saved.errors {
                tracing::warn!(%error, "Sichern fehlgeschlagen");
            }
            toasts.add_toast(adw::Toast::new(&saved_message(&saved, missing)));
            status.set_label("");
        });
        return;
    }

    // Retouch bekommt die IDs; der Server dort nimmt die Originale aus dem
    // Cache dieser App. Starten und Übergeben dauert — im Hilfsthread.
    if job.action == export::Action::Retouch && !job.ready().is_empty() {
        let ids: Vec<String> = job
            .ready()
            .iter()
            .map(|(item, _)| item.id.clone())
            .collect();
        let toasts = toasts.clone();
        toasts.add_toast(adw::Toast::new("Opening in Retouch…"));
        status.set_label("");
        glib::spawn_future_local(async move {
            let result = gio::spawn_blocking(move || crate::retouch::hand_off(&ids))
                .await
                .unwrap_or_else(|_| Err("Retouch couldn’t be opened.".to_string()));
            if let Err(message) = result {
                toasts.add_toast(adw::Toast::new(&message));
            }
        });
        return;
    }

    let message = finish_job(window, &job);
    toasts.add_toast(adw::Toast::new(&message));
    status.set_label("");
}

/// „3 Fotos gesichert · 1 Video im Original · 2 nicht"
fn saved_message(saved: &export::Saved, missing: usize) -> String {
    let mut message = match saved.saved {
        1 => "1 photo exported".to_string(),
        other => format!("{other} photos exported"),
    };
    match saved.kept_as_video {
        0 => {}
        1 => message.push_str(" · 1 video as original"),
        other => message.push_str(&format!(" · {other} videos as originals")),
    }
    let failed = missing + saved.errors.len();
    if failed > 0 {
        message.push_str(&format!(" · {failed} failed"));
    }
    message
}

/// Führt den fertigen Auftrag aus und beschreibt, was dabei herauskam.
fn finish_job(window: &adw::ApplicationWindow, job: &export::Job) -> String {
    let files = job.ready();
    if files.is_empty() {
        return "No originals available.".to_string();
    }
    let missing = job.failed();

    match &job.action {
        // Laeuft in `advance_job` im Hilfsthread; hier nur, falls es
        // nichts zu sichern gab.
        export::Action::SaveTo(..) => format!("Nothing exported · {missing} unavailable"),
        export::Action::Retouch => "Nothing to open in Retouch.".to_string(),
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
                1 => "1 photo copied".to_string(),
                other => format!("{other} photos copied"),
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
            "Opening…".to_string()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::DateTime;

    /// Ein Zustand mit dem Raster als Inhalt — die Seitenwahl um die
    /// Anmeldung herum gilt für jede Seite gleich.
    fn state() -> Rc<AppState> {
        let state = AppState::new(
            Config::default(),
            PathBuf::from("/nonexistent/config.toml"),
            Catalog::open_in_memory().unwrap(),
        );
        state.content.set(Content::Grid);
        state
    }

    #[test]
    fn home_is_the_start_page_unless_the_library_is_empty() {
        let state = AppState::new(
            Config::default(),
            PathBuf::from("/nonexistent/config.toml"),
            Catalog::open_in_memory().unwrap(),
        );
        state.authenticated.set(true);
        assert_eq!(state.current_page(120), "home");
        assert_eq!(state.current_page(0), "empty");
        state.syncing.set(true);
        assert_eq!(state.current_page(0), "home");
    }

    #[test]
    fn album_subtitles_name_the_span() {
        let day = |raw: &str| chrono::NaiveDate::parse_from_str(raw, "%Y-%m-%d").unwrap();
        assert_eq!(
            date_span(day("2026-10-10"), day("2026-10-12")),
            "October 10 – 12, 2026"
        );
        assert_eq!(
            date_span(day("2026-09-21"), day("2026-10-12")),
            "September 21 – October 12, 2026"
        );
        assert_eq!(
            date_span(day("2025-12-24"), day("2026-01-02")),
            "December 24, 2025 – January 2, 2026"
        );
        let summary = icloud_photos_cache::Summary {
            items: 66,
            videos: 4,
            first: Some(day("2026-10-10")),
            last: Some(day("2026-10-12")),
        };
        assert_eq!(
            summary_line(&summary),
            "62 photos · 4 videos · October 10 – 12, 2026"
        );
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

        assert_eq!(ago(0), "just now");
        assert_eq!(ago(1), "1 minute ago");
        assert_eq!(ago(3), "3 minutes ago");
        assert_eq!(ago(59), "59 minutes ago");
        assert_eq!(ago(60), "1 hour ago");
        assert_eq!(ago(150), "2 hours ago");
        assert_eq!(ago(1_440), "1 day ago");
        assert_eq!(ago(4_320), "3 days ago");

        // Eine rueckwaerts gestellte Uhr darf keine negative Angabe ergeben.
        assert_eq!(since(now + chrono::TimeDelta::minutes(5), now), "just now");
    }

    #[test]
    fn sums_up_what_a_run_did() {
        let mut summary = SyncSummary {
            unchanged: true,
            ..Default::default()
        };
        assert_eq!(outcome(&summary), "Library unchanged");

        // Nachgeholte Vorschauen sind Arbeit, auch wenn sich sonst nichts tat.
        summary.thumbnails = 300;
        assert_eq!(
            outcome(&summary),
            "Library unchanged · 300 thumbnails fetched"
        );
        summary.thumbnails = 0;

        summary.unchanged = false;
        assert_eq!(outcome(&summary), "Sync complete · nothing new");

        summary.stored = 1_234;
        summary.thumbnails = 12;
        summary.deleted = 2;
        assert_eq!(
            outcome(&summary),
            "Sync complete · 1,234 photos, 12 thumbnails, 2 removed"
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

    fn photo(id: &str, day: u32) -> Photo {
        Photo::new(icloud_photos_core::model::PhotoRecord {
            id: id.to_string(),
            master_id: None,
            filename: format!("{id}.HEIC"),
            added_date: DateTime::parse_from_rfc3339(&format!("2026-01-{day:02}T00:00:00Z"))
                .unwrap()
                .with_timezone(&chrono::Utc),
            asset_date: None,
            checksum: None,
            width: None,
            height: None,
            lat: None,
            lon: None,
            item_type: None,
        })
    }

    fn ids(model: &gio::ListStore) -> Vec<String> {
        (0..model.n_items())
            .map(|i| {
                let object = model.item(i).and_downcast::<PhotoObject>().unwrap();
                if object.is_placeholder() {
                    "_".to_string()
                } else {
                    object.id()
                }
            })
            .collect()
    }

    fn object(model: &gio::ListStore, index: u32) -> PhotoObject {
        model.item(index).and_downcast().unwrap()
    }

    /// Zaehlt, wie viele Positionen die Aenderungsmeldungen beruehren.
    fn watch(model: &gio::ListStore) -> Rc<Cell<u32>> {
        let touched = Rc::new(Cell::new(0));
        model.connect_items_changed({
            let touched = touched.clone();
            move |_, _, removed, added| touched.set(touched.get() + removed + added)
        });
        touched
    }

    #[test]
    fn diff_inserts_new_photos_without_replacing_the_others() {
        let model = gio::ListStore::new::<PhotoObject>();
        apply_diff(&model, &[photo("c", 3), photo("a", 1)]);
        let before_c = object(&model, 0);
        let before_a = object(&model, 1);
        let touched = watch(&model);

        // Ein neues Foto faellt zwischen die beiden.
        let real = apply_diff(&model, &[photo("c", 3), photo("b", 2), photo("a", 1)]);

        assert_eq!(real, 3);
        assert_eq!(ids(&model), ["c", "b", "a"]);
        // Dieselben Objekte: Auswahl und Scrollhalt bleiben erhalten.
        assert_eq!(object(&model, 0), before_c);
        assert_eq!(object(&model, 2), before_a);
        // Genau eine Einfuegung, kein Umbau des ganzen Modells.
        assert_eq!(touched.get(), 1);
    }

    #[test]
    fn diff_removes_deleted_photos_and_keeps_placeholders() {
        let model = gio::ListStore::new::<PhotoObject>();
        apply_diff(&model, &[photo("c", 3), photo("b", 2), photo("a", 1)]);
        model.append(&PhotoObject::placeholder());

        let real = apply_diff(&model, &[photo("c", 3), photo("a", 1)]);

        assert_eq!(real, 2);
        assert_eq!(ids(&model), ["c", "a", "_"]);
    }

    #[test]
    fn diff_handles_a_moved_photo() {
        let model = gio::ListStore::new::<PhotoObject>();
        apply_diff(&model, &[photo("c", 4), photo("b", 3), photo("a", 2)]);

        // Aufnahmedatum korrigiert: "c" rutscht ans Ende.
        apply_diff(&model, &[photo("b", 3), photo("a", 2), photo("c", 1)]);

        assert_eq!(ids(&model), ["b", "a", "c"]);
    }

    #[test]
    fn arriving_thumbnail_updates_the_object_in_place() {
        let model = gio::ListStore::new::<PhotoObject>();
        apply_diff(&model, &[photo("a", 1)]);
        let tile = object(&model, 0);
        assert!(tile.needs_skeleton());

        let notified = Rc::new(Cell::new(0));
        tile.connect_thumb_path_notify({
            let notified = notified.clone();
            move |_| notified.set(notified.get() + 1)
        });
        let touched = watch(&model);

        let mut with_thumb = photo("a", 1);
        with_thumb.thumb_path = Some(PathBuf::from("/cache/a.jpg"));
        apply_diff(&model, std::slice::from_ref(&with_thumb));

        assert_eq!(object(&model, 0), tile);
        assert!(!tile.needs_skeleton());
        assert_eq!(notified.get(), 1);
        assert_eq!(touched.get(), 0, "das Modell selbst bleibt unberuehrt");

        // Unveraendert: keine erneute Meldung, also kein Neuzeichnen.
        apply_diff(&model, std::slice::from_ref(&with_thumb));
        assert_eq!(notified.get(), 1);
    }

    #[test]
    fn sync_model_replaces_skeletons_with_arriving_photos() {
        let state = state();
        state.syncing.set(true);
        state.expected_total.set(5);
        state.sync_model();
        assert_eq!(ids(&state.model), ["_", "_", "_", "_", "_"]);

        let catalog = state.catalog.borrow();
        catalog.upsert_record(&photo("a", 1).record).unwrap();
        catalog.upsert_record(&photo("b", 2).record).unwrap();
        drop(catalog);
        state.sync_model();
        assert_eq!(ids(&state.model), ["b", "a", "_", "_", "_"]);

        state.syncing.set(false);
        state.sync_model();
        assert_eq!(ids(&state.model), ["b", "a"]);
    }
}

#[cfg(test)]
mod tile_tests {
    use super::*;

    #[test]
    fn pinch_snaps_to_the_nearest_tile_step() {
        assert_eq!(tile_step_for(THUMB_SIZE, 1.0), THUMB_SIZE);
        assert_eq!(tile_step_for(THUMB_SIZE, 1.4), 240);
        assert_eq!(tile_step_for(THUMB_SIZE, 0.7), 132);
        assert_eq!(tile_step_for(THUMB_SIZE, 10.0), 320);
        assert_eq!(tile_step_for(THUMB_SIZE, 0.01), 96);
    }
}
