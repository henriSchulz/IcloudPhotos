//! „Map" — Fotos mit Aufnahmeort auf einer Karte, wie in Photos.
//!
//! Die Karte kommt von libshumate mit OpenStreetMap-Kacheln. Auf ihr liegt je
//! Gruppe nahe beieinander aufgenommener Fotos eine Markierung: das neueste
//! Foto als abgerundete Vorschau, darauf die Anzahl. Ein Klick zeigt genau
//! diese Fotos im gewohnten Raster. Wie gruppiert wird, steht in
//! [`crate::cluster`].
//!
//! Drei Dinge halten die Karte auch bei zehntausend Fotos flüssig:
//!
//! - **Gruppen je Zoomstufe, einmal gerechnet.** Beim Verschieben ändert sich
//!   an ihnen nichts, sie werden pro ganzzahliger Zoomstufe zwischengespeichert.
//! - **Nur sichtbare Markierungen sind Widgets.** Was außerhalb des
//!   Ausschnitts (plus Rand) liegt, wird gar nicht erst gebaut; beim
//!   Verschieben kommen nur die neu sichtbaren dazu, bereits gebaute bleiben.
//! - **Vorschauen werden im Hilfsthread dekodiert** und dabei gleich auf
//!   Markierungsgröße verkleinert — hunderte volle Vorschauen im UI-Thread
//!   ließen die Karte bei jedem Zoomschritt stehen.
//!
//! Die Karte wird erst beim ersten Öffnen gebaut. Jede Kachel ist eine Anfrage
//! an die Server von OpenStreetMap, und die verrät, welche Gegend man sich
//! ansieht — wer die Karte nie öffnet, schickt nichts weg.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use gtk::prelude::*;
use gtk::{gdk, gio, glib};
use icloud_photos_cache::LocatedPhoto;

use crate::cluster::{self, Bounds, Cluster};
use crate::sync_manager::thousands;

/// Kantenlänge einer Rasterzelle in Pixeln. Etwas größer als eine Markierung,
/// damit sich Nachbarn nicht überdecken.
const CELL_PX: f64 = 80.0;

/// Kantenlänge der Vorschau auf einer Markierung.
const MARKER_SIZE: i32 = 56;

/// Auf diese Größe wird eine Vorschau fürs Dekodieren verkleinert — doppelt
/// so groß wie die Markierung, damit sie auch bei Skalierung 2 scharf bleibt.
const DECODE_SIZE: i32 = MARKER_SIZE * 2 + 48;

/// Wie lange nach der letzten Bewegung gewartet wird, bevor die Markierungen
/// nachgeführt werden. Ein Zoom mit dem Mausrad sind Dutzende Einzelschritte.
const SETTLE: Duration = Duration::from_millis(120);

/// Mehr Markierungen baut die Karte nie auf einmal. Bei `CELL_PX` passen auf
/// einen großen Bildschirm ohnehin nur ein paar Hundert.
const MAX_MARKERS: usize = 600;

/// So viele verkleinerte Vorschauen bleiben im Speicher (je rund 100 KB).
const COVER_CACHE: usize = 1500;

/// OpenStreetMap verlangt einen User-Agent, der die Anwendung erkennen lässt;
/// die Vorgabe von libshumate nennt nur die Bibliothek.
const USER_AGENT: &str = concat!(
    "icloud-photos/",
    env!("CARGO_PKG_VERSION"),
    " (personal iCloud Photos client for Linux; libshumate)"
);

const MAP_CSS: &str = "
.map-cluster {
    padding: 0;
    min-width: 0;
    min-height: 0;
    background: none;
    box-shadow: none;
}
.map-cluster-photo {
    border-radius: 12px;
    border: 2px solid white;
    background-color: alpha(black, 0.25);
    box-shadow: 0 1px 4px alpha(black, 0.5);
}
.map-cluster:hover .map-cluster-photo,
.map-cluster:focus-visible .map-cluster-photo {
    border-color: @accent_bg_color;
}
/* Anzahl oben rechts, auf jedem Bild und jeder Karte lesbar. */
.map-cluster-count {
    background-color: @accent_bg_color;
    color: @accent_fg_color;
    border-radius: 99px;
    padding: 1px 7px;
    font-size: smaller;
    font-weight: bold;
}
";

