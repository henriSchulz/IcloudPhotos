//! Der Sync-Manager: alles über die Synchronisation an einem Ort, getrennt
//! von der Bibliothek.
//!
//! Die Bibliothek ist zum Ansehen da. Ein Fortschrittsstreifen darüber lenkte
//! bei jedem Lauf ab, und ein Fehlerdialog riss mitten aus dem Betrachten.
//! Deshalb steht in der Kopfzeile der Bibliothek nur noch, wie viele Fotos da
//! sind, und ein Knopf für diesen Manager. Der Knopf zeigt dezent, ob gerade
//! ein Lauf arbeitet oder der letzte gescheitert ist; alles Weitere — Phase,
//! Balken, Bestand, Zeitpunkte, Cache, Fehler — steht hier.
//!
//! Ein Dialog statt eines zweiten Fensters: unter Hyprland landete ein eigenes
//! Fenster als Kachel neben der App statt über ihr.

use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;

use adw::prelude::*;

/// Was der Manager ausser dem laufenden Fortschritt anzeigt. Die Werte liest
/// das Fenster aus Katalog und Konfiguration.
pub struct Overview {
    pub photos: u64,
    pub apple_id: String,
    /// „vor 3 Minuten" oder `None`, wenn es noch keinen Lauf gab.
    pub last_sync: Option<String>,
    pub last_full_pass: Option<String>,
    /// Belegung der Originale, z.B. „405.2 MB von 5.0 GB".
    pub cache: String,
    /// Belegung der Vorschauen, z.B. „148.0 MB · 3.000 Vorschauen".
    pub thumbnails: String,
    pub thumbnail_dir: PathBuf,
    pub full_res_dir: PathBuf,
}

/// Der Sync-Manager samt Kopfzeilen-Knopf.
pub struct SyncManager {
    dialog: adw::Dialog,
    /// Knopf in der Kopfzeile der Bibliothek.
    button: gtk::Button,
    button_spinner: gtk::Spinner,
    button_icon: gtk::Image,

    spinner: gtk::Spinner,
    title: gtk::Label,
    detail: gtk::Label,
    bar: gtk::ProgressBar,
    sync_now: gtk::Button,

    photos_row: adw::ActionRow,
    icloud_row: adw::ActionRow,
    last_sync_row: adw::ActionRow,
    full_pass_row: adw::ActionRow,
    cache_row: adw::ActionRow,
    thumbs_row: adw::ActionRow,
    /// Wohin die Ordner-Knoepfe fuehren. Kommt mit jeder Uebersicht neu,
    /// weil die Pfade in der Konfiguration stehen.
    thumbnail_dir: RefCell<PathBuf>,
    full_res_dir: RefCell<PathBuf>,
    account_row: adw::ActionRow,
    result_row: adw::ActionRow,
    error_row: adw::ActionRow,

    /// Gesamtbestand laut iCloud, sobald bekannt.
    expected: Cell<u64>,
    /// Was ein fortgesetzter Erstaufbau schon hinter sich hat. Ohne diesen
    /// Sockel fienge der Balken bei null an, obwohl der halbe Katalog steht.
    base: Cell<u64>,
    /// Nur ein vollstaendiger Durchlauf laesst sich gegen den Gesamtbestand
    /// messen; ein Delta-Lauf zaehlt Aenderungen gegen nichts.
    full_pass: Cell<bool>,
    /// Wie viele Vorschauen der Nachlauf insgesamt holt.
    backfill_total: Cell<u64>,
    /// Hoechster bisher gezeigter Stand. Fotos werden in Bloecken von 256
    /// gemeldet, Vorschauen einzeln — beide speisen denselben Zaehler, und der
    /// soll dabei nicht zurueckspringen.
    reached: Cell<u64>,
    /// Ob der letzte Lauf gescheitert ist und das noch niemand gesehen hat.
    unseen_error: Cell<bool>,
    on_sync: RefCell<Option<Rc<dyn Fn()>>>,
}

