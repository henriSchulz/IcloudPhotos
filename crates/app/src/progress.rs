//! Die sichtbare Seite des Syncs.
//!
//! Vorher stand der ganze Sync in einer grauen Zeile am Rand der Kopfleiste:
//! „Sync läuft … 1234 / 19624". Wer nicht gezielt hinsah, bekam von einem
//! Lauf über mehrere Minuten nichts mit — und von einem fortgesetzten
//! Erstaufbau schon gar nicht. Dieser Streifen fährt stattdessen unter der
//! Kopfleiste aus, sagt, was gerade passiert, und zeigt es als Balken.

use std::cell::Cell;
use std::rc::Rc;

use adw::prelude::*;
use gtk::glib;

/// So lange bleibt die Abschlussmeldung stehen, bevor der Streifen einfährt.
const LINGER: std::time::Duration = std::time::Duration::from_secs(4);

/// Der Fortschrittsstreifen.
pub struct SyncBanner {
    revealer: gtk::Revealer,
    spinner: gtk::Spinner,
    title: gtk::Label,
    detail: gtk::Label,
    bar: gtk::ProgressBar,
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
    /// Zählt die Läufe. Ein verspätetes Ausblenden darf den nächsten Lauf
    /// nicht mit wegräumen — deshalb geteilt, der Zeitgeber liest mit.
    generation: Rc<Cell<u64>>,
}

impl SyncBanner {
    pub fn new() -> Rc<Self> {
        let spinner = gtk::Spinner::builder().spinning(true).build();

        let title = gtk::Label::builder()
            .xalign(0.0)
            .hexpand(true)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .css_classes(["heading"])
            .build();

        let detail = gtk::Label::builder()
            .xalign(1.0)
            .css_classes(["dim-label", "numeric"])
            .build();

        let bar = gtk::ProgressBar::builder().css_classes(["osd"]).build();

        let line = gtk::Box::new(gtk::Orientation::Horizontal, 10);
        line.append(&spinner);
        line.append(&title);
        line.append(&detail);

        let content = gtk::Box::new(gtk::Orientation::Vertical, 6);
        content.set_margin_top(8);
        content.set_margin_bottom(8);
        content.set_margin_start(14);
        content.set_margin_end(14);
        content.append(&line);
        content.append(&bar);
        content.add_css_class("sync-banner");

        let revealer = gtk::Revealer::builder()
            .transition_type(gtk::RevealerTransitionType::SlideDown)
            .child(&content)
            .reveal_child(false)
            .build();

        Rc::new(Self {
            revealer,
            spinner,
            title,
            detail,
            bar,
            expected: Cell::new(0),
            base: Cell::new(0),
            full_pass: Cell::new(false),
            reached: Cell::new(0),
            backfill_total: Cell::new(0),
            generation: Rc::new(Cell::new(0)),
        })
    }

    pub fn widget(&self) -> &gtk::Widget {
        self.revealer.upcast_ref()
    }

    /// Ein Lauf beginnt.
    pub fn start(&self, resuming: Option<u64>, full_pass: bool) {
        self.generation.set(self.generation.get() + 1);
        self.expected.set(0);
        self.base.set(resuming.unwrap_or(0));
        self.full_pass.set(full_pass);
        self.reached.set(0);
        self.spinner.set_visible(true);
        self.spinner.set_spinning(true);
        self.bar.set_fraction(0.0);
        self.bar.set_visible(true);

        match resuming {
            // Ein fortgesetzter Erstaufbau sieht sonst aus wie einer, der
            // wieder von vorn anfängt.
            Some(offset) => {
                self.title.set_label("Abgebrochenen Abgleich fortsetzen …");
                self.detail.set_label(&format!("ab {}", thousands(offset)));
            }
            None => {
                self.title.set_label("Verbinde mit iCloud …");
                self.detail.set_label("");
            }
        }
        self.revealer.set_reveal_child(true);
    }

    /// Die Anmeldung ist durch.
    pub fn authenticated(&self) {
        self.title.set_label("Gleiche Bibliothek ab …");
    }

    /// Der Gesamtbestand ist bekannt — ab jetzt kann der Balken etwas zeigen.
    pub fn set_expected(&self, total: u64) {
        self.expected.set(total);
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
                "Lade Fotos · Fortsetzung"
            } else {
                "Lade Fotos"
            });
            self.detail
                .set_label(&format!("{} / {}", thousands(total), thousands(expected)));
            self.bar
                .set_fraction((total as f64 / expected as f64).clamp(0.0, 1.0));
        } else {
            // Ohne Bezugsgroesse sagt ein Balkenstand nichts — ein wandernder
            // Balken immerhin, dass es weitergeht.
            self.title.set_label("Gleiche Änderungen ab");
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
        self.title.set_label("Hole fehlende Vorschauen");
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

    pub fn deleting(&self, removed: u64) {
        self.title.set_label("Entferne gelöschte Fotos");
        self.detail.set_label(&thousands(removed));
        self.bar.pulse();
    }

    /// Der Lauf ist durch. Die Meldung bleibt kurz stehen.
    pub fn finished(&self, message: &str) {
        self.spinner.set_spinning(false);
        self.spinner.set_visible(false);
        self.bar.set_visible(false);
        self.title.set_label(message);
        self.detail.set_label("");
        self.hide_soon();
    }

    /// Der Lauf ist gescheitert. Bleibt ebenfalls nur kurz stehen — den
    /// eigentlichen Fehler zeigt der Dialog.
    pub fn failed(&self, message: &str) {
        self.spinner.set_spinning(false);
        self.spinner.set_visible(false);
        self.bar.set_visible(false);
        self.title.set_label(message);
        self.detail.set_label("");
        self.hide_soon();
    }

    fn hide_soon(&self) {
        let generation = self.generation.get();
        let revealer = self.revealer.clone();
        let current = self.generation.clone();
        glib::timeout_add_local_once(LINGER, move || {
            // Läuft inzwischen ein neuer Sync, bleibt der Streifen stehen.
            if current.get() == generation {
                revealer.set_reveal_child(false);
            }
        });
    }
}

/// `19624` → `19.624`. Sechsstellige Zahlen ohne Trennung liest niemand.
pub fn thousands(value: u64) -> String {
    let digits = value.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            out.push('.');
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
        assert_eq!(thousands(1_000), "1.000");
        assert_eq!(thousands(19_624), "19.624");
        assert_eq!(thousands(1_234_567), "1.234.567");
    }
}
