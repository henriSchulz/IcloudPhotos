//! Rahmen der Bibliothek nach Vorschlag A des Velum-Labs
//! (`~/Projects/velum/lab/web/photos.html?p=a`): der Seitenkopf mit großem
//! Titel, Brotkrume und den Knöpfen rechts, die Sync-Zeile unten in der
//! Seitenleiste und die schwebende Aktionsleiste der Auswahl.
//!
//! Der Kopf ersetzt die frühere `AdwHeaderBar`: er ist zugleich Griff zum
//! Verschieben des Fensters (`GtkWindowHandle`) und trägt rechts die
//! Fensterknöpfe, falls das System welche zeigt.

use std::cell::RefCell;
use std::rc::Rc;

use gtk::prelude::*;

use crate::sync_manager::SyncStatus;

type Callback = Rc<dyn Fn()>;

pub struct PageHead {
    pub widget: gtk::WindowHandle,
    /// Öffnet die Seitenleiste, solange sie eingeklappt ist (schmales Fenster).
    pub nav: gtk::Button,
    crumb: gtk::Box,
    crumb_label: gtk::Label,
    title: gtk::Label,
    subtitle: gtk::Label,
    /// Je Seite ein Satz Knöpfe; der Kopf zeigt den passenden.
    pub controls: gtk::Stack,
    pub window_controls: gtk::WindowControls,
    row: gtk::Box,
    titles: gtk::Box,
    crumb_action: RefCell<Option<Callback>>,
}

impl PageHead {
    pub fn new() -> Rc<Self> {
        let nav = gtk::Button::builder()
            .icon_name("sidebar-show-symbolic")
            .tooltip_text("Show Sidebar (F9)")
            .valign(gtk::Align::Center)
            .visible(false)
            .css_classes(["nav-button"])
            .build();
        let crumb_back = gtk::Button::builder()
            .icon_name("go-previous-symbolic")
            .css_classes(["flat", "crumb-back"])
            .build();
        let crumb_label = gtk::Label::new(None);
        let crumb = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .css_classes(["crumb"])
            .visible(false)
            .build();
        crumb.append(&crumb_back);
        crumb.append(&crumb_label);

        let title = gtk::Label::builder()
            .xalign(0.0)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .css_classes(["page-title"])
            .build();
        let title_row = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .css_classes(["title-row"])
            .build();
        title_row.append(&nav);
        title_row.append(&title);
        let subtitle = gtk::Label::builder()
            .xalign(0.0)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .css_classes(["page-subtitle"])
            .build();
        let titles = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .hexpand(true)
            .valign(gtk::Align::End)
            .css_classes(["titles"])
            .build();
        titles.append(&crumb);
        titles.append(&title_row);
        titles.append(&subtitle);

        let controls = gtk::Stack::builder()
            .valign(gtk::Align::End)
            .hhomogeneous(false)
            .vhomogeneous(false)
            .css_classes(["page-controls"])
            .build();
        let window_controls = gtk::WindowControls::builder()
            .side(gtk::PackType::End)
            .valign(gtk::Align::Start)
            .build();

        let row = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .css_classes(["photos-head"])
            .build();
        row.append(&titles);
        row.append(&controls);
        row.append(&window_controls);
        let widget = gtk::WindowHandle::builder().child(&row).build();

        let head = Rc::new(Self {
            widget,
            nav,
            crumb,
            crumb_label,
            title,
            subtitle,
            controls,
            window_controls,
            row: row.clone(),
            titles: titles.clone(),
            crumb_action: RefCell::new(None),
        });
        crumb_back.connect_clicked({
            let weak = Rc::downgrade(&head);
            move |_| {
                let action = weak
                    .upgrade()
                    .and_then(|head| head.crumb_action.borrow().clone());
                if let Some(action) = action {
                    action();
                }
            }
        });
        head
    }

    pub fn set(&self, title: &str, subtitle: &str) {
        self.title.set_label(title);
        self.subtitle.set_label(subtitle);
        self.subtitle.set_visible(!subtitle.is_empty());
    }

    pub fn title(&self) -> String {
        self.title.label().to_string()
    }

    /// Die Brotkrume über dem Titel: wohin der Pfeil zurückführt.
    pub fn set_crumb(&self, crumb: Option<(&str, Callback)>) {
        match crumb {
            Some((label, action)) => {
                self.crumb_label.set_label(label);
                *self.crumb_action.borrow_mut() = Some(action);
                self.crumb.set_visible(true);
            }
            None => {
                self.crumb_action.borrow_mut().take();
                self.crumb.set_visible(false);
            }
        }
    }

    /// Schmale Seite: die Knöpfe rutschen unter den Titel (im Lab bricht
    /// der Kopf um, `flex-wrap`), sonst bliebe vom Titel nur „…".
    pub fn set_compact(&self, compact: bool) {
        let inside = self
            .controls
            .parent()
            .is_some_and(|p| p == *self.titles.upcast_ref::<gtk::Widget>());
        if compact && !inside {
            self.row.remove(&self.controls);
            self.titles.append(&self.controls);
            self.controls.set_halign(gtk::Align::Start);
            self.controls.add_css_class("below");
        } else if !compact && inside {
            self.titles.remove(&self.controls);
            self.row
                .insert_child_after(&self.controls, Some(&self.titles));
            self.controls.set_halign(gtk::Align::Fill);
            self.controls.remove_css_class("below");
        }
    }

