//! Hauptfenster: virtualisiertes Foto-Grid, Viewer und Sync-Steuerung.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use adw::prelude::*;
use gtk::{gdk, gio, glib};
use icloud_photos_cache::Catalog;
use icloud_photos_core::Config;
use std::path::PathBuf;

use crate::photo_object::PhotoObject;
use crate::sync_task::{self, SyncMessage};

/// So viele Katalogzeilen werden pro Nachladeschritt geholt. Der Katalog landet
/// dadurch nie vollständig im Speicher.
const PAGE_SIZE: u64 = 200;

/// Ab welcher Restscrollhöhe (in Pixeln) die nächste Seite geholt wird.
const PREFETCH_MARGIN: f64 = 600.0;

const THUMB_SIZE: i32 = 180;

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
        if total == 0 { "empty" } else { "grid" }
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
        self.model.remove_all();
        self.loaded.set(0);
        self.exhausted.set(false);
        self.load_next_page();
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

    // Nachladen, sobald das Ende in Sichtweite kommt.
    let adjustment = scroller.vadjustment();
    adjustment.connect_value_changed({
        let state = state.clone();
        move |adj| {
            let remaining = adj.upper() - (adj.value() + adj.page_size());
            if remaining < PREFETCH_MARGIN {
                state.load_next_page();
            }
        }
    });

    let empty = adw::StatusPage::builder()
        .icon_name("image-x-generic-symbolic")
        .title("Noch keine Fotos")
        .description("Starte einen Sync, um den Katalog zu füllen.")
        .build();

    let stack = gtk::Stack::new();
    stack.add_named(&scroller, Some("grid"));
    stack.add_named(&empty, Some("empty"));

    let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
    content.append(&header);
    content.append(&stack);
    window.set_content(Some(&content));

    state.load_next_page();

    let trigger: Rc<dyn Fn()> = {
        let state = state.clone();
        let status = status.clone();
        let stack = stack.clone();
        let window = window.clone();
        let sync_button = sync_button.clone();
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

            if state.authenticated.get() {
                status.set_label("Sync läuft …");
            } else {
                // Auch beim Start mit hinterlegtem Passwort: erst nach
                // `auth_ok` gilt die Anmeldung als durch.
                state.authenticating.set(true);
                state.login_busy(true);
                status.set_label("Anmeldung läuft …");
            }
            start_sync(
                state.clone(),
                status.clone(),
                stack.clone(),
                window.clone(),
                sync_button.clone(),
            );
        })
    };

    let login = build_login_page(&state, trigger.clone());
    stack.add_named(&login, Some("login"));

    sync_button.connect_clicked({
        let trigger = trigger.clone();
        move |_| trigger()
    });

    update_status(&state, &status, &stack);
    window.present();

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

        let label = gtk::Label::builder()
            .ellipsize(gtk::pango::EllipsizeMode::Middle)
            .max_width_chars(20)
            .css_classes(["caption", "dim-label"])
            .build();

        let boxed = gtk::Box::new(gtk::Orientation::Vertical, 4);
        boxed.append(&picture);
        boxed.append(&label);
        item.set_child(Some(&boxed));
    });

    factory.connect_bind(|_, item| {
        let item = item.downcast_ref::<gtk::ListItem>().unwrap();
        let Some(photo) = item.item().and_downcast::<PhotoObject>() else {
            return;
        };
        let Some(boxed) = item.child().and_downcast::<gtk::Box>() else {
            return;
        };
        let picture = boxed
            .first_child()
            .unwrap()
            .downcast::<gtk::Picture>()
            .unwrap();
        let label = boxed
            .last_child()
            .unwrap()
            .downcast::<gtk::Label>()
            .unwrap();

        label.set_label(&photo.filename());
        set_image(&picture, photo.thumb_path().as_deref());
    });

    // Freigeben, sonst hält jede recycelte Zelle ihre Textur fest.
    factory.connect_unbind(|_, item| {
        let item = item.downcast_ref::<gtk::ListItem>().unwrap();
        if let Some(boxed) = item.child().and_downcast::<gtk::Box>()
            && let Some(picture) = boxed.first_child().and_downcast::<gtk::Picture>()
        {
            picture.set_paintable(gdk::Paintable::NONE);
        }
    });

    let selection = gtk::SingleSelection::new(Some(state.model.clone()));
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
            open_viewer(&state, &window, &photo);
        }
    });

    grid
}

/// Öffnet ein Foto und vermerkt den Zugriff.
fn open_viewer(state: &Rc<AppState>, parent: &adw::ApplicationWindow, photo: &PhotoObject) {
    // Auch das reine Ansehen eines bereits gecachten Fotos zählt als Zugriff —
    // sonst verhält sich der Full-Res-Cache wie FIFO statt LRU.
    if let Err(error) = state.catalog.borrow().touch(&photo.id(), chrono_now()) {
        tracing::error!(%error, "last_accessed_at konnte nicht aktualisiert werden");
    }

    let picture = gtk::Picture::builder()
        .content_fit(gtk::ContentFit::Contain)
        .vexpand(true)
        .hexpand(true)
        .build();

    let body: gtk::Widget = match photo.cached_original() {
        Some(path) => {
            set_image(&picture, Some(&path));
            picture.upcast()
        }
        None => adw::StatusPage::builder()
            .icon_name("folder-download-symbolic")
            .title("Original nicht im Cache")
            .description(
                "Der Full-Res-Download ist noch nicht angebunden — \
                 siehe offene Punkte in CLAUDE.md.",
            )
            .build()
            .upcast(),
    };

    let header = adw::HeaderBar::new();
    let viewer = adw::Window::builder()
        .transient_for(parent)
        .modal(true)
        .title(photo.filename())
        .default_width(900)
        .default_height(700)
        .build();

    let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
    content.append(&header);
    content.append(&body);
    viewer.set_content(Some(&content));
    viewer.present();
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
    status.set_label(&format!(
        "{total} Fotos · Cache {} / {}",
        human_bytes(cached),
        human_bytes(limit)
    ));
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
) {
    let mut messages = sync_task::spawn(&state.config.borrow(), state.take_password());

    glib::spawn_future_local(async move {
        while let Some(message) = messages.recv().await {
            match message {
                SyncMessage::Progress { stored } => {
                    status.set_label(&format!("Sync läuft … {stored} Fotos"));
                }
                SyncMessage::Thumbnails { stored } => {
                    status.set_label(&format!("Lade Vorschauen … {stored}"));
                }
                SyncMessage::Authenticated { trusted } => {
                    // Erst hier ist die Anmeldung wirklich durch — Passwort und
                    // 2FA sind bestaetigt. Jetzt darf die Maske weichen.
                    tracing::info!(trusted, "Anmeldung abgeschlossen");
                    state.authenticated.set(true);
                    state.authenticating.set(false);
                    state.login_busy(false);
                    update_status(&state, &status, &stack);
                    status.set_label("Sync läuft …");
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
                    update_status(&state, &status, &stack);
                    status.set_label("Anmeldung fehlgeschlagen");
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

#[cfg(test)]
mod tests {
    use super::*;

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
    fn formats_each_unit_step() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(999), "999 B");
        assert_eq!(human_bytes(106_220), "106.2 KB");
        assert_eq!(human_bytes(5_000_000), "5.0 MB");
        assert_eq!(human_bytes(20_000_000), "20.0 MB");
        assert_eq!(human_bytes(5_000_000_000), "5.0 GB");
    }
}
