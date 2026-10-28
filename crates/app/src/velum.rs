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
