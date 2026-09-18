//! Ein Bild, das sich zoomen und verschieben lässt — mit Maus, Touchpad und
//! Tastatur.
//!
//! Früher lag ein `GtkPicture` in einem `GtkScrolledWindow`, gezoomt über
//! `set_size_request`. Das ging nie sauber: Die Scrollgrenzen stehen erst nach
//! der nächsten Zuteilung fest, der Ausschnitt wurde also geraten und sprang.
//! Im eingepassten Zustand lag das Bild zudem mit Rändern im Scrollbereich, der
//! Punkt unter dem Zeiger wurde aber so gerechnet, als fülle es ihn aus.
//!
//! Hier hält das Widget Maßstab und Ausschnitt selbst und zeichnet das Bild
//! direkt. Die Rechnung steckt in [`View`], ohne GTK und damit testbar.

use std::cell::{Cell, RefCell};

use gtk::prelude::*;
use gtk::subclass::prelude::*;
use gtk::{gdk, glib, graphene, gsk};

/// Ein Zoomschritt: eine Rasterung des Mausrads oder ein Druck auf `+`.
pub const ZOOM_STEP: f64 = 1.25;

/// Weiter als achtfach pro Bildpunkt zu vergrößern zeigt nichts Neues mehr.
const MAX_PIXEL_SCALE: f64 = 8.0;

/// Auch ein kleines Bild — etwa die Vorschau, solange das Original lädt — soll
/// sich um mindestens so viel vergrößern lassen.
const MIN_ZOOM_RANGE: f64 = 4.0;

/// Touchpad-Scrollen mit Strg: so viele Pixel Weg verdoppeln etwa den Maßstab.
const SMOOTH_ZOOM_PIXELS: f64 = 70.0;

/// Wer über Änderungen am Ausschnitt Bescheid wissen will.
type ChangeCallback = Box<dyn Fn(&View)>;

/// Maßstab und Ausschnitt, unabhängig von GTK.
///
/// `zoom` ist relativ zur Einpassung (1.0 = das ganze Bild passt ins Fenster),
/// `center` ist der Bildpunkt in der Fenstermitte, normiert auf 0…1. Beides
/// bleibt gültig, wenn das Fenster seine Größe ändert oder die Vorschau durch
/// das Original ersetzt wird — die Pixelmaße stecken nirgends drin.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct View {
    pub image: (f64, f64),
    pub viewport: (f64, f64),
    pub zoom: f64,
    pub center: (f64, f64),
}

impl Default for View {
    fn default() -> Self {
        Self {
            image: (0.0, 0.0),
            viewport: (0.0, 0.0),
            zoom: 1.0,
            center: (0.5, 0.5),
        }
    }
}

impl View {
    fn is_empty(&self) -> bool {
        self.image.0 <= 0.0
            || self.image.1 <= 0.0
            || self.viewport.0 <= 0.0
            || self.viewport.1 <= 0.0
    }

    /// Bildpunkte pro Bildschirmpunkt, bei der das Bild gerade ganz passt.
    pub fn fit_scale(&self) -> f64 {
        if self.is_empty() {
            return 1.0;
        }
        (self.viewport.0 / self.image.0).min(self.viewport.1 / self.image.1)
    }

    /// Der tatsächliche Maßstab: 1.0 heißt ein Bildpunkt pro Bildschirmpunkt.
    pub fn pixel_scale(&self) -> f64 {
        self.fit_scale() * self.zoom
    }

    pub fn max_zoom(&self) -> f64 {
        (MAX_PIXEL_SCALE / self.fit_scale()).max(MIN_ZOOM_RANGE)
    }

    pub fn is_fitted(&self) -> bool {
        self.zoom <= 1.0 + 1e-6
    }

    /// Größe des gezeichneten Bildes auf dem Schirm.
    fn displayed(&self) -> (f64, f64) {
        let scale = self.pixel_scale();
        (self.image.0 * scale, self.image.1 * scale)
    }

    /// Linke obere Ecke des Bildes im Fenster.
    pub fn origin(&self) -> (f64, f64) {
        let (width, height) = self.displayed();
        (
            self.viewport.0 / 2.0 - self.center.0 * width,
            self.viewport.1 / 2.0 - self.center.1 * height,
        )
    }

