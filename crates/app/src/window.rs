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

use crate::collections::{Collections, Pick};
use crate::downloader::{self, DownloadMessage, Downloader};
use crate::export;
use crate::geocode::{self, Geocoder};
use crate::map_view::{self, MapView};
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
}

/// Was statt der Anmeldung im Inhaltsbereich steht.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Content {
    /// Das Raster — Bibliothek, Album oder ein Ort von der Karte.
    Grid,
    /// Die Übersicht „Collections".
    Collections,
    /// Die Karte.
    Map,
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
            content: Cell::new(Content::Grid),
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

    let status = gtk::Label::builder()
        .label("")
        .css_classes(["dim-label"])
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .build();

    // Die Bibliothek zeigt nur, wie viele Fotos da sind. Wie die
    // Synchronisation steht, steht im Sync-Manager hinter diesem Knopf.
    let manager = SyncManager::new();
    *state.sync_manager.borrow_mut() = Some(manager.clone());

    let header = adw::HeaderBar::new();
    // Ganz links, wie in Photos. Verbunden wird er, sobald es die
    // Seitenleiste gibt.
    let sidebar_toggle = gtk::ToggleButton::builder()
        .icon_name("sidebar-show-symbolic")
        .tooltip_text("Show Sidebar (F9)")
        .active(true)
        .build();
    header.pack_start(&sidebar_toggle);
    // Aus dem Raster eines Orts zurück zur Karte, dort wo man sie verlassen
    // hat. Nur dann sichtbar — die Seitenleiste bleibt dabei auf „Map".
    let back_to_map = gtk::Button::builder()
        .icon_name("go-previous-symbolic")
        .tooltip_text("Back to Map")
        .visible(false)
        .build();
    header.pack_start(&back_to_map);
    header.pack_start(manager.button());
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

    let stack = gtk::Stack::new();
    stack.add_named(&photos, Some("grid"));
    stack.add_named(&empty, Some("empty"));
    stack.add_named(&collections.widget, Some("collections"));
    stack.add_named(&map_view.widget, Some("map"));

    let library = gtk::Box::new(gtk::Orientation::Vertical, 0);
    library.append(&header);
    library.append(&stack);

    // Aufbau wie in Photos: links die Seitenleiste, rechts was sie gewählt
    // hat. Eine OverlaySplitView und keine NavigationSplitView: nur sie lässt
    // sich auch im breiten Fenster ausblenden. Auf schmalen Fenstern legt sie
    // sich über den Inhalt, statt ihn zu verdrängen.
    let sidebar = Sidebar::new();
    *state.sidebar.borrow_mut() = Some(sidebar.clone());
    let sidebar_view = adw::ToolbarView::new();
    sidebar_view.add_top_bar(
        &adw::HeaderBar::builder()
            .title_widget(&adw::WindowTitle::new("iCloud Photos", ""))
            .build(),
    );
    sidebar_view.set_content(Some(&sidebar.widget));
    let split = adw::OverlaySplitView::builder()
        .sidebar(&sidebar_view)
        .content(&library)
        .min_sidebar_width(200.0)
        .max_sidebar_width(280.0)
        .build();
    let narrow = adw::Breakpoint::new(adw::BreakpointCondition::new_length(
        adw::BreakpointConditionLengthType::MaxWidth,
        600.0,
        adw::LengthUnit::Sp,
    ));
    narrow.add_setter(&split, "collapsed", Some(&true.to_value()));
    window.add_breakpoint(narrow);

    // Ein- und ausblenden, per Knopf und mit F9 wie in GNOME üblich.
    let title = adw::WindowTitle::new("Library", "");
    header.set_title_widget(Some(&title));
    split
        .bind_property("show-sidebar", &sidebar_toggle, "active")
        .bidirectional()
        .sync_create()
        .build();

    // Die beiden Ebenen des Fensters. Ein geöffnetes Foto ist keine zweite
    // Fensterebene, sondern diese Seite — auf einem Kachel-Compositor landete
    // ein eigenes Fenster sonst als Kachel neben der App statt über ihr.
    let screens = gtk::Stack::new();
    screens.set_transition_type(gtk::StackTransitionType::Crossfade);
    screens.add_named(&split, Some(viewer::LIBRARY_PAGE));

    sidebar.connect_select({
        let state = state.clone();
        let status = status.clone();
        let stack = stack.clone();
        let title = title.clone();
        let split = split.clone();
        let empty = empty.clone();
        let back_to_map = back_to_map.clone();
        move |target| {
            let name = match target {
                Target::Library => "Library".to_string(),
                Target::Collections => "Collections".to_string(),
                Target::Map => "Map".to_string(),
                Target::Album(id) => state
                    .sidebar
                    .borrow()
                    .as_ref()
                    .and_then(|sidebar| {
                        sidebar
                            .albums()
                            .iter()
                            .find(|album| &album.id == id)
                            .map(display_name)
                    })
                    .unwrap_or_default(),
            };
            title.set_title(&name);
            back_to_map.set_visible(false);
            state.map_title.borrow_mut().take();
            state.content.set(match target {
                Target::Collections => Content::Collections,
                Target::Map => Content::Map,
                Target::Library | Target::Album(_) => Content::Grid,
            });
            match target {
                Target::Library => state.set_scope(Scope::Library),
                Target::Album(id) => state.set_scope(Scope::Album(id.clone())),
                Target::Collections => {
                    if let Some(collections) = state.collections.borrow().as_ref() {
                        collections.open_folder(None);
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
            update_status(&state, &status, &stack);
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
    state.refresh_albums();

    map_view.connect_pick({
        let state = state.clone();
        let status = status.clone();
        let stack = stack.clone();
        let title = title.clone();
        let back_to_map = back_to_map.clone();
        let grid = grid.clone();
        move |pick| show_place(&state, &status, &stack, &title, &back_to_map, &grid, pick)
    });
    back_to_map.connect_clicked({
        let sidebar = sidebar.clone();
        move |_| sidebar.select(&Target::Map)
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

    let toggle_sidebar = gio::SimpleAction::new("toggle-sidebar", None);
    toggle_sidebar.connect_activate({
        let split = split.clone();
        move |_, _| split.set_show_sidebar(!split.shows_sidebar())
    });
    group.add_action(&toggle_sidebar);
    app.set_accels_for_action("photos.toggle-sidebar", &["F9"]);

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

    manager.set_on_sync(trigger.clone());

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
    status: &gtk::Label,
    stack: &gtk::Stack,
    title: &adw::WindowTitle,
    back_to_map: &gtk::Button,
    grid: &gtk::GridView,
    pick: map_view::Pick,
) {
    let count = pick.ids.len() as u64;
    let (anchor, lat, lon) = pick.anchor;
    state.content.set(Content::Grid);
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
    back_to_map.set_visible(true);
    update_status(state, status, stack);
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
    let position = in_model.unwrap_or_else(|| u32::try_from(position).unwrap_or(count - 1).min(count - 1));
    tracing::info!(id, position, "Foto per Aktion geöffnet");
    open_viewer(state, window, position);
    window.present();
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
        overlay.set_tooltip_text(None);
        return;
    };

    badge.set_visible(photo.is_video());
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
fn set_image(picture: &gtk::Picture, path: &str) {
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

fn update_status(state: &Rc<AppState>, status: &gtk::Label, stack: &gtk::Stack) {
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
    stack.set_visible_child_name(page);

    // In der Bibliothek steht nur, wie viele Fotos da sind. Alles Weitere
    // gehoert in den Sync-Manager. Die Übersicht zählt keine Fotos.
    status.set_visible(state.content.get() == Content::Grid);
    status.set_label(&format!(
        "{} {}",
        thousands(shown),
        if shown == 1 { "Photo" } else { "Photos" }
    ));

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
    let button = gtk::MenuButton::builder()
        .icon_name("x-office-calendar-symbolic")
        .tooltip_text("Jump to a Month (Ctrl+J)")
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
        .tooltip_text("Export Selection…")
        .sensitive(false)
        .build();
    let copy = gtk::Button::builder()
        .icon_name("edit-copy-symbolic")
        .tooltip_text("Copy Selection")
        .sensitive(false)
        .build();
    let open = gtk::Button::builder()
        .icon_name("document-open-symbolic")
        .tooltip_text("Open in Default App")
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
