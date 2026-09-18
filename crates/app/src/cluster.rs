//! Gruppen für die Karte: nahe beieinander liegende Fotos werden ein Punkt.
//!
//! Tausende Fotos als je eigene Markierung wären tausende Widgets — die Karte
//! stünde, und zu sehen wäre nur ein Teppich aus Vorschauen. Wie in Photos wird
//! deshalb gruppiert, und zwar in Bildschirmgröße: Die Welt wird je Zoomstufe in
//! ein Raster aus Zellen von `cell_px` Pixeln geteilt, jede besetzte Zelle ist
//! eine Gruppe. Das kostet einen Durchgang über alle Punkte und hängt nicht von
//! ihrer Zahl im Quadrat ab. Weil das Raster an der Welt hängt und nicht am
//! Ausschnitt, bleiben die Gruppen beim Verschieben dieselben — neu gerechnet
//! wird nur, wenn sich die Zoomstufe ändert.
//!
//! Ein reines Raster zerschneidet einen Ort, der zufällig auf einer Zellgrenze
//! liegt, in zwei Markierungen direkt nebeneinander. Ein zweiter Schritt legt
//! deshalb Nachbargruppen zusammen, deren Mittelpunkte näher als eine Zelle
//! beieinander liegen.
//!
//! Alles hier ist GTK-frei, damit die Rechnung getestet werden kann.

use std::collections::HashMap;

/// Kantenlänge einer Kachel der Kartenquelle in Pixeln (OSM: 256).
pub const TILE_SIZE: f64 = 256.0;

/// Web-Mercator reicht nicht bis an die Pole; jenseits davon wird geklemmt.
const MAX_LATITUDE: f64 = 85.051_128_78;

/// Weiter hinein zoomt die Startansicht nicht, auch bei einem einzigen Ort —
/// sonst sähe man zuerst ein einzelnes Haus statt der Gegend.
const MAX_FIT_ZOOM: f64 = 15.0;
const MIN_FIT_ZOOM: f64 = 1.0;

/// Anteil der Fotos, der an jedem Rand der Startansicht wegfallen darf. Ein
/// einziger Urlaub am anderen Ende der Welt soll die Ansicht nicht auf
/// Weltkartengröße ziehen.
const FIT_TRIM: f64 = 0.02;

/// Breite und Länge in Web-Mercator, normiert auf 0…1 in beiden Achsen
/// (x nach Osten, y nach Süden — wie auf dem Bildschirm).
pub fn project(lat: f64, lon: f64) -> (f64, f64) {
    let lat = lat.clamp(-MAX_LATITUDE, MAX_LATITUDE).to_radians();
    let x = (lon + 180.0) / 360.0;
    let y = (1.0 - (lat.tan() + 1.0 / lat.cos()).ln() / std::f64::consts::PI) / 2.0;
    (x.clamp(0.0, 1.0), y.clamp(0.0, 1.0))
}

/// Umkehrung von [`project`].
pub fn unproject(x: f64, y: f64) -> (f64, f64) {
    let lon = x * 360.0 - 180.0;
    let lat = (std::f64::consts::PI * (1.0 - 2.0 * y))
        .sinh()
        .atan()
        .to_degrees();
    (lat, lon)
}

/// Eine Gruppe von Fotos, die auf der Karte ein Punkt ist.
#[derive(Debug, Clone, PartialEq)]
pub struct Cluster {
    /// Schwerpunkt der Mitglieder, normiert wie bei [`project`].
    pub x: f64,
    pub y: f64,
    /// Indizes in die Eingabe, aufsteigend. Ist die Eingabe neueste zuerst
    /// sortiert, steht das neueste Foto also vorn.
    pub members: Vec<usize>,
}

impl Cluster {
    /// Schwerpunkt als Breite und Länge.
    pub fn center(&self) -> (f64, f64) {
        unproject(self.x, self.y)
    }
}

/// Zwischenstand einer Gruppe während der Rechnung.
struct Group {
    cell: (i64, i64),
    sum_x: f64,
    sum_y: f64,
    members: Vec<usize>,
    alive: bool,
}

impl Group {
    fn centroid(&self) -> (f64, f64) {
        let n = self.members.len() as f64;
        (self.sum_x / n, self.sum_y / n)
    }
}

/// Wie viele Zellen die Welt bei dieser Zoomstufe breit ist.
fn cells_across(zoom: u32, cell_px: f64) -> f64 {
    TILE_SIZE * 2f64.powi(zoom.min(30) as i32) / cell_px
}