    /// Das gezeichnete Rechteck: Ursprung und Größe.
    pub fn bounds(&self) -> (f64, f64, f64, f64) {
        let (x, y) = self.origin();
        let (width, height) = self.displayed();
        (x, y, width, height)
    }

    /// Der Bildpunkt (normiert) unter einer Fensterposition.
    pub fn image_point(&self, at: (f64, f64)) -> (f64, f64) {
        let (x, y, width, height) = self.bounds();
        ((at.0 - x) / width.max(1e-9), (at.1 - y) / height.max(1e-9))
    }

    /// Hält das Bild im Fenster: Eine Achse, auf der es ganz hineinpasst, wird
    /// mittig gestellt; sonst darf kein leerer Rand hereingeschoben werden.
    pub fn clamp(&mut self) {
        self.zoom = self.zoom.clamp(1.0, self.max_zoom());
        if self.is_empty() {
            self.center = (0.5, 0.5);
            return;
        }
        let (width, height) = self.displayed();
        self.center = (
            clamp_axis(self.center.0, self.viewport.0, width),
            clamp_axis(self.center.1, self.viewport.1, height),
        );
    }

    /// Setzt den Maßstab so, dass der Bildpunkt `point` danach an der
    /// Fensterposition `at` liegt.
    pub fn place(&mut self, zoom: f64, point: (f64, f64), at: (f64, f64)) {
        self.zoom = zoom.clamp(1.0, self.max_zoom());
        let (width, height) = self.displayed();
        self.center = (
            (self.viewport.0 / 2.0 - at.0) / width.max(1e-9) + point.0,
            (self.viewport.1 / 2.0 - at.1) / height.max(1e-9) + point.1,
        );
        self.clamp();
    }

    /// Zoomt so, dass der Punkt unter `anchor` stehen bleibt.
    pub fn zoom_to(&mut self, zoom: f64, anchor: (f64, f64)) {
        let point = self.image_point(anchor);
        self.place(zoom, point, anchor);
    }

    /// Verschiebt den Ausschnitt um so viele Bildschirmpunkte.
    pub fn pan(&mut self, dx: f64, dy: f64) {
        let (width, height) = self.displayed();
        self.center.0 -= dx / width.max(1e-9);
        self.center.1 -= dy / height.max(1e-9);
        self.clamp();
    }

    pub fn viewport_center(&self) -> (f64, f64) {
        (self.viewport.0 / 2.0, self.viewport.1 / 2.0)
    }
}

fn clamp_axis(center: f64, viewport: f64, displayed: f64) -> f64 {
    if displayed <= viewport {
        return 0.5;
    }
    let half = viewport / 2.0 / displayed;
    center.clamp(half, 1.0 - half)
}

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct ZoomView {
        pub texture: RefCell<Option<gdk::Texture>>,
        pub view: Cell<View>,
        /// Letzte Zeigerposition, damit Strg+Touchpad dort zoomt.
        pub pointer: Cell<Option<(f64, f64)>>,
        /// Ausschnitt zu Beginn eines Ziehens.
        pub drag_start: Cell<View>,
        /// Zustand zu Beginn einer Pinch-Geste: Maßstab und der Bildpunkt
        /// zwischen den Fingern.
        pub pinch_start: Cell<Option<(f64, (f64, f64))>>,
        pub on_change: RefCell<Option<ChangeCallback>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for ZoomView {
        const NAME: &'static str = "IcloudZoomView";
        type Type = super::ZoomView;
        type ParentType = gtk::Widget;
    }

    impl ObjectImpl for ZoomView {
        fn constructed(&self) {
            self.parent_constructed();
            let widget = self.obj();
            widget.set_hexpand(true);
            widget.set_vexpand(true);
            // Beim Heranzoomen ragt das Bild über den Rand hinaus.
            widget.set_overflow(gtk::Overflow::Hidden);
            widget.set_focusable(false);
            connect_input(&widget);
        }
    }

    impl WidgetImpl for ZoomView {
        fn measure(&self, _orientation: gtk::Orientation, _for_size: i32) -> (i32, i32, i32, i32) {
            // Das Bild bestimmt die Fenstergröße nicht — es passt sich an.
            (0, 0, -1, -1)
        }

        fn size_allocate(&self, width: i32, height: i32, _baseline: i32) {
            let mut view = self.view.get();
            view.viewport = (width as f64, height as f64);
            view.clamp();
            self.view.set(view);
            self.obj().notify_change();
        }

        fn snapshot(&self, snapshot: &gtk::Snapshot) {
            let Some(texture) = self.texture.borrow().clone() else {
                return;
            };
            let view = self.view.get();
            if view.is_empty() {
                return;
            }
            let (x, y, width, height) = view.bounds();
            let bounds = graphene::Rect::new(x as f32, y as f32, width as f32, height as f32);
            // Beim Verkleinern glättet Trilinear, statt Treppen zu zeigen; ab
            // einem Bildpunkt pro Bildschirmpunkt aufwärts sollen die Pixel
            // scharf sichtbar werden, wie in jedem Bildbetrachter.
            let filter = if view.pixel_scale() >= 2.0 {
                gsk::ScalingFilter::Nearest
            } else if view.pixel_scale() >= 1.0 {
                gsk::ScalingFilter::Linear
            } else {
                gsk::ScalingFilter::Trilinear
            };
            snapshot.append_scaled_texture(&texture, filter, &bounds);
        }
    }
}