/// Was ein Klick auf eine Markierung auslöst.
#[derive(Debug, Clone)]
pub struct Pick {
    /// Alle Fotos der Gruppe.
    pub ids: Arc<[String]>,
    /// Das neueste Foto der Gruppe mit seiner Stelle — daran hängt der
    /// Ortsname im Titel.
    pub anchor: (String, f64, f64),
}

type PickCallback = Rc<dyn Fn(Pick)>;

/// Die eigentliche Karte — erst beim ersten Öffnen gebaut.
struct Built {
    simple: shumate::SimpleMap,
    map: shumate::Map,
    viewport: shumate::Viewport,
    layer: shumate::MarkerLayer,
}

pub struct MapView {
    /// „map" oder „empty", falls kein Foto einen Ort hat.
    pub widget: gtk::Stack,
    built: RefCell<Option<Built>>,
    photos: RefCell<Rc<Vec<LocatedPhoto>>>,
    /// Die Fotos in Web-Mercator, gleiche Reihenfolge wie `photos`.
    points: RefCell<Rc<Vec<(f64, f64)>>>,
    /// Gruppen je Zoomstufe. Wird mit jedem neuen Fotostand verworfen.
    clusters: RefCell<HashMap<u32, Rc<Vec<Cluster>>>>,
    /// Die Zoomstufe, zu der die gebauten Markierungen gehören.
    marker_zoom: Cell<Option<u32>>,
    /// Gebaute Markierungen, nach Index ihrer Gruppe.
    markers: RefCell<HashMap<usize, shumate::Marker>>,
    fitted: Cell<bool>,
    pending: RefCell<Option<glib::SourceId>>,
    on_pick: RefCell<Option<PickCallback>>,
}

impl MapView {
    pub fn new() -> Rc<Self> {
        let widget = gtk::Stack::new();
        widget.add_named(
            &adw::StatusPage::builder()
                .icon_name("mark-location-symbolic")
                .title("No Places Yet")
                .description("Photos taken with location information appear here.")
                .build(),
            Some("empty"),
        );
        Rc::new(Self {
            widget,
            built: RefCell::new(None),
            photos: RefCell::new(Rc::new(Vec::new())),
            points: RefCell::new(Rc::new(Vec::new())),
            clusters: RefCell::new(HashMap::new()),
            marker_zoom: Cell::new(None),
            markers: RefCell::new(HashMap::new()),
            fitted: Cell::new(false),
            pending: RefCell::new(None),
            on_pick: RefCell::new(None),
        })
    }

    pub fn connect_pick(&self, callback: impl Fn(Pick) + 'static) {
        *self.on_pick.borrow_mut() = Some(Rc::new(callback));
    }

    /// Zeigt die Karte mit diesem Fotostand.
    ///
    /// Beim ersten Mal wird die Karte gebaut und auf die Fotos ausgerichtet.
    /// Ein unveränderter Stand lässt Gruppen und Markierungen stehen — die
    /// Karte kommt so zurück, wie man sie verlassen hat.
    pub fn show(self: &Rc<Self>, photos: Vec<LocatedPhoto>) {
        if photos.is_empty() {
            self.widget.set_visible_child_name("empty");
            *self.photos.borrow_mut() = Rc::new(photos);
            return;
        }
        if **self.photos.borrow() != photos {
            let points: Vec<(f64, f64)> = photos
                .iter()
                .map(|photo| cluster::project(photo.lat, photo.lon))
                .collect();
            *self.photos.borrow_mut() = Rc::new(photos);
            *self.points.borrow_mut() = Rc::new(points);
            self.clusters.borrow_mut().clear();
            self.marker_zoom.set(None);
        }
        self.build();
        self.widget.set_visible_child_name("map");
        // Ausgerichtet wird im ersten `refresh` — erst dann ist die Karte
        // zugeteilt und kennt ihre Größe.
        self.schedule();
    }

