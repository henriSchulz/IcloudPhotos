//! Velum-Optik, wenn der Desktop sie hat.
//!
//! Velum (`~/Projects/velum`) legt seine Farben, Radien und Schatten als
//! `--v-*`-Variablen über `~/.config/gtk-4.0/gtk.css` in jede GTK-App. Das
//! eigene Stylesheet `velum.css` benutzt nur diese Rollen — ohne die Schicht
//! wären sie leer, deshalb wird es nur dann geladen und die App behält sonst
//! ihre libadwaita-Optik.

use std::path::PathBuf;

use gtk::gdk;
use gtk::prelude::*;

const CSS: &str = include_str!("velum.css");

/// Klasse, unter der alle Regeln aus `velum.css` stehen.
pub const WINDOW_CLASS: &str = "velum-photos";

/// Ob die Velum-Schicht im Benutzer-Stylesheet von GTK steckt.
pub fn active() -> bool {
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")));
    config
        .and_then(|dir| std::fs::read_to_string(dir.join("gtk-4.0/gtk.css")).ok())
        .is_some_and(|css| css.contains("velum"))
}

/// Hängt das Stylesheet ein und markiert das Fenster. Über dem
/// Benutzer-Stylesheet: die Velum-Schicht dort rundet jede `.card` — auch die
/// Kacheln, die hier Kante an Kante liegen sollen. Damit steht es zugleich
/// über den übrigen App-Regeln (Skelett, Karte) und ersetzt sie.
pub fn apply(window: &impl IsA<gtk::Widget>) {
    if !active() {
        return;
    }
    let provider = gtk::CssProvider::new();
    provider.load_from_string(CSS);
    if let Some(display) = gdk::Display::default() {
        gtk::style_context_add_provider_for_display(
            &display,
            &provider,
            gtk::STYLE_PROVIDER_PRIORITY_USER + 1,
        );
    }
    window.add_css_class(WINDOW_CLASS);
}

// ── Tablet-Modus ──────────────────────────────────────────────────────────
//
// Velum schaltet den Desktop per `tablet: true` in
// `$XDG_STATE_HOME/velum/settings.json` auf Fingerbedienung. GTK kennt keine
// Umschaltung der Dichte wie `Theme.touch` in QML; die App liest den Schalter
// selbst, setzt die Klasse `velum-touch` (Größen aus `--v-touch-*`) und sagt
// den Teilen Bescheid, die sich anders verhalten (Antippen im Betrachter).

/// Klasse am Fenster, solange der Tablet-Modus an ist.
pub const TOUCH_CLASS: &str = "velum-touch";

fn state_dir() -> Option<PathBuf> {
    std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state")))
        .map(|dir| dir.join("velum"))
}

fn read_tablet() -> bool {
    state_dir()
        .and_then(|dir| std::fs::read_to_string(dir.join("settings.json")).ok())
        .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
        .and_then(|value| value.get("tablet").and_then(serde_json::Value::as_bool))
        .unwrap_or(false)
}

/// Wird bei jedem Wechsel des Tablet-Modus gerufen.
type TabletListener = Box<dyn Fn(bool)>;

thread_local! {
    static TABLET: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// Wer bei einem Wechsel Bescheid bekommt (etwa die Kopfzeile).
    static LISTENERS: std::cell::RefCell<Vec<TabletListener>> = const { std::cell::RefCell::new(Vec::new()) };
    /// Hält die Dateibeobachtung am Leben.
    static MONITOR: std::cell::RefCell<Option<gtk::gio::FileMonitor>> = const { std::cell::RefCell::new(None) };
}

/// Ob gerade mit dem Finger bedient wird.
pub fn tablet() -> bool {
    TABLET.get()
}

/// Ruft `f` jetzt und bei jedem Wechsel des Tablet-Modus.
pub fn on_tablet(f: impl Fn(bool) + 'static) {
    f(tablet());
    LISTENERS.with_borrow_mut(|list| list.push(Box::new(f)));
}

/// Liest den Schalter, setzt die Klasse und folgt späteren Änderungen.
pub fn follow_tablet(window: &impl IsA<gtk::Widget>) {
    let window = window.as_ref().clone();
    let apply = move || {
        let on = read_tablet();
        let changed = TABLET.replace(on) != on;
        if on {
            window.add_css_class(TOUCH_CLASS);
        } else {
            window.remove_css_class(TOUCH_CLASS);
        }
        if changed {
            LISTENERS.with_borrow(|list| list.iter().for_each(|f| f(on)));
        }
    };
    apply();
    let Some(dir) = state_dir() else {
        return;
    };
    // Beobachtet wird das Verzeichnis: Velum ersetzt die Datei per rename,
    // ein Monitor auf der Datei selbst verlöre sie dabei.
    let Ok(monitor) = gtk::gio::File::for_path(&dir).monitor_directory(
        gtk::gio::FileMonitorFlags::WATCH_MOVES,
        gtk::gio::Cancellable::NONE,
    ) else {
        return;
    };
    monitor.connect_changed(move |_, file, other, _| {
        let touches = |f: &gtk::gio::File| {
            f.basename()
                .is_some_and(|name| name.as_os_str() == "settings.json")
        };
        if touches(file) || other.is_some_and(touches) {
            apply();
        }
    });
    MONITOR.with_borrow_mut(|slot| *slot = Some(monitor));
}

// ── Federn ────────────────────────────────────────────────────────────────

fn core_token(path: &[&str]) -> Option<serde_json::Value> {
    let path_js = std::env::var_os("HOME")
        .map(|home| PathBuf::from(home).join(".local/share/velum/qml/Velum/Tokens.js"))?;
    let text = std::fs::read_to_string(path_js).ok()?;
    let line = text
        .lines()
        .find_map(|line| line.strip_prefix("var core = "))?;
    let mut value: serde_json::Value = serde_json::from_str(line.trim_end_matches(';')).ok()?;
    for key in path {
        value = value.get(*key)?.clone();
    }
    Some(value)
}

/// Velums Dauer `name` (instant, fast, base, slow …) in ms; ohne Velum
/// GTKs übliche 250 ms.
pub fn duration(name: &str) -> u32 {
    core_token(&["motion", "duration", name])
        .and_then(|value| value.as_f64())
        .map(|ms| ms.round() as u32)
        .unwrap_or(250)
}

/// Velums Feder `name` (smooth, snappy, gentle, bouncy, interactive) als
/// libadwaita-Feder. Gelesen aus `qml/Velum/Tokens.js` der installierten
/// Velum-Kopie — dieselben Werte, mit denen die Shell federt. Ohne Velum eine
/// kritisch gedämpfte Feder mit libadwaitas Steifigkeit.
pub fn spring(name: &str) -> adw::SpringParams {
    let preset = core_token(&["motion", "spring", name]).and_then(|spring| {
        Some((
            spring.get("damping")?.as_f64()?,
            spring.get("stiffness")?.as_f64()?,
        ))
    });
    match preset {
        Some((damping, stiffness)) => adw::SpringParams::new_full(damping, 1.0, stiffness),
        None => adw::SpringParams::new(1.0, 1.0, 100.0),
    }
}