glib::wrapper! {
    pub struct ZoomView(ObjectSubclass<imp::ZoomView>)
        @extends gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl Default for ZoomView {
    fn default() -> Self {
        glib::Object::new()
    }
}

impl ZoomView {
    pub fn new() -> Self {
        Self::default()
    }

    /// Setzt das Bild. Maßstab und Ausschnitt bleiben — wird die Vorschau
    /// durch das Original ersetzt, bleibt der Blick auf derselben Stelle.
    pub fn set_texture(&self, texture: Option<&gdk::Texture>) {
        let imp = self.imp();
        *imp.texture.borrow_mut() = texture.cloned();
        let mut view = imp.view.get();
        view.image = texture
            .map(|texture| (texture.width() as f64, texture.height() as f64))
            .unwrap_or((0.0, 0.0));
        view.clamp();
        imp.view.set(view);
        self.queue_draw();
        self.notify_change();
    }

    /// Zurück auf „eingepasst" — für ein neues Foto.
    pub fn reset(&self) {
        self.update(|view| {
            view.zoom = 1.0;
            view.center = (0.5, 0.5);
        });
    }

    pub fn view(&self) -> View {
        self.imp().view.get()
    }

    /// Meldet jede Änderung von Maßstab oder Ausschnitt.
    pub fn connect_changed(&self, callback: impl Fn(&View) + 'static) {
        *self.imp().on_change.borrow_mut() = Some(Box::new(callback));
    }

    /// Zoomt um einen Faktor, mit dem Punkt unter `anchor` als Festpunkt —
    /// ohne Anker um die Fenstermitte.
    pub fn zoom_by(&self, factor: f64, anchor: Option<(f64, f64)>) {
        self.update(|view| {
            let anchor = anchor.unwrap_or_else(|| view.viewport_center());
            view.zoom_to(view.zoom * factor, anchor);
        });
    }

    pub fn fit(&self) {
        self.update(|view| {
            view.zoom = 1.0;
            view.clamp();
        });
    }

    /// Ein Bildpunkt pro Bildschirmpunkt. Die Stelle in der Fenstermitte bzw.
    /// unter `anchor` bleibt, wo sie ist.
    pub fn actual_size(&self, anchor: Option<(f64, f64)>) {
        self.update(|view| {
            let anchor = anchor.unwrap_or_else(|| view.viewport_center());
            view.zoom_to(1.0 / view.fit_scale(), anchor);
        });
    }

    /// Doppelklick: eingepasst ↔ 1:1 an der geklickten Stelle. Ist das Bild
    /// kleiner als das Fenster, wäre 1:1 kleiner als eingepasst — dann wird
    /// stattdessen verdoppelt.
    pub fn toggle_zoom(&self, anchor: (f64, f64)) {
        self.update(|view| {
            if view.is_fitted() {
                let actual = 1.0 / view.fit_scale();
                let target = if actual > 1.5 { actual } else { 2.0 };
                view.zoom_to(target, anchor);
            } else {
                view.zoom = 1.0;
                view.clamp();
            }
        });
    }

    fn update(&self, change: impl FnOnce(&mut View)) {
        let imp = self.imp();
        let mut view = imp.view.get();
        let before = view;
        change(&mut view);
        if view == before {
            return;
        }
        imp.view.set(view);
        self.queue_draw();
        self.notify_change();
    }

    fn notify_change(&self) {
        let view = self.imp().view.get();
        self.set_cursor_from_name(if view.is_fitted() { None } else { Some("grab") });
        if let Some(callback) = self.imp().on_change.borrow().as_ref() {
            callback(&view);
        }
    }
}

fn connect_input(widget: &ZoomView) {
    let motion = gtk::EventControllerMotion::new();
    motion.connect_motion({
        let widget = widget.downgrade();
        move |_, x, y| {
            if let Some(widget) = widget.upgrade() {
                widget.imp().pointer.set(Some((x, y)));
            }
        }
    });
    motion.connect_leave({
        let widget = widget.downgrade();
        move |_| {
            if let Some(widget) = widget.upgrade() {
                widget.imp().pointer.set(None);
            }
        }
    });
    widget.add_controller(motion);

    // Mausrad zoomt, Rasterung für Rasterung. Das Touchpad liefert dagegen
    // feine Pixel-Schritte, Dutzende pro Wischer — früher zoomte jeder davon
    // um einen vollen Schritt. Zwei Finger verschieben deshalb den Ausschnitt;
    // gezoomt wird mit Strg oder mit der Pinch-Geste.
    let scroll = gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::BOTH_AXES);
    scroll.connect_scroll({
        let widget = widget.downgrade();
        move |controller, dx, dy| {
            let Some(widget) = widget.upgrade() else {
                return glib::Propagation::Proceed;
            };
            let ctrl = controller
                .current_event_state()
                .contains(gdk::ModifierType::CONTROL_MASK);
            let anchor = widget.imp().pointer.get();
            match controller.unit() {
                gdk::ScrollUnit::Wheel => {
                    if dy == 0.0 {
                        return glib::Propagation::Proceed;
                    }
                    // Hochauflösende Räder liefern Bruchteile einer Rasterung.
                    widget.zoom_by(ZOOM_STEP.powf(-dy), anchor);
                }
                _ if ctrl => {
                    widget.zoom_by((-dy / SMOOTH_ZOOM_PIXELS).exp2(), anchor);
                }
                _ => {
                    if widget.view().is_fitted() {
                        return glib::Propagation::Proceed;
                    }
                    widget.update(|view| view.pan(-dx, -dy));
                }
            }
            glib::Propagation::Stop
        }
    });
    widget.add_controller(scroll);

    // Pinch auf dem Touchpad: Maßstab und Lage folgen den Fingern. Festpunkt
    // ist der Bildpunkt, der zu Beginn zwischen ihnen lag.
    let pinch = gtk::GestureZoom::new();
    pinch.connect_begin({
        let widget = widget.downgrade();
        move |gesture, _| {
            let Some(widget) = widget.upgrade() else {
                return;
            };
            let view = widget.view();
            let at = gesture
                .bounding_box_center()
                .unwrap_or_else(|| view.viewport_center());
            widget
                .imp()
                .pinch_start
                .set(Some((view.zoom, view.image_point(at))));
        }
    });
    pinch.connect_scale_changed({
        let widget = widget.downgrade();
        move |gesture, scale| {
            let Some(widget) = widget.upgrade() else {
                return;
            };
            let Some((zoom, point)) = widget.imp().pinch_start.get() else {
                return;
            };
            let at = gesture
                .bounding_box_center()
                .unwrap_or_else(|| widget.view().viewport_center());
            widget.update(|view| view.place(zoom * scale, point, at));
        }
    });
    pinch.connect_end({
        let widget = widget.downgrade();
        move |_, _| {
            if let Some(widget) = widget.upgrade() {
                widget.imp().pinch_start.set(None);
            }
        }
    });
    widget.add_controller(pinch);

    // Ziehen verschiebt den Ausschnitt, sobald hineingezoomt ist.
    let drag = gtk::GestureDrag::new();
    drag.set_button(gdk::BUTTON_PRIMARY);
    drag.connect_drag_begin({
        let widget = widget.downgrade();
        move |_, _, _| {
            let Some(widget) = widget.upgrade() else {
                return;
            };
            widget.imp().drag_start.set(widget.view());
            if !widget.view().is_fitted() {
                widget.set_cursor_from_name(Some("grabbing"));
            }
        }
    });
    drag.connect_drag_update({
        let widget = widget.downgrade();
        move |_, dx, dy| {
            let Some(widget) = widget.upgrade() else {
                return;
            };
            let start = widget.imp().drag_start.get();
            if start.is_fitted() {
                return;
            }
            widget.update(|view| {
                view.center = start.center;
                view.pan(dx, dy);
            });
        }
    });
    drag.connect_drag_end({
        let widget = widget.downgrade();
        move |_, _, _| {
            if let Some(widget) = widget.upgrade() {
                widget.notify_change();
            }
        }
    });
    widget.add_controller(drag);

    let click = gtk::GestureClick::new();
    click.set_button(gdk::BUTTON_PRIMARY);
    click.connect_pressed({
        let widget = widget.downgrade();
        move |_, clicks, x, y| {
            if clicks == 2
                && let Some(widget) = widget.upgrade()
            {
                widget.toggle_zoom((x, y));
            }
        }
    });
    widget.add_controller(click);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 4000×3000 in einem 1000×1000-Fenster: eingepasst mit 0,25.
    fn view() -> View {
        let mut view = View {
            image: (4000.0, 3000.0),
            viewport: (1000.0, 1000.0),
            ..View::default()
        };
        view.clamp();
        view
    }

    fn close(a: (f64, f64), b: (f64, f64)) -> bool {
        (a.0 - b.0).abs() < 1e-6 && (a.1 - b.1).abs() < 1e-6
    }

    #[test]
    fn fitted_image_is_centred_with_letterbox() {
        let view = view();
        assert!((view.fit_scale() - 0.25).abs() < 1e-9);
        let (x, y, width, height) = view.bounds();
        assert!(close((x, y), (0.0, 125.0)));
        assert!(close((width, height), (1000.0, 750.0)));
    }

    #[test]
    fn zoom_keeps_the_point_under_the_pointer() {
        let mut view = view();
        let anchor = (700.0, 400.0);
        let before = view.image_point(anchor);
        view.zoom_to(3.0, anchor);
        assert!(close(view.image_point(anchor), before));
    }

    #[test]
    fn zoom_from_the_letterbox_does_not_jump() {
        // Genau der alte Fehler: im Rand oberhalb des Bildes gezoomt.
        let mut view = view();
        view.zoom_to(4.0, (500.0, 50.0));
        let (_, y, _, _) = view.bounds();
        // Oberkante bündig, kein leerer Rand hereingezogen.
        assert!(y.abs() < 1e-6);
    }

    #[test]
    fn repeated_zoom_in_and_out_returns_to_the_start() {
        let mut view = view();
        let start = view;
        let anchor = (300.0, 600.0);
        for _ in 0..5 {
            view.zoom_to(view.zoom * ZOOM_STEP, anchor);
        }
        for _ in 0..5 {
            view.zoom_to(view.zoom / ZOOM_STEP, anchor);
        }
        assert!((view.zoom - 1.0).abs() < 1e-9);
        assert!(close(view.center, start.center));
    }

    #[test]
    fn pan_stops_at_the_edge() {
        let mut view = view();
        view.zoom_to(2.0, view.viewport_center());
        view.pan(100_000.0, 0.0);
        let (x, _, _, _) = view.bounds();
        assert!(
            x.abs() < 1e-6,
            "linke Kante muss am Fensterrand stehen, x = {x}"
        );
    }

    #[test]
    fn zoom_is_limited() {
        let mut view = view();
        view.zoom_to(1e9, view.viewport_center());
        assert!((view.pixel_scale() - MAX_PIXEL_SCALE).abs() < 1e-9);
        view.zoom_to(1e-9, view.viewport_center());
        assert!(view.is_fitted());
    }

    #[test]
    fn view_survives_a_resize_and_a_sharper_texture() {
        let mut view = view();
        view.zoom_to(3.0, (200.0, 300.0));
        let center = view.center;
        // Das Original ersetzt die Vorschau: gleiches Seitenverhältnis,
        // zehnfache Auflösung.
        view.image = (40_000.0, 30_000.0);
        view.clamp();
        assert!(close(view.center, center));
        assert!((view.zoom - 3.0).abs() < 1e-9);
    }
}
