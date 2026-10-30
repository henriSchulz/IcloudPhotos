//! Velums Symbole statt der Symbole des Systems.
//!
//! Komponenten nennen Velum-Namen („trash", „chevron.left", „heart"); welche
//! Schrift sie zeichnet, entscheidet Velum: `icons/sf.json` hält die
//! Codepunkte der lokal installierten SF Symbols (die Schrift wird nie
//! mitgeliefert), `icons/aliases.json` + `icons/phosphor.json` die Phosphor-
//! Glyphen (MIT, in Velum enthalten). Mit `icons: "sf"` in Velums
//! Einstellungen nimmt ein Name, den SF kennt, SF — der Rest bleibt Phosphor,
//! wie in `VIcon` (QML) und `velum-icons.css` (Web).
//!
//! Gezeichnet wird ein Label mit der Glyphe: die Farbe folgt dem Text um sie
//! herum (Hover, Wahl, Akzent) ohne eigenes Umfärben, die Größe kommt aus dem
//! Stylesheet (`font-size` an `.glyph`). Phosphor ist nicht im System
//! installiert und wird beim ersten Gebrauch in Pangos Schriftliste geladen.
//!
//! Ohne Velum (kein `VELUM_HOME`, keine `~/.local/share/velum`) bleibt es bei
//! den Symbolen des Systems — `fallback` nennt sie.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;

use gtk::prelude::*;
use pango::prelude::FontMapExt;

/// Strichstärke wie in Velum: regular in Listen und Knöpfen, fill in
/// Kennzeichen, bold für kleine Marken (Haken, Pfeile, Schließen).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Weight {
    Regular,
    Bold,
    Fill,
}

struct Set {
    aliases: HashMap<String, String>,
    phosphor: HashMap<String, HashMap<String, u32>>,
    sf: HashMap<String, (u32, u32)>,
    sf_family: String,
    use_sf: bool,
}

thread_local! {
    static SET: RefCell<Option<Option<Set>>> = const { RefCell::new(None) };
}

fn velum_home() -> Option<PathBuf> {
    std::env::var_os("VELUM_HOME")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share/velum"))
        })
        .filter(|dir| dir.join("icons/aliases.json").is_file())
}

fn read_json(path: PathBuf) -> Option<serde_json::Value> {
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

/// Welche Bibliothek Velum gewählt hat (`settings.json` → `icons`).
fn wants_sf() -> bool {
    let state = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state")));
    state
        .and_then(|dir| read_json(dir.join("velum/settings.json")))
        .and_then(|value| value.get("icons")?.as_str().map(str::to_string))
        .is_none_or(|icons| icons == "sf")
}

fn load() -> Option<Set> {
    let home = velum_home()?;
    let aliases: HashMap<String, String> = read_json(home.join("icons/aliases.json"))?
        .as_object()?
        .iter()
        .filter(|(key, _)| !key.starts_with('$'))
        .filter_map(|(key, value)| Some((key.clone(), value.as_str()?.to_string())))
        .collect();
    let mut phosphor = HashMap::new();
    let weights = read_json(home.join("icons/phosphor.json"))?;
    for (weight, glyphs) in weights.get("weights")?.as_object()? {
        let map: HashMap<String, u32> = glyphs
            .as_object()?
            .iter()
            .filter_map(|(name, code)| Some((name.clone(), u32::try_from(code.as_u64()?).ok()?)))
            .collect();
        phosphor.insert(weight.clone(), map);
    }
    let sf_json = read_json(home.join("icons/sf.json"));
    let sf_family = sf_json
        .as_ref()
        .and_then(|json| json.get("family")?.as_str().map(str::to_string))
        .unwrap_or_else(|| ".SF Symbols Fallback".to_string());
    let sf: HashMap<String, (u32, u32)> = sf_json
        .as_ref()
        .and_then(|json| json.get("icons")?.as_object().cloned())
        .map(|icons| {
            icons
                .iter()
                .filter_map(|(name, codes)| {
                    let codes = codes.as_array()?;
                    let parse = |v: &serde_json::Value| u32::from_str_radix(v.as_str()?, 16).ok();
                    Some((
                        name.clone(),
                        (parse(codes.first()?)?, parse(codes.get(1)?)?),
                    ))
                })
                .collect()
        })
        .unwrap_or_default();

    // Phosphor in Pangos Schriftliste — dieselbe, aus der GTK zeichnet.
    let font_map = gtk::Label::new(None).pango_context().font_map();
    let mut sf_installed = false;
    if let Some(font_map) = font_map {
        for file in ["Phosphor.ttf", "Phosphor-Bold.ttf", "Phosphor-Fill.ttf"] {
            let path = home.join("qml/Velum/fonts").join(file);
            if let Err(error) = font_map.add_font_file(&path) {
                tracing::warn!(path = %path.display(), %error, "Phosphor nicht ladbar");
            }
        }
        sf_installed = font_map
            .list_families()
            .iter()
            .any(|family| family.name() == sf_family);
    }
    Some(Set {
        aliases,
        phosphor,
        sf,
        sf_family,
        use_sf: sf_installed && wants_sf(),
    })
}

fn with_set<R>(f: impl FnOnce(Option<&Set>) -> R) -> R {
    SET.with_borrow_mut(|slot| {
        let set = slot.get_or_insert_with(load);
        f(set.as_ref())
    })
}

/// Schrift und Zeichen für einen Velum-Namen.
fn resolve(name: &str, weight: Weight) -> Option<(String, char)> {
    with_set(|set| {
        let set = set?;
        if set.use_sf
            && let Some((outline, filled)) = set.sf.get(name)
        {
            let code = if weight == Weight::Fill {
                *filled
            } else {
                *outline
            };
            return Some((set.sf_family.clone(), char::from_u32(code)?));
        }
        let phosphor = set.aliases.get(name)?;
        let (family, table) = match weight {
            Weight::Regular => ("Phosphor", "regular"),
            Weight::Bold => ("Phosphor-Bold", "bold"),
            Weight::Fill => ("Phosphor-Fill", "fill"),
        };
        let code = *set.phosphor.get(table)?.get(phosphor)?;
        Some((family.to_string(), char::from_u32(code)?))
    })
}

/// Das Symbol `name` als Widget. Ohne Velum das Systemsymbol `fallback`.
pub fn glyph(name: &str, weight: Weight, fallback: &str) -> gtk::Widget {
    match resolve(name, weight) {
        Some((family, code)) => {
            let label = gtk::Label::builder()
                .label(code.to_string())
                .halign(gtk::Align::Center)
                .valign(gtk::Align::Center)
                .accessible_role(gtk::AccessibleRole::Presentation)
                .css_classes(["glyph"])
                .build();
            let attrs = pango::AttrList::new();
            attrs.insert(pango::AttrString::new_family(&family));
            // SF zeichnet „bold" als Umriss in fetterem Schnitt, wie VIcon.
            if weight == Weight::Bold && family.starts_with(".SF") {
                attrs.insert(pango::AttrInt::new_weight(pango::Weight::Bold));
            }
            label.set_attributes(Some(&attrs));
            label.upcast()
        }
        None => gtk::Image::builder()
            .icon_name(fallback)
            .css_classes(["glyph"])
            .build()
            .upcast(),
    }
}

/// Das Systemsymbol für einen Velum-Namen — nur ohne Velum gebraucht.
pub fn fallback(name: &str) -> &'static str {
    match name {
        "personal" => "user-home-symbolic",
        "image" => "image-x-generic-symbolic",
        "layout.tiles" => "view-grid-symbolic",
        "layout.columns" => "zoom-fit-best-symbolic",
        "network" => "mark-location-symbolic",
        "heart" => "starred-symbolic",
        "film" => "camera-video-symbolic",
        "display" => "applets-screenshooter-symbolic",
        "clock" => "alarm-symbolic",
        "duplicate" => "edit-copy-symbolic",
        "folder" => "folder-symbolic",
        "chevron.left" => "go-previous-symbolic",
        "chevron.right" => "go-next-symbolic",
        "chevron.down" => "pan-down-symbolic",
        "check" => "object-select-symbolic",
        "close" => "window-close-symbolic",
        "plus" => "list-add-symbolic",
        "more" => "view-more-symbolic",
        "search" => "system-search-symbolic",
        "sidebar" => "sidebar-show-symbolic",
        "share" => "document-save-symbolic",
        "downloads" => "document-save-as-symbolic",
        "trash" => "user-trash-symbolic",
        "editor" => "document-edit-symbolic",
        "callout" => "dialog-information-symbolic",
        "fullscreen" => "view-fullscreen-symbolic",
        "fullscreen.exit" => "view-restore-symbolic",
        "restart" => "view-refresh-symbolic",
        "calendar" => "x-office-calendar-symbolic",
        "open.external" => "document-open-symbolic",
        "play" => "media-playback-start-symbolic",
        "warning" => "dialog-warning-symbolic",
        "cloud" => "emblem-synchronizing-symbolic",
        _ => "image-missing-symbolic",
    }
}

