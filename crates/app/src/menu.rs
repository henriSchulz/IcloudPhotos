//! Menüs mit Velums Symbolen.
//!
//! `GtkPopoverMenu` zeichnet Zeilen aus einem `GMenu` und kennt dafür nur
//! Symbole des Systems. Velums Menüs tragen aber vor jedem Eintrag ihr Symbol
//! (Lab `photos.html`, das Kontextmenü des Finders). Deshalb baut dieses Modul
//! das Menü selbst: ein Popover mit Zeilen aus Knöpfen — Symbol, Text, rechts
//! das Kürzel —, optional oben eine Reihe großer Knöpfe. Jede Zeile löst eine
//! Aktion aus (`photos.save`, `viewer.delete` …): ist sie gesperrt, ist es die
//! Zeile auch. Das Menü öffnet ohne Übergang und schließt nach einem Klick.
//!
//! Farben, Radien und Abstände nehmen die Menü-Rollen (`--v-menu-*`) im
//! Stylesheet der App (`popover.photos-menu`).

use std::rc::Rc;

use gtk::prelude::*;

use crate::icons::{self, Weight};

/// Ein Eintrag.
pub enum Entry {
    /// Symbol (Velum-Name, Systemsymbol als Rückfall), Text, Aktion, Kürzel.
    Item {
        icon: &'static str,
        fallback: &'static str,
        label: &'static str,
        action: &'static str,
        key: Option<&'static str>,
        danger: bool,
    },
    Separator,
}

pub fn item(
    icon: &'static str,
    fallback: &'static str,
    label: &'static str,
    action: &'static str,
    key: Option<&'static str>,
) -> Entry {
    Entry::Item {
        icon,
        fallback,
        label,
        action,
        key,
        danger: false,
    }
}

pub fn danger(
    icon: &'static str,
    fallback: &'static str,
    label: &'static str,
    action: &'static str,
    key: Option<&'static str>,
) -> Entry {
    Entry::Item {
        icon,
        fallback,
        label,
        action,
        key,
        danger: true,
    }
}

/// Ein großer Knopf der Reihe oben (Export, Copy, Favorite, Delete).
pub struct Big {
    pub icon: &'static str,
    pub fallback: &'static str,
    pub label: &'static str,
    pub action: &'static str,
    pub danger: bool,
}

/// Ein gebautes Menü.
pub struct Menu {
    pub popover: gtk::Popover,
    /// Die großen Knöpfe, damit sich etwa „Favorite"/„Unfavorite" umschreiben
    /// lässt, bevor das Menü aufgeht.
    pub big: Vec<gtk::Button>,
}

pub fn build(big: &[Big], entries: &[Entry]) -> Rc<Menu> {
    let column = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .css_classes(["menu-column"])
        .build();
    let popover = gtk::Popover::builder()
        .has_arrow(false)
        .child(&column)
        .css_classes(["photos-menu"])
        .build();
    let close = {
        let popover = popover.downgrade();
        move || {
            if let Some(popover) = popover.upgrade() {
                popover.popdown();
            }
        }
    };
    let mut big_buttons = Vec::new();
    if !big.is_empty() {
        let row = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .homogeneous(true)
            .css_classes(["menu-actions"])
            .build();
        for entry in big {
            let content = gtk::Box::builder()
                .orientation(gtk::Orientation::Vertical)
                .build();
            content.append(&icons::glyph(entry.icon, Weight::Regular, entry.fallback));
            content.append(
                &gtk::Label::builder()
                    .label(entry.label)
                    .css_classes(["bar-label"])
                    .build(),
            );
            let button = gtk::Button::builder()
                .child(&content)
                .action_name(entry.action)
                .tooltip_text(entry.label)
                .css_classes(["menu-action"])
                .build();
            if entry.danger {
                button.add_css_class("danger");
            }
            button.connect_clicked({
                let close = close.clone();
                move |_| close()
            });
            row.append(&button);
            big_buttons.push(button);
        }
        column.append(&row);
        column.append(&separator());
    }
    for entry in entries {
        match entry {
            Entry::Separator => column.append(&separator()),
            Entry::Item {
                icon,
                fallback,
                label,
                action,
                key,
                danger,
            } => {
                let content = gtk::Box::builder()
                    .orientation(gtk::Orientation::Horizontal)
                    .css_classes(["menu-row"])
                    .build();
                content.append(&icons::glyph(icon, Weight::Regular, fallback));
                content.append(
                    &gtk::Label::builder()
                        .label(*label)
                        .xalign(0.0)
                        .hexpand(true)
                        .build(),
                );
                if let Some(key) = key {
                    content.append(
                        &gtk::Label::builder()
                            .label(*key)
                            .css_classes(["menu-key"])
                            .build(),
                    );
                }
                let button = gtk::Button::builder()
                    .child(&content)
                    .action_name(*action)
                    .css_classes(["menu-item"])
                    .build();
                if *danger {
                    button.add_css_class("danger");
                }
                button.connect_clicked({
                    let close = close.clone();
                    move |_| close()
                });
                column.append(&button);
            }
        }
    }
    Rc::new(Menu {
        popover,
        big: big_buttons,
    })
}

fn separator() -> gtk::Separator {
    gtk::Separator::builder()
        .orientation(gtk::Orientation::Horizontal)
        .css_classes(["menu-separator"])
        .build()
}

/// Schreibt einen großen Knopf um (Symbol, Text).
pub fn relabel(button: &gtk::Button, icon: &str, weight: Weight, fallback: &str, label: &str) {
    let Some(content) = button.child().and_downcast::<gtk::Box>() else {
        return;
    };
    if let Some(glyph) = content.first_child() {
        icons::set_glyph(&glyph, icon, weight, fallback);
        if let Some(text) = glyph.next_sibling().and_downcast::<gtk::Label>() {
            text.set_label(label);
        }
    }
    button.set_tooltip_text(Some(label));
}