impl SyncManager {
    pub fn new() -> Rc<Self> {
        // --- Kopfzeilen-Knopf -------------------------------------------
        let button_icon = gtk::Image::from_icon_name("emblem-synchronizing-symbolic");
        let button_spinner = gtk::Spinner::builder().visible(false).build();
        let button_content = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        button_content.append(&button_icon);
        button_content.append(&button_spinner);
        let button = gtk::Button::builder()
            .child(&button_content)
            .tooltip_text("Sync (Ctrl+Shift+R)")
            .build();

        // --- Aktueller Lauf ----------------------------------------------
        let spinner = gtk::Spinner::builder().visible(false).build();
        let title = gtk::Label::builder()
            .xalign(0.0)
            .hexpand(true)
            .wrap(true)
            .css_classes(["heading"])
            .label("Ready")
            .build();
        let detail = gtk::Label::builder()
            .xalign(1.0)
            .css_classes(["dim-label", "numeric"])
            .build();
        let line = gtk::Box::new(gtk::Orientation::Horizontal, 10);
        line.append(&spinner);
        line.append(&title);
        line.append(&detail);

        let bar = gtk::ProgressBar::builder().visible(false).build();

        let sync_now = gtk::Button::builder()
            .label("Sync Now")
            .halign(gtk::Align::Center)
            .css_classes(["pill", "suggested-action"])
            .build();

        let current = gtk::Box::new(gtk::Orientation::Vertical, 10);
        current.add_css_class("card");
        current.set_margin_bottom(6);
        let inner = gtk::Box::new(gtk::Orientation::Vertical, 10);
        inner.set_margin_top(14);
        inner.set_margin_bottom(14);
        inner.set_margin_start(16);
        inner.set_margin_end(16);
        inner.append(&line);
        inner.append(&bar);
        current.append(&inner);

        // --- Bestand und Zustand -----------------------------------------
        let row = |title: &str| {
            adw::ActionRow::builder()
                .title(title)
                .css_classes(["property"])
                .build()
        };
        let photos_row = row("Photos on This Device");
        let icloud_row = row("Photos in iCloud");
        icloud_row.set_subtitle("determined on the next full sync");
        let last_sync_row = row("Last Synced");
        let full_pass_row = row("Last Full Sync");
        let result_row = row("Last Result");
        result_row.set_visible(false);

        let library = adw::PreferencesGroup::builder().title("Library").build();
        library.add(&photos_row);
        library.add(&icloud_row);
        library.add(&last_sync_row);
        library.add(&full_pass_row);
        library.add(&result_row);

        let error_row = adw::ActionRow::builder()
            .title("Last Sync Failed")
            .subtitle_selectable(true)
            .css_classes(["error"])
            .visible(false)
            .build();
        error_row.add_prefix(&gtk::Image::from_icon_name("dialog-warning-symbolic"));
        let problems = adw::PreferencesGroup::new();
        problems.add(&error_row);

        let thumbs_row = row("Thumbnails");
        let cache_row = row("Originals Cache");
        let account_row = row("Apple Account");
        // Je ein Knopf, der den Ordner im Dateimanager oeffnet.
        let folder_button = || {
            gtk::Button::builder()
                .icon_name("folder-open-symbolic")
                .tooltip_text("Open Folder")
                .valign(gtk::Align::Center)
                .css_classes(["flat"])
                .build()
        };
        let open_thumbs = folder_button();
        let open_originals = folder_button();
        thumbs_row.add_suffix(&open_thumbs);
        cache_row.add_suffix(&open_originals);

        let details = adw::PreferencesGroup::builder()
            .title("Account and Storage")
            .build();
        details.add(&account_row);
        details.add(&thumbs_row);
        details.add(&cache_row);

        let column = gtk::Box::new(gtk::Orientation::Vertical, 18);
        column.set_margin_top(12);
        column.set_margin_bottom(24);
        column.set_margin_start(12);
        column.set_margin_end(12);
        column.append(&current);
        column.append(&sync_now);
        column.append(&problems);
        column.append(&library);
        column.append(&details);

        let clamp = adw::Clamp::builder()
            .maximum_size(520)
            .child(&column)
            .build();
        let scroller = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .propagate_natural_height(true)
            .child(&clamp)
            .build();

        let toolbar = adw::ToolbarView::new();
        toolbar.add_top_bar(&adw::HeaderBar::new());
        toolbar.set_content(Some(&scroller));

        let dialog = adw::Dialog::builder()
            .title("Sync")
            .content_width(480)
            .child(&toolbar)
            .build();

        let manager = Rc::new(Self {
            dialog,
            button,
            button_spinner,
            button_icon,
            spinner,
            title,
            detail,
            bar,
            sync_now,
            photos_row,
            icloud_row,
            last_sync_row,
            full_pass_row,
            cache_row,
            thumbs_row,
            thumbnail_dir: RefCell::default(),
            full_res_dir: RefCell::default(),
            account_row,
            result_row,
            error_row,
            expected: Cell::new(0),
            base: Cell::new(0),
            full_pass: Cell::new(false),
            backfill_total: Cell::new(0),
            reached: Cell::new(0),
            unseen_error: Cell::new(false),
            on_sync: RefCell::new(None),
        });

        manager.sync_now.connect_clicked({
            let manager = Rc::downgrade(&manager);
            move |_| {
                if let Some(manager) = manager.upgrade()
                    && let Some(on_sync) = manager.on_sync.borrow().clone()
                {
                    on_sync();
                }
            }
        });

        open_thumbs.connect_clicked({
            let manager = Rc::downgrade(&manager);
            move |button| {
                if let Some(manager) = manager.upgrade() {
                    open_folder(button, &manager.thumbnail_dir.borrow());
                }
            }
        });
        open_originals.connect_clicked({
            let manager = Rc::downgrade(&manager);
            move |button| {
                if let Some(manager) = manager.upgrade() {
                    open_folder(button, &manager.full_res_dir.borrow());
                }
            }
        });

        manager
    }