    pub fn add_controls(&self, name: &str, controls: &impl IsA<gtk::Widget>) {
        self.controls.add_named(controls, Some(name));
    }

    pub fn show_controls(&self, name: &str) {
        self.controls.set_visible_child_name(name);
    }
}

/// Eine Knopfreihe für den Kopf.
pub fn controls(children: &[&gtk::Widget]) -> gtk::Box {
    let row = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .css_classes(["control-row"])
        .valign(gtk::Align::Center)
        .build();
    for child in children {
        row.append(*child);
    }
    row
}

/// Ein Knopf mit Symbol (und optional Text) für den Kopf — macOS-förmig, die
/// Velum-Schicht gibt ihm die Fläche.
pub fn button(icon: Option<&str>, label: Option<&str>, tooltip: &str) -> gtk::Button {
    let button = gtk::Button::builder().tooltip_text(tooltip).build();
    match (icon, label) {
        (Some(icon), Some(label)) => {
            let content = adw::ButtonContent::builder()
                .icon_name(icon)
                .label(label)
                .build();
            button.set_child(Some(&content));
        }
        (Some(icon), None) => button.set_icon_name(icon),
        (None, Some(label)) => button.set_label(label),
        (None, None) => {}
    }
    button
}

/// Die Zeile unten in der Seitenleiste: Stand der Synchronisation, ein
/// Klick öffnet den Sync-Manager, der kleine Knopf synchronisiert sofort.
pub struct SyncRow {
    pub widget: gtk::Box,
    icon: gtk::Image,
    spinner: gtk::Spinner,
    title: gtk::Label,
    detail: gtk::Label,
    pub now: gtk::Button,
    texts: gtk::Box,
}

impl SyncRow {
    pub fn new(on_open: Callback, on_sync: Callback) -> Rc<Self> {
        let icon = gtk::Image::from_icon_name("emblem-synchronizing-symbolic");
        icon.add_css_class("sync-glyph");
        let spinner = gtk::Spinner::builder().visible(false).build();
        let title = gtk::Label::builder()
            .xalign(0.0)
            .label("Up to Date")
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .css_classes(["sync-title"])
            .build();
        let detail = gtk::Label::builder()
            .xalign(0.0)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .css_classes(["sync-detail"])
            .build();
        let texts = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .hexpand(true)
            .valign(gtk::Align::Center)
            .build();
        texts.append(&title);
        texts.append(&detail);
        let now = gtk::Button::builder()
            .icon_name("view-refresh-symbolic")
            .tooltip_text("Sync Now (Ctrl+R)")
            .valign(gtk::Align::Center)
            .css_classes(["flat", "sync-now"])
            .build();
        now.connect_clicked(move |_| on_sync());
        let widget = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .css_classes(["photos-syncrow"])
            .tooltip_text("Sync (Ctrl+Shift+R)")
            .build();
        widget.append(&icon);
        widget.append(&spinner);
        widget.append(&texts);
        widget.append(&now);
        let click = gtk::GestureClick::new();
        click.connect_released(move |gesture, _, _, _| {
            gesture.set_state(gtk::EventSequenceState::Claimed);
            on_open();
        });
        widget.add_controller(click);
        widget.set_cursor_from_name(Some("pointer"));
        Rc::new(Self {
            widget,
            icon,
            spinner,
            title,
            detail,
            now,
            texts,
        })
    }

    /// Zeigt den Stand: läuft, gescheitert oder aktuell, mit „vor 4 Min.".
    pub fn show(&self, status: &SyncStatus, last: Option<&str>) {
        self.spinner.set_visible(status.running);
        self.spinner.set_spinning(status.running);
        self.icon.set_visible(!status.running);
        self.icon.set_icon_name(Some(if status.failed {
            "dialog-warning-symbolic"
        } else {
            "emblem-synchronizing-symbolic"
        }));
        if status.failed {
            self.widget.add_css_class("failed");
        } else {
            self.widget.remove_css_class("failed");
        }
        self.title.set_label(if status.running {
            "Syncing…"
        } else if status.failed {
            "Sync Failed"
        } else {
            "Up to Date"
        });
        let detail = if status.running {
            status.phase.clone()
        } else {
            match last {
                Some(last) => format!("Synced {last}"),
                None => "Never synced".to_string(),
            }
        };
        self.detail.set_label(&detail);
        self.now.set_sensitive(!status.running);
    }

    /// In der Symbolleiste nur das Zeichen.
    pub fn set_rail(&self, rail: bool) {
        self.texts.set_visible(!rail);
        self.now.set_visible(!rail);
        if rail {
            self.widget.add_css_class("rail");
        } else {
            self.widget.remove_css_class("rail");
        }
    }
}

/// Ein Knopf der schwebenden Aktionsleiste: Symbol über kurzem Text.
pub fn bar_button(icon: &str, label: &str, tooltip: &str, action: &str) -> gtk::Button {
    let content = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .build();
    content.append(&gtk::Image::from_icon_name(icon));
    content.append(
        &gtk::Label::builder()
            .label(label)
            .css_classes(["bar-label"])
            .build(),
    );
    gtk::Button::builder()
        .child(&content)
        .tooltip_text(tooltip)
        .action_name(action)
        .css_classes(["flat", "bar-button"])
        .build()
}