/// Gruppiert Punkte (normiert wie bei [`project`]) für eine Zoomstufe.
///
/// Jeder Punkt landet in genau einer Gruppe. Die Reihenfolge der Ergebnisse
/// folgt dem ersten Mitglied, ist also bei neueste-zuerst-Eingabe ebenfalls
/// neueste zuerst — und vor allem stabil: dieselbe Eingabe ergibt dieselben
/// Gruppen, sonst flackerten die Markierungen bei jedem Neuzeichnen.
pub fn cluster(points: &[(f64, f64)], zoom: u32, cell_px: f64) -> Vec<Cluster> {
    let cells = cells_across(zoom, cell_px);
    let mut index: HashMap<(i64, i64), usize> = HashMap::new();
    let mut groups: Vec<Group> = Vec::new();

    for (i, &(x, y)) in points.iter().enumerate() {
        let cell = ((x * cells).floor() as i64, (y * cells).floor() as i64);
        let slot = *index.entry(cell).or_insert_with(|| {
            groups.push(Group {
                cell,
                sum_x: 0.0,
                sum_y: 0.0,
                members: Vec::new(),
                alive: true,
            });
            groups.len() - 1
        });
        let group = &mut groups[slot];
        group.sum_x += x;
        group.sum_y += y;
        group.members.push(i);
    }

    // Die großen Gruppen schlucken zuerst: ein Ort mit hundert Fotos soll
    // nicht an einem Nachbarn mit dreien hängen, der zufällig früher kam.
    let mut order: Vec<usize> = (0..groups.len()).collect();
    order.sort_by(|&a, &b| {
        groups[b]
            .members
            .len()
            .cmp(&groups[a].members.len())
            .then(groups[a].members[0].cmp(&groups[b].members[0]))
    });
    for &slot in &order {
        if !groups[slot].alive {
            continue;
        }
        let (cx, cy) = groups[slot].cell;
        for dx in -1..=1 {
            for dy in -1..=1 {
                if dx == 0 && dy == 0 {
                    continue;
                }
                let Some(&other) = index.get(&(cx + dx, cy + dy)) else {
                    continue;
                };
                if !groups[other].alive {
                    continue;
                }
                let (ax, ay) = groups[slot].centroid();
                let (bx, by) = groups[other].centroid();
                let distance = ((ax - bx) * cells).hypot((ay - by) * cells);
                if distance < 1.0 {
                    let absorbed = std::mem::take(&mut groups[other].members);
                    let (sum_x, sum_y) = (groups[other].sum_x, groups[other].sum_y);
                    groups[other].alive = false;
                    let group = &mut groups[slot];
                    group.sum_x += sum_x;
                    group.sum_y += sum_y;
                    group.members.extend(absorbed);
                }
            }
        }
    }

    let mut clusters: Vec<Cluster> = groups
        .into_iter()
        .filter(|group| group.alive)
        .map(|mut group| {
            group.members.sort_unstable();
            let (x, y) = group.centroid();
            Cluster {
                x,
                y,
                members: group.members,
            }
        })
        .collect();
    clusters.sort_by_key(|cluster| cluster.members[0]);
    clusters
}

/// Ein Ausschnitt der Welt, normiert wie bei [`project`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Bounds {
    pub min_x: f64,
    pub min_y: f64,
    pub max_x: f64,
    pub max_y: f64,
}

impl Bounds {
    /// Der kleinste Ausschnitt, der alle Punkte enthält.
    pub fn around(points: impl IntoIterator<Item = (f64, f64)>) -> Option<Self> {
        points.into_iter().fold(None, |bounds, (x, y)| {
            Some(match bounds {
                None => Self {
                    min_x: x,
                    min_y: y,
                    max_x: x,
                    max_y: y,
                },
                Some(b) => Self {
                    min_x: b.min_x.min(x),
                    min_y: b.min_y.min(y),
                    max_x: b.max_x.max(x),
                    max_y: b.max_y.max(y),
                },
            })
        })
    }

    pub fn contains(&self, x: f64, y: f64) -> bool {
        (self.min_x..=self.max_x).contains(&x) && (self.min_y..=self.max_y).contains(&y)
    }
}

/// Wohin die Karte beim ersten Öffnen schaut.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Fit {
    pub lat: f64,
    pub lon: f64,
    pub zoom: f64,
}

