//! Hauptfenster: virtualisiertes Foto-Grid, Viewer und Sync-Steuerung.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use adw::prelude::*;
use gtk::{gdk, gio, glib};
use icloud_photos_cache::Catalog;
use icloud_photos_core::Config;

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
    pub config: Config,
    /// Eigene Verbindung des UI-Threads. Der Sync schreibt über eine zweite.
    pub catalog: RefCell<Catalog>,
    pub model: gio::ListStore,
    loaded: Cell<u64>,
    exhausted: Cell<bool>,
    syncing: Cell<bool>,
}

impl AppState {
    pub fn new(config: Config, catalog: Catalog) -> Rc<Self> {
        Rc::new(Self {
            config,
            catalog: RefCell::new(catalog),
            model: gio::ListStore::new::<PhotoObject>(),
            loaded: Cell::new(0),
            exhausted: Cell::new(false),
            syncing: Cell::new(false),
        })
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
    update_status(&state, &status, &stack);

    sync_button.connect_clicked({
        let state = state.clone();
        let status = status.clone();
        let stack = stack.clone();
        let window = window.clone();
        let sync_button = sync_button.clone();
        move |_| {
            if state.syncing.replace(true) {
                return;
            }
            sync_button.set_sensitive(false);
            status.set_label("Sync läuft …");
            start_sync(
                state.clone(),
                status.clone(),
                stack.clone(),
                window.clone(),
                sync_button.clone(),
            );
        }
    });

    window.present();
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
    let limit = state.config.cache.limit_bytes();

    stack.set_visible_child_name(if total == 0 { "empty" } else { "grid" });
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
    let mut messages = sync_task::spawn(&state.config);

    glib::spawn_future_local(async move {
        while let Some(message) = messages.recv().await {
            match message {
                SyncMessage::Progress { stored } => {
                    status.set_label(&format!("Sync läuft … {stored} Fotos"));
                }
                SyncMessage::TwoFactorRequired { responder } => {
                    let code = ask_two_factor_code(&window).await;
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
                    tracing::info!(stored = summary.stored, "Sync fertig");
                    state.reload();
                    update_status(&state, &status, &stack);
                }
                SyncMessage::Finished(Err(error)) => {
                    tracing::error!(%error, "Sync fehlgeschlagen");
                    show_error(&window, &error);
                    update_status(&state, &status, &stack);
                }
            }
        }
        state.syncing.set(false);
        sync_button.set_sensitive(true);
    });
}

/// Blockierender Dialog für den 2FA-Code.
async fn ask_two_factor_code(parent: &adw::ApplicationWindow) -> Option<String> {
    let entry = gtk::Entry::builder()
        .placeholder_text("6-stelliger Code")
        .input_purpose(gtk::InputPurpose::Digits)
        .max_length(6)
        .activates_default(true)
        .build();

    let dialog = adw::AlertDialog::builder()
        .heading("Zwei-Faktor-Authentifizierung")
        .body("Apple hat einen Bestätigungscode an deine Geräte geschickt.")
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
    let code = entry.text().trim().to_string();
    if code.is_empty() { None } else { Some(code) }
}

fn show_error(parent: &adw::ApplicationWindow, error: &str) {
    let dialog = adw::AlertDialog::builder()
        .heading("Sync fehlgeschlagen")
        .body(error)
        .build();
    dialog.add_response("ok", "OK");
    dialog.present(Some(parent));
}

#[cfg(test)]
mod tests {
    use super::human_bytes;

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