    /// Der Knopf fuer die Kopfzeile der Bibliothek.
    pub fn button(&self) -> &gtk::Button {
        &self.button
    }

    /// Was „Jetzt synchronisieren" ausloest.
    pub fn set_on_sync(&self, on_sync: Rc<dyn Fn()>) {
        *self.on_sync.borrow_mut() = Some(on_sync);
    }

    /// Oeffnet den Manager ueber dem Fenster.
    pub fn present(&self, parent: &impl IsA<gtk::Widget>) {
        // Wer den Manager aufmacht, hat den Fehler gesehen.
        self.unseen_error.set(false);
        self.update_button(self.spinner.is_visible());
        self.dialog.present(Some(parent));
    }

    /// Bestand, Zeitpunkte und Speicher — was auch ohne laufenden Sync gilt.
    pub fn show_overview(&self, overview: &Overview) {
        self.photos_row.set_subtitle(&thousands(overview.photos));
        self.last_sync_row
            .set_subtitle(overview.last_sync.as_deref().unwrap_or("never"));
        self.full_pass_row
            .set_subtitle(overview.last_full_pass.as_deref().unwrap_or("never"));
        self.cache_row.set_subtitle(&overview.cache);
        self.thumbs_row.set_subtitle(&overview.thumbnails);
        *self.thumbnail_dir.borrow_mut() = overview.thumbnail_dir.clone();
        *self.full_res_dir.borrow_mut() = overview.full_res_dir.clone();
        self.account_row
            .set_subtitle(if overview.apple_id.is_empty() {
                "not signed in"
            } else {
                &overview.apple_id
            });
    }

    fn update_button(&self, running: bool) {
        self.button_spinner.set_visible(running);
        self.button_spinner.set_spinning(running);
        let failed = self.unseen_error.get() && !running;
        self.button_icon.set_visible(!running);
        self.button_icon.set_icon_name(Some(if failed {
            "dialog-warning-symbolic"
        } else {
            "emblem-synchronizing-symbolic"
        }));
        self.button.set_tooltip_text(Some(if running {
            "Syncing (Ctrl+Shift+R)"
        } else if failed {
            "Last Sync Failed (Ctrl+Shift+R)"
        } else {
            "Sync (Ctrl+Shift+R)"
        }));
    }

    fn set_running(&self, running: bool) {
        self.spinner.set_visible(running);
        self.spinner.set_spinning(running);
        self.bar.set_visible(running);
        self.sync_now.set_sensitive(!running);
        self.update_button(running);
    }

    /// Ein Lauf beginnt.
    pub fn start(&self, resuming: Option<u64>, full_pass: bool) {
        self.expected.set(0);
        self.base.set(resuming.unwrap_or(0));
        self.full_pass.set(full_pass);
        self.reached.set(0);
        self.bar.set_fraction(0.0);
        self.error_row.set_visible(false);
        self.set_running(true);

        match resuming {
            // Ein fortgesetzter Erstaufbau sieht sonst aus wie einer, der
            // wieder von vorn anfängt.
            Some(offset) => {
                self.title.set_label("Resuming Interrupted Sync…");
                self.detail
                    .set_label(&format!("from {}", thousands(offset)));
            }
            None => {
                self.title.set_label("Connecting to iCloud…");
                self.detail.set_label("");
            }
        }
    }

    /// Die Anmeldung ist durch.
    pub fn authenticated(&self) {
        self.title.set_label("Syncing Library…");
    }

    /// Der Gesamtbestand ist bekannt — ab jetzt kann der Balken etwas zeigen.
    pub fn set_expected(&self, total: u64) {
        self.expected.set(total);
        self.icloud_row.set_subtitle(&thousands(total));
    }