/// `glyph` mit dem Rückfall aus `fallback`.
pub fn icon(name: &str, weight: Weight) -> gtk::Widget {
    glyph(name, weight, fallback(name))
}

/// Tauscht das Zeichen eines Symbols aus (etwa Herz ↔ volles Herz), ohne
/// das Widget neu zu bauen.
pub fn set_glyph(widget: &gtk::Widget, name: &str, weight: Weight, fallback: &str) {
    if let Some(label) = widget.downcast_ref::<gtk::Label>()
        && let Some((family, code)) = resolve(name, weight)
    {
        label.set_label(&code.to_string());
        let attrs = pango::AttrList::new();
        attrs.insert(pango::AttrString::new_family(&family));
        if weight == Weight::Bold && family.starts_with(".SF") {
            attrs.insert(pango::AttrInt::new_weight(pango::Weight::Bold));
        }
        label.set_attributes(Some(&attrs));
    } else if let Some(image) = widget.downcast_ref::<gtk::Image>() {
        image.set_icon_name(Some(fallback));
    }
}

/// Ein Knopf mit Symbol (und optional Text) — macOS-förmig, die Velum-Schicht
/// gibt ihm die Fläche.
pub fn button(name: &str, fallback: &str, label: Option<&str>, tooltip: &str) -> gtk::Button {
    let button = gtk::Button::builder().tooltip_text(tooltip).build();
    button.set_child(Some(&content(name, fallback, label)));
    if label.is_none() {
        button.add_css_class("image-button");
    }
    button
}

/// Symbol und Text nebeneinander, für Knöpfe.
pub fn content(name: &str, fallback: &str, label: Option<&str>) -> gtk::Widget {
    let glyph = glyph(name, Weight::Regular, fallback);
    let Some(text) = label else {
        return glyph;
    };
    let row = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .css_classes(["glyph-label"])
        .build();
    row.append(&glyph);
    row.append(&gtk::Label::new(Some(text)));
    row.upcast()
}

/// Ein Menüknopf mit Symbol.
pub fn menu_button(name: &str, fallback: &str, tooltip: &str) -> gtk::MenuButton {
    let button = gtk::MenuButton::builder().tooltip_text(tooltip).build();
    button.set_child(Some(&glyph(name, Weight::Regular, fallback)));
    button
}