/// Die Ansicht, in die alle Fotos passen — ohne die äußersten paar Prozent.
///
/// `width` und `height` sind die Größe der Karte in Pixeln.
pub fn fit(points: &[(f64, f64)], width: f64, height: f64) -> Option<Fit> {
    if points.is_empty() {
        return None;
    }
    let mut xs: Vec<f64> = points.iter().map(|p| p.0).collect();
    let mut ys: Vec<f64> = points.iter().map(|p| p.1).collect();
    xs.sort_by(f64::total_cmp);
    ys.sort_by(f64::total_cmp);
    let trim = (points.len() as f64 * FIT_TRIM) as usize;
    let last = points.len() - 1 - trim;
    let (min_x, max_x) = (xs[trim], xs[last]);
    let (min_y, max_y) = (ys[trim], ys[last]);

    // Etwas Rand, damit die äußeren Markierungen nicht am Fensterrand kleben.
    let span_x = (max_x - min_x) * 1.2;
    let span_y = (max_y - min_y) * 1.2;
    let zoom_x = if span_x > 0.0 {
        (width / (span_x * TILE_SIZE)).log2()
    } else {
        MAX_FIT_ZOOM
    };
    let zoom_y = if span_y > 0.0 {
        (height / (span_y * TILE_SIZE)).log2()
    } else {
        MAX_FIT_ZOOM
    };
    let zoom = zoom_x.min(zoom_y).clamp(MIN_FIT_ZOOM, MAX_FIT_ZOOM);
    let (lat, lon) = unproject((min_x + max_x) / 2.0, (min_y + max_y) / 2.0);
    Some(Fit { lat, lon, zoom })
}

#[cfg(test)]
mod tests {
    use super::*;

    const CELL: f64 = 80.0;

    fn at(lat: f64, lon: f64) -> (f64, f64) {
        project(lat, lon)
    }

    #[test]
    fn projection_round_trips_and_centres_null_island() {
        assert_eq!(project(0.0, 0.0), (0.5, 0.5));
        for (lat, lon) in [(48.137, 11.575), (-33.86, 151.21), (22.3, 114.17)] {
            let (x, y) = project(lat, lon);
            let (back_lat, back_lon) = unproject(x, y);
            assert!((back_lat - lat).abs() < 1e-9, "{lat} -> {back_lat}");
            assert!((back_lon - lon).abs() < 1e-9, "{lon} -> {back_lon}");
        }
        // Norden liegt oben, also bei kleinerem y.
        assert!(project(60.0, 0.0).1 < project(40.0, 0.0).1);
        // Die Pole liegen außerhalb von Web-Mercator und werden geklemmt.
        let (_, y) = project(90.0, 0.0);
        assert!((0.0..=1.0).contains(&y));
    }

    #[test]
    fn nearby_photos_share_a_marker_and_distant_ones_do_not() {
        let points = [
            at(48.137, 11.575), // München
            at(48.138, 11.576), // ein paar Schritte weiter
            at(48.857, 2.352),  // Paris
        ];
        let clusters = cluster(&points, 8, CELL);
        assert_eq!(clusters.len(), 2);
        assert_eq!(clusters[0].members, [0, 1]);
        assert_eq!(clusters[1].members, [2]);
        // Ganz hinausgezoomt ist Europa ein einziger Punkt.
        assert_eq!(cluster(&points, 1, CELL).len(), 1);
    }

    #[test]
    fn zooming_in_splits_a_group() {
        let points = [at(48.137, 11.575), at(48.150, 11.600)];
        assert_eq!(cluster(&points, 10, CELL).len(), 1);
        assert_eq!(cluster(&points, 17, CELL).len(), 2);
    }

    #[test]
    fn photos_from_the_very_same_spot_never_split() {
        let points = vec![at(52.52, 13.405); 50];
        let clusters = cluster(&points, 19, CELL);
        assert_eq!(clusters.len(), 1);
        assert_eq!(clusters[0].members.len(), 50);
    }

    #[test]
    fn a_place_on_a_cell_border_is_not_cut_in_two() {
        // Zwei Punkte links und rechts einer Zellgrenze, eine Zehntelzelle
        // auseinander.
        let zoom = 12;
        let cells = cells_across(zoom, CELL);
        let border = 1000.0 / cells;
        let tenth = 0.1 / cells;
        let points = [(border - tenth / 2.0, 0.3), (border + tenth / 2.0, 0.3)];
        let clusters = cluster(&points, zoom, CELL);
        assert_eq!(clusters.len(), 1, "{clusters:?}");
        // Zwei volle Zellen auseinander bleiben dagegen zwei.
        let apart = [(border - 1.0 / cells, 0.3), (border + 1.0 / cells, 0.3)];
        assert_eq!(cluster(&apart, zoom, CELL).len(), 2);
    }

