//! „Years" und „Months" — die gröberen Stufen der Bibliothek, wie im Lab
//! (`photos.html?p=a&s=library&z=years|months`): je Jahr eine große Karte mit
//! dem jüngsten Foto, je Monat eine Karte mit drei Fotos. Ein Klick springt
//! im Raster („All") an den Anfang dieses Jahres oder Monats.
//!
//! Die Zahlen kommen aus `Catalog::month_groups` — dieselbe Liste wie hinter
//! dem Monatssprung (Strg+J), samt Rasterposition jedes Monats.

use std::rc::Rc;

use gtk::prelude::*;
use icloud_photos_cache::{Catalog, DateGroup};

use crate::home::{label, photos_word, thumb};
use crate::layout::RatioBox;
use crate::photo_object::month_name;
use crate::velum;

/// Ein Jahr aus den Monaten: Jahr, Fotos, Rasterposition seines ersten.
pub fn years(groups: &[DateGroup]) -> Vec<(String, u64, u64)> {
    let mut out: Vec<(String, u64, u64)> = Vec::new();
    for group in groups {
        let year = group.month.get(..4).unwrap_or(&group.month).to_string();
        match out.last_mut() {
            Some((last, count, _)) if *last == year => *count += group.count,
            _ => out.push((year, group.count, group.offset)),
        }
    }
    out
}

pub struct Overview {
    pub widget: gtk::ScrolledWindow,
    flow: gtk::FlowBox,
    on_pick: Rc<dyn Fn(u64)>,
}

impl Overview {
    /// `on_pick` bekommt die Rasterposition des gewählten Jahres oder Monats.
    pub fn new(on_pick: impl Fn(u64) + 'static) -> Rc<Self> {
        let flow = gtk::FlowBox::builder()
            .homogeneous(true)
            .selection_mode(gtk::SelectionMode::None)
            .max_children_per_line(8)
            .valign(gtk::Align::Start)
            .row_spacing(velum::photos("cardGap", 12.0) as u32)
            .column_spacing(velum::photos("cardGap", 12.0) as u32)
            .css_classes(["overview-grid"])
            .build();
        let widget = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .child(&flow)
            .css_classes(["photos-pane"])
            .build();
        // Die Seite bekommt ihre Wunschhöhe, nicht ihr Minimum: Titelbilder
        // melden kein Minimum (siehe `layout::RatioBox`).
        if let Some(viewport) = widget.child().and_downcast::<gtk::Viewport>() {
            viewport.set_vscroll_policy(gtk::ScrollablePolicy::Natural);
        }
        Rc::new(Self {
            widget,
            flow,
            on_pick: Rc::new(on_pick),
        })
    }

    fn clear(&self) {
        while let Some(child) = self.flow.first_child() {
            self.flow.remove(&child);
        }
    }

    fn card(&self, cover: &gtk::Widget, caption: &gtk::Widget, offset: u64, class: &str, min: i32) {
        let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
        content.append(cover);
        content.append(caption);
        let button = gtk::Button::builder()
            .child(&content)
            .css_classes(["flat", "tile-button", class])
            .width_request(min)
            .build();
        let pick = self.on_pick.clone();
        button.connect_clicked(move |_| pick(offset));
        self.flow.append(&button);
    }

    /// Je Jahr eine Karte: das jüngste Foto mit Jahr und Zahl darüber.
    pub fn show_years(&self, catalog: &Catalog, groups: &[DateGroup]) {
        self.clear();
        let min = velum::photos("yearCard", 300.0) as i32;
        for (year, count, offset) in years(groups) {
            let cover = catalog
                .page(offset, 1)
                .ok()
                .and_then(|page| page.into_iter().next());
            let tile = thumb(
                cover.and_then(|p| p.thumb_path).as_deref(),
                16.0 / 10.0,
                min,
            );
            let caption = gtk::Box::builder()
                .orientation(gtk::Orientation::Horizontal)
                .halign(gtk::Align::Start)
                .valign(gtk::Align::End)
                .css_classes(["year-caption"])
                .build();
            caption.append(&label(&year, "year-title"));
            caption.append(&label(&photos_word(count), "caption-sub"));
            tile.add_overlay(&caption);
            tile.add_css_class("year-cover");
            self.card(
                tile.upcast_ref(),
                &gtk::Box::new(gtk::Orientation::Vertical, 0).upcast(),
                offset,
                "year-card",
                min,
            );
        }
    }

    /// Je Monat eine Karte: drei Fotos, darunter Monat und Zahl.
    pub fn show_months(&self, catalog: &Catalog, groups: &[DateGroup]) {
        self.clear();
        let min = velum::photos("monthCard", 240.0) as i32;
        for group in groups {
            let photos = catalog
                .page(group.offset, group.count.min(3))
                .unwrap_or_default();
            let grid = gtk::Grid::builder()
                .row_homogeneous(true)
                .column_homogeneous(true)
                .css_classes(["tri"])
                .build();
            for (index, photo) in photos.iter().enumerate() {
                let tile = thumb(photo.thumb_path.as_deref(), 1.0, 60);
                match index {
                    // Das erste groß links über zwei Reihen, wie im Lab.
                    0 => grid.attach(&tile, 0, 0, 2, 2),
                    1 => grid.attach(&tile, 2, 0, 1, 1),
                    _ => grid.attach(&tile, 2, 1, 1, 1),
                }
            }
            let cover = RatioBox::new(&grid, 3.0 / 2.0, min);
            cover.add_css_class("month-cover");
            let caption = gtk::Box::builder()
                .orientation(gtk::Orientation::Vertical)
                .css_classes(["month-caption"])
                .build();
            caption.append(&label(&month_title(&group.month), "month-title"));
            caption.append(&label(&photos_word(group.count), "card-sub"));
            self.card(
                cover.upcast_ref(),
                caption.upcast_ref(),
                group.offset,
                "month-card",
                min,
            );
        }
    }
}

/// `2024-07` → „July 2024".
pub fn month_title(month: &str) -> String {
    match month.split_once('-') {
        Some((year, number)) => match number.parse::<u32>() {
            Ok(number) => format!("{} {year}", month_name(number)),
            Err(_) => month.to_string(),
        },
        None => month.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn years_sum_their_months_and_start_at_the_newest() {
        let group = |month: &str, count, offset| DateGroup {
            month: month.into(),
            count,
            offset,
        };
        let groups = [
            group("2026-10", 5, 0),
            group("2026-03", 2, 5),
            group("2025-12", 4, 7),
        ];
        assert_eq!(
            years(&groups),
            [("2026".to_string(), 7, 0), ("2025".to_string(), 4, 7)]
        );
        assert_eq!(month_title("2024-07"), "July 2024");
    }
}