    /// Fortschritt des Abgleichs.
    ///
    /// Gespeist von zwei Quellen: Metadaten kommen in Bloecken von 256,
    /// Vorschauen einzeln. Beide zaehlen dieselben Assets, deshalb gewinnt der
    /// hoehere Stand — sonst zuckte die Anzeige zwischen grob und fein hin und
    /// her.
    pub fn progress(&self, done: u64) {
        if done <= self.reached.get() {
            return;
        }
        self.reached.set(done);

        let total = self.base.get() + done;
        let expected = self.expected.get();
        if self.full_pass.get() && expected > 0 {
            self.title.set_label(if self.base.get() > 0 {
                "Loading Photos · Resumed"
            } else {
                "Loading Photos"
            });
            self.detail
                .set_label(&format!("{} / {}", thousands(total), thousands(expected)));
            self.bar
                .set_fraction((total as f64 / expected as f64).clamp(0.0, 1.0));
        } else {
            // Ohne Bezugsgroesse sagt ein Balkenstand nichts — ein wandernder
            // Balken immerhin, dass es weitergeht.
            self.title.set_label("Syncing Changes");
            self.detail.set_label(&thousands(total));
            self.bar.pulse();
        }
    }

    /// Der Nachlauf für fehlende Vorschauen. Eigene Phase mit eigener
    /// Bezugsgroesse: seine Zahl hat mit dem Bibliotheksbestand nichts zu tun.
    pub fn backfill(&self, missing: u64) {
        self.expected.set(0);
        self.base.set(0);
        self.full_pass.set(false);
        self.reached.set(0);
        self.title.set_label("Fetching Missing Thumbnails");
        self.detail
            .set_label(&format!("0 / {}", thousands(missing)));
        self.backfill_total.set(missing);
        self.bar.set_fraction(0.0);
    }

    /// Fortschritt im Nachlauf.
    pub fn backfilled(&self, done: u64) {
        let total = self.backfill_total.get();
        self.detail
            .set_label(&format!("{} / {}", thousands(done), thousands(total)));
        if total > 0 {
            self.bar
                .set_fraction((done as f64 / total as f64).clamp(0.0, 1.0));
        } else {
            self.bar.pulse();
        }
    }

    /// Die Alben werden geladen. Ihre Zahl steht erst mit dem ersten Album
    /// fest, bis dahin laeuft der Balken nur.
    pub fn albums(&self) {
        self.expected.set(0);
        self.base.set(0);
        self.full_pass.set(false);
        self.reached.set(0);
        self.title.set_label("Loading Albums");
        self.detail.set_label("");
        self.bar.pulse();
    }

    /// Fortschritt beim Laden der Alben.
    pub fn albums_loaded(&self, done: u64, total: u64) {
        self.detail
            .set_label(&format!("{} / {}", thousands(done), thousands(total)));
        if total > 0 {
            self.bar
                .set_fraction((done as f64 / total as f64).clamp(0.0, 1.0));
        }
    }

    pub fn deleting(&self, removed: u64) {
        self.title.set_label("Removing Deleted Photos");
        self.detail.set_label(&thousands(removed));
        self.bar.pulse();
    }

    /// Der Lauf ist durch.
    pub fn finished(&self, message: &str) {
        self.set_running(false);
        self.title.set_label("Up to Date");
        self.detail.set_label("");
        self.result_row.set_subtitle(message);
        self.result_row.set_visible(true);
    }

    /// Der Lauf ist gescheitert. Der Fehler bleibt im Manager stehen, statt
    /// als Dialog mitten ins Betrachten zu platzen; der Knopf in der
    /// Kopfzeile zeigt ein Warnzeichen, bis jemand hineingesehen hat.
    pub fn failed(&self, message: &str, detail: &str) {
        self.unseen_error.set(true);
        self.set_running(false);
        self.title.set_label(message);
        self.detail.set_label("");
        self.error_row.set_subtitle(detail);
        self.error_row.set_visible(true);
    }
}

/// Oeffnet einen Ordner im Dateimanager. Fehlt er noch — vor dem ersten
/// Download gibt es keinen Ordner fuer Originale —, wird er angelegt, statt
/// dass der Knopf wortlos nichts tut.
fn open_folder(button: &gtk::Button, dir: &std::path::Path) {
    if let Err(error) = std::fs::create_dir_all(dir) {
        tracing::warn!(dir = %dir.display(), %error, "Ordner nicht anlegbar");
        return;
    }
    let launcher = gtk::FileLauncher::new(Some(&gtk::gio::File::for_path(dir)));
    let window = button.root().and_downcast::<gtk::Window>();
    let dir = dir.to_path_buf();
    launcher.launch(
        window.as_ref(),
        gtk::gio::Cancellable::NONE,
        move |result| {
            if let Err(error) = result {
                tracing::warn!(dir = %dir.display(), %error, "Ordner liess sich nicht oeffnen");
            }
        },
    );
}

/// `19624` → `19.624`. Sechsstellige Zahlen ohne Trennung liest niemand.
pub fn thousands(value: u64) -> String {
    let digits = value.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            out.push(',');
        }
        out.push(digit);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn groups_thousands() {
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(7), "7");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(1_000), "1,000");
        assert_eq!(thousands(19_624), "19,624");
        assert_eq!(thousands(1_234_567), "1,234,567");
    }
}