    fn build(self: &Rc<Self>) {
        if self.built.borrow().is_some() {
            return;
        }
        shumate::functions::set_user_agent(Some(USER_AGENT));
        install_css();

        let registry = shumate::MapSourceRegistry::with_defaults();
        let simple = shumate::SimpleMap::new();
        simple.set_hexpand(true);
        simple.set_vexpand(true);
        // Im Prüfstand (`ICLOUD_PHOTOS_OFFLINE`) ohne Kacheln: keine Anfrage
        // an den Kachelserver, nur die Markierungen auf leerem Grund.
        match registry.by_id(shumate::MAP_SOURCE_OSM_MAPNIK) {
            Some(_) if crate::offline() => {
                // Eine Quelle ohne Netz, damit die Karte ihre Projektion hat
                // und die Markierungen stehen; die Kacheln bleiben leer.
                let blank =
                    shumate::RasterRenderer::from_url("file:///nonexistent/{z}/{x}/{y}.png");
                simple.set_map_source(Some(&blank));
            }
            Some(source) => simple.set_map_source(Some(&source)),
            None => tracing::error!("Kartenquelle OSM Mapnik fehlt in libshumate"),
        }
        let (Some(map), Some(viewport)) = (simple.map(), simple.viewport()) else {
            tracing::error!("libshumate liefert keine Karte");
            return;
        };
        // Keine Auswahl: ein Klick öffnet die Fotos, er markiert nichts.
        let layer = shumate::MarkerLayer::new_full(&viewport, gtk::SelectionMode::None);
        simple.add_overlay_layer(&layer);

        viewport.connect_changed({
            let weak = Rc::downgrade(self);
            move |_| {
                if let Some(view) = weak.upgrade() {
                    view.schedule();
                }
            }
        });

        self.widget.add_named(&simple, Some("map"));
        *self.built.borrow_mut() = Some(Built {
            simple,
            map,
            viewport,
            layer,
        });
    }

    /// Führt die Markierungen nach, sobald die Karte zur Ruhe gekommen ist.
    fn schedule(self: &Rc<Self>) {
        if let Some(source) = self.pending.borrow_mut().take() {
            source.remove();
        }
        let weak = Rc::downgrade(self);
        let source = glib::timeout_add_local_once(SETTLE, move || {
            if let Some(view) = weak.upgrade() {
                // Die Quelle ist gerade gelaufen und darf nicht mehr entfernt
                // werden.
                view.pending.borrow_mut().take();
                view.refresh();
            }
        });
        *self.pending.borrow_mut() = Some(source);
    }

    /// Die Gruppen einer Zoomstufe, gerechnet beim ersten Bedarf.
    fn clusters_at(&self, zoom: u32) -> Rc<Vec<Cluster>> {
        if let Some(clusters) = self.clusters.borrow().get(&zoom) {
            return clusters.clone();
        }
        let start = std::time::Instant::now();
        let points = self.points.borrow().clone();
        let clusters = Rc::new(cluster::cluster(&points, zoom, CELL_PX));
        tracing::debug!(
            zoom,
            fotos = points.len(),
            gruppen = clusters.len(),
            dauer = ?start.elapsed(),
            "Kartengruppen gerechnet"
        );
        self.clusters.borrow_mut().insert(zoom, clusters.clone());
        clusters
    }