    #[test]
    fn every_photo_lands_in_exactly_one_group_newest_first() {
        let points = scattered(5_000);
        for zoom in [0, 3, 6, 9, 12, 15, 18] {
            let clusters = cluster(&points, zoom, CELL);
            let mut seen: Vec<usize> = clusters
                .iter()
                .flat_map(|c| c.members.iter().copied())
                .collect();
            seen.sort_unstable();
            assert_eq!(seen, (0..points.len()).collect::<Vec<_>>(), "zoom {zoom}");
            for cluster in &clusters {
                assert!(cluster.members.windows(2).all(|w| w[0] < w[1]));
            }
            assert!(
                clusters
                    .windows(2)
                    .all(|w| w[0].members[0] < w[1].members[0])
            );
        }
    }

    #[test]
    fn clustering_is_stable() {
        let points = scattered(2_000);
        assert_eq!(cluster(&points, 7, CELL), cluster(&points, 7, CELL));
    }

    #[test]
    fn fit_frames_all_photos() {
        let munich = at(48.137, 11.575);
        let paris = at(48.857, 2.352);
        let fit = fit(&[munich, paris], 1000.0, 700.0).unwrap();
        // Beide Städte liegen innerhalb der Ansicht.
        let world = TILE_SIZE * 2f64.powf(fit.zoom);
        assert!((munich.0 - paris.0).abs() * world < 1000.0);
        assert!(fit.lon > 2.352 && fit.lon < 11.575);
        // Und die Ansicht ist nicht unnötig weit.
        assert!(fit.zoom > 4.0, "{fit:?}");
    }

    #[test]
    fn a_single_place_does_not_zoom_to_a_single_house() {
        let fit = fit(&[at(48.137, 11.575)], 1000.0, 700.0).unwrap();
        assert_eq!(fit.zoom, MAX_FIT_ZOOM);
        assert!((fit.lat - 48.137).abs() < 1e-9);
        assert!(super::fit(&[], 1000.0, 700.0).is_none());
    }

    #[test]
    fn one_far_away_trip_does_not_pull_the_view_to_the_whole_world() {
        let mut points = vec![at(48.137, 11.575); 99];
        points.push(at(-33.86, 151.21)); // Sydney
        let fit = fit(&points, 1000.0, 700.0).unwrap();
        assert!((fit.lat - 48.137).abs() < 1e-6, "{fit:?}");
    }

    #[test]
    fn bounds_contain_what_they_were_built_around() {
        let bounds = Bounds::around([(0.2, 0.3), (0.4, 0.1)]).unwrap();
        assert!(bounds.contains(0.3, 0.2));
        assert!(!bounds.contains(0.5, 0.2));
        assert!(Bounds::around(std::iter::empty()).is_none());
    }

    /// Pseudozufällige, aber feste Punkte in ein paar Häufungen — so wie eine
    /// Bibliothek aussieht: viel zu Hause, einiges auf Reisen.
    fn scattered(n: usize) -> Vec<(f64, f64)> {
        let homes = [
            (48.137, 11.575),
            (52.52, 13.405),
            (22.3, 114.17),
            (43.3, -5.8),
        ];
        let mut state: u64 = 0x9e37_79b9_7f4a_7c15;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state % 1_000_000) as f64 / 1_000_000.0
        };
        (0..n)
            .map(|i| {
                let (lat, lon) = homes[i % homes.len()];
                let spread = if i % 10 == 0 { 5.0 } else { 0.05 };
                at(lat + (next() - 0.5) * spread, lon + (next() - 0.5) * spread)
            })
            .collect()
    }

    /// Zeitmessung, keine Prüfung: `cargo test --release -p icloud-photos-app
    /// cluster_timing -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn cluster_timing() {
        let points = scattered(20_000);
        for zoom in [2, 6, 10, 14, 18] {
            let start = std::time::Instant::now();
            let clusters = cluster(&points, zoom, CELL);
            println!(
                "zoom {zoom:>2}: {:>6} Gruppen in {:?}",
                clusters.len(),
                start.elapsed()
            );
        }
    }
}