    /// Baut die Markierungen für den sichtbaren Ausschnitt.
    fn refresh(self: &Rc<Self>) {
        let Some((simple, map, viewport, layer)) = self.built.borrow().as_ref().map(|b| {
            (
                b.simple.clone(),
                b.map.clone(),
                b.viewport.clone(),
                b.layer.clone(),
            )
        }) else {
            return;
        };
        if !simple.is_mapped() || self.points.borrow().is_empty() {
            return;
        }
        let (width, height) = (f64::from(map.width()), f64::from(map.height()));
        if width <= 0.0 || height <= 0.0 {
            return;
        }

        // Beim ersten Öffnen: so ausrichten, dass die Fotos hineinpassen.
        // Danach bleibt die Karte, wo man sie verlassen hat.
        if !self.fitted.replace(true) {
            let points = self.points.borrow().clone();
            if let Some(fit) = cluster::fit(&points, width, height) {
                viewport.set_zoom_level(fit.zoom);
                map.center_on(fit.lat, fit.lon);
            }
        }

        let zoom = viewport.zoom_level().round().max(0.0) as u32;
        let clusters = self.clusters_at(zoom);
        if self.marker_zoom.get() != Some(zoom) {
            layer.remove_all();
            self.markers.borrow_mut().clear();
            self.marker_zoom.set(Some(zoom));
        }

        // Ein halber Bildschirm Rand in jede Richtung: beim Verschieben sind
        // die nächsten Markierungen schon da. Alle vier Ecken, weil die Karte
        // gedreht sein kann.
        let corners = [
            (-width / 2.0, -height / 2.0),
            (width * 1.5, -height / 2.0),
            (-width / 2.0, height * 1.5),
            (width * 1.5, height * 1.5),
        ];
        let Some(bounds) = Bounds::around(corners.iter().map(|&(x, y)| {
            let (lat, lon) = viewport.widget_coords_to_location(&map, x, y);
            cluster::project(lat, lon)
        })) else {
            return;
        };

        let mut visible: Vec<usize> = clusters
            .iter()
            .enumerate()
            .filter(|(_, cluster)| bounds.contains(cluster.x, cluster.y))
            .map(|(index, _)| index)
            .collect();
        if visible.len() > MAX_MARKERS {
            visible.sort_by_key(|&index| std::cmp::Reverse(clusters[index].members.len()));
            visible.truncate(MAX_MARKERS);
        }
        let wanted: HashSet<usize> = visible.iter().copied().collect();

        let mut markers = self.markers.borrow_mut();
        markers.retain(|index, marker| {
            let keep = wanted.contains(index);
            if !keep {
                layer.remove_marker(marker);
            }
            keep
        });
        // Kleine zuerst, damit große Gruppen obenauf liegen.
        visible.sort_by_key(|&index| clusters[index].members.len());
        let photos = self.photos.borrow().clone();
        for index in visible {
            if markers.contains_key(&index) {
                continue;
            }
            let marker = self.marker(&clusters[index], &photos);
            layer.add_marker(&marker);
            markers.insert(index, marker);
        }
    }

    /// Eine Markierung: Vorschau des neuesten Fotos und die Anzahl.
    fn marker(self: &Rc<Self>, cluster: &Cluster, photos: &[LocatedPhoto]) -> shumate::Marker {
        let count = cluster.members.len();
        let picture = gtk::Picture::builder()
            .content_fit(gtk::ContentFit::Cover)
            .build();
        // Fest in Markierungsgröße: ein Picture meldet sonst die Größe seiner
        // Textur als Wunsch, und die Markierung wüchse auf 160 px.
        let frame = crate::layout::RatioBox::fixed(&picture, 1.0, MARKER_SIZE);
        frame.add_css_class("map-cluster-photo");
        // Platz für die Anzahl, die über die Ecke hinausragt.
        frame.set_margin_top(8);
        frame.set_margin_end(8);
        // Titelbild ist das neueste Foto mit Vorschau — wie bei den Alben.
        if let Some(path) = cluster
            .members
            .iter()
            .find_map(|&i| photos[i].thumb_path.as_deref())
        {
            load_cover(&picture, path);
        }

        let overlay = gtk::Overlay::new();
        overlay.set_child(Some(&frame));
        if count > 1 {
            let badge = gtk::Label::builder()
                .label(thousands(count as u64))
                .css_classes(["map-cluster-count", "numeric"])
                .halign(gtk::Align::End)
                .valign(gtk::Align::Start)
                .build();
            badge.set_can_target(false);
            overlay.add_overlay(&badge);
        }

        let label = if count == 1 {
            "1 Photo".to_string()
        } else {
            format!("{} Photos", thousands(count as u64))
        };
        let button = gtk::Button::builder()
            .child(&overlay)
            .css_classes(["map-cluster"])
            .tooltip_text(&label)
            .build();

        let first = &photos[cluster.members[0]];
        let pick = Pick {
            ids: cluster
                .members
                .iter()
                .map(|&i| photos[i].id.clone())
                .collect(),
            anchor: (first.id.clone(), first.lat, first.lon),
        };
        button.connect_clicked({
            let weak = Rc::downgrade(self);
            move |_| {
                let callback = weak
                    .upgrade()
                    .and_then(|view| view.on_pick.borrow().clone());
                if let Some(callback) = callback {
                    callback(pick.clone());
                }
            }
        });

        let (lat, lon) = cluster.center();
        shumate::Marker::builder()
            .latitude(lat)
            .longitude(lon)
            .child(&button)
            .build()
    }
}

fn install_css() {
    let provider = gtk::CssProvider::new();
    provider.load_from_string(MAP_CSS);
    if let Some(display) = gdk::Display::default() {
        gtk::style_context_add_provider_for_display(
            &display,
            &provider,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    }
}

thread_local! {
    /// Bereits verkleinerte Vorschauen. Beim Zoomen wechseln die Titelbilder
    /// ständig zwischen denselben Fotos — ein zweites Dekodieren wäre Verschwendung.
    static COVERS: RefCell<HashMap<PathBuf, gdk::Texture>> = RefCell::default();
}

/// Setzt die Vorschau einer Markierung. Dekodiert wird im Hilfsthread und
/// gleich verkleinert: eine volle Vorschau hat 360×480 Pixel, auf der
/// Markierung sind es 56×56.
fn load_cover(picture: &gtk::Picture, path: &Path) {
    if let Some(texture) = COVERS.with_borrow(|covers| covers.get(path).cloned()) {
        picture.set_paintable(Some(&texture));
        return;
    }
    let picture = picture.downgrade();
    let path = path.to_path_buf();
    glib::spawn_future_local(async move {
        let source = path.clone();
        let decoded = gio::spawn_blocking(move || {
            gtk::gdk_pixbuf::Pixbuf::from_file_at_scale(&source, DECODE_SIZE, DECODE_SIZE, true)
                .map(|pixbuf| {
                    let pixbuf = pixbuf.apply_embedded_orientation().unwrap_or(pixbuf);
                    // Ab GTK 4.20 veraltet; der Ersatz verlangt rohe Pixeldaten.
                    #[allow(deprecated)]
                    gdk::Texture::for_pixbuf(&pixbuf)
                })
        })
        .await;
        let texture = match decoded {
            Ok(Ok(texture)) => texture,
            Ok(Err(error)) => {
                tracing::debug!(path = %path.display(), %error, "Vorschau fuer die Karte nicht ladbar");
                return;
            }
            Err(_) => return,
        };
        COVERS.with_borrow_mut(|covers| {
            // Grob, aber ausreichend: wer so viele Orte angesehen hat, verliert
            // nur die Abkürzung, nicht die Vorschauen selbst.
            if covers.len() >= COVER_CACHE {
                covers.clear();
            }
            covers.insert(path, texture.clone());
        });
        if let Some(picture) = picture.upgrade() {
            picture.set_paintable(Some(&texture));
        }
    });
}
