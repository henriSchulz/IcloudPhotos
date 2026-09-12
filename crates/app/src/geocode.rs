//! Ortsnamen zu Koordinaten — nachgeschlagen, sobald ein Foto offen ist.
//!
//! iCloud liefert zum Aufnahmeort nur Breiten- und Längengrad. Einen Namen
//! rechnet Apples Photos lokal aus; hier kommt er von Nominatim (OpenStreetMap)
//! und wird im Katalog gemerkt, damit dieselbe Stelle nie zweimal angefragt
//! wird.
//!
//! Zwei Randbedingungen bestimmen den Zuschnitt:
//!
//! - **Nominatim erlaubt eine Anfrage pro Sekunde** und verlangt einen
//!   sprechenden User-Agent. Deshalb ein einzelner Arbeitsfaden mit Pause
//!   dazwischen, kein paralleler Schwarm.
//! - **Jede Anfrage verrät, wo jemand war.** Gefragt wird darum nur für Fotos,
//!   die tatsächlich geöffnet werden — nicht für die ganze Bibliothek im
//!   Hintergrund. Wer nie ein Foto öffnet, schickt nichts weg.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use chrono::Utc;
use icloud_photos_cache::{Catalog, place_cell};
use tokio::sync::mpsc;

/// Nominatims Nutzungsregeln: höchstens eine Anfrage pro Sekunde.
const MIN_INTERVAL: Duration = Duration::from_secs(1);

/// Ohne sprechenden User-Agent weist Nominatim Anfragen ab.
const USER_AGENT: &str = concat!(
    "icloud-photos/",
    env!("CARGO_PKG_VERSION"),
    " (persoenlicher iCloud-Photos-Client fuer Linux)"
);

/// Zoomstufe der Rückwärtssuche: Straße und Ortsteil, nicht das einzelne Haus.
const ZOOM: u8 = 16;

const TIMEOUT: Duration = Duration::from_secs(10);

/// Ein aufgelöster Ort, auf dem Weg zurück in die Oberfläche.
pub struct Resolved {
    pub id: String,
    /// `None` heisst: der Dienst kennt die Stelle nicht.
    pub place: Option<String>,
}

struct Request {
    id: String,
    lat: f64,
    lon: f64,
}

/// Griff auf den Geocoder.
pub struct Geocoder {
    requests: mpsc::UnboundedSender<Request>,
}

impl Geocoder {
    /// Fragt den Ortsnamen für ein Foto an.
    pub fn request(&self, id: &str, lat: f64, lon: f64) {
        let _ = self.requests.send(Request {
            id: id.to_string(),
            lat,
            lon,
        });
    }
}

/// Startet den Geocoder. Er meldet sich nur, wenn er einen Namen hat.
pub fn spawn(db_path: PathBuf) -> (Geocoder, mpsc::UnboundedReceiver<Resolved>) {
    let (request_tx, mut request_rx) = mpsc::unbounded_channel::<Request>();
    let (answer_tx, answer_rx) = mpsc::unbounded_channel::<Resolved>();

    std::thread::spawn(move || {
        let catalog = match Catalog::open(&db_path) {
            Ok(catalog) => catalog,
            Err(error) => {
                tracing::error!(%error, "Katalog fuer Ortsnamen nicht zu oeffnen");
                return;
            }
        };
        let agent = ureq::Agent::config_builder()
            .timeout_global(Some(TIMEOUT))
            .user_agent(USER_AGENT)
            .build()
            .new_agent();

        let mut last_call: Option<Instant> = None;

        while let Some(Request { id, lat, lon }) = request_rx.blocking_recv() {
            let cell = place_cell(lat, lon);

            // Schon einmal nachgesehen? Dann kostet es nichts.
            match catalog.place(&cell) {
                Ok(Some(known)) => {
                    if known.is_some() {
                        let _ = answer_tx.send(Resolved { id, place: known });
                    }
                    continue;
                }
                Ok(None) => {}
                Err(error) => {
                    tracing::warn!(%error, "Ortscache nicht lesbar");
                    continue;
                }
            }

            // Eine Anfrage pro Sekunde, wie die Nutzungsregeln es verlangen.
            if let Some(last) = last_call {
                let waited = last.elapsed();
                if waited < MIN_INTERVAL {
                    std::thread::sleep(MIN_INTERVAL - waited);
                }
            }
            last_call = Some(Instant::now());

            let place = match lookup(&agent, lat, lon) {
                Ok(place) => place,
                Err(error) => {
                    // Kein Netz, kein Name — aber auch nichts, was gemerkt
                    // werden dürfte: beim nächsten Mal soll es erneut gehen.
                    tracing::info!(%error, %cell, "Ortsname nicht aufloesbar");
                    continue;
                }
            };

            if let Err(error) = catalog.remember_place(&cell, place.as_deref(), Utc::now()) {
                tracing::warn!(%error, "Ortsname nicht gemerkt");
            }
            if place.is_some() {
                let _ = answer_tx.send(Resolved { id, place });
            }
        }
    });

    (
        Geocoder {
            requests: request_tx,
        },
        answer_rx,
    )
}

/// Fragt Nominatim nach der Adresse zu einer Koordinate.
fn lookup(agent: &ureq::Agent, lat: f64, lon: f64) -> Result<Option<String>, ureq::Error> {
    let url = format!(
        "https://nominatim.openstreetmap.org/reverse\
         ?format=jsonv2&zoom={ZOOM}&addressdetails=1&lat={lat}&lon={lon}"
    );
    let body: serde_json::Value = agent.get(&url).call()?.body_mut().read_json()?;
    Ok(place_name(&body))
}

/// Macht aus Nominatims Adressbaum eine Zeile für die Kopfleiste.
///
/// Gesucht ist, was man einem Foto ansehen würde: die nähere Umgebung und die
/// Stadt. Die volle Adresse („Marienplatz 8, 80331 München, Bayern,
/// Deutschland") wäre für eine Titelzeile unbrauchbar.
fn place_name(body: &serde_json::Value) -> Option<String> {
    let address = body.get("address")?;
    let pick = |keys: &[&str]| -> Option<String> {
        keys.iter()
            .find_map(|key| address.get(*key)?.as_str().map(str::to_string))
    };

    // Das Nähere zuerst: ein benannter Ort schlägt die Straße, die Straße den
    // Ortsteil.
    let detail = pick(&[
        "tourism",
        "attraction",
        "leisure",
        "building",
        "road",
        "neighbourhood",
        "suburb",
    ]);
    let area = pick(&[
        "city",
        "town",
        "village",
        "municipality",
        "county",
        "state",
        "country",
    ]);

    match (detail, area) {
        (Some(detail), Some(area)) if detail != area => Some(format!("{detail}, {area}")),
        (Some(only), _) | (None, Some(only)) => Some(only),
        (None, None) => body
            .get("display_name")
            .and_then(|name| name.as_str())
            .map(str::to_string),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn address(json: &str) -> serde_json::Value {
        serde_json::from_str(json).unwrap()
    }

    /// Fragt wirklich bei Nominatim an — deshalb nicht im normalen Lauf.
    ///
    /// Prüft die Kette, die sich sonst nirgends prüfen lässt: Anfrage, Antwort,
    /// Auswertung und der Eintrag im Ortscache.
    ///
    ///     cargo test -p icloud-photos-app -- --ignored --nocapture ueber_das_netz
    #[test]
    #[ignore = "fragt einen fremden Dienst über das Netz"]
    fn ueber_das_netz_aufloesen() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("catalog.sqlite");
        Catalog::open(&db).unwrap();

        let (geocoder, mut answers) = spawn(db.clone());
        geocoder.request("foto-1", 48.1372, 11.5756);

        let resolved = answers.blocking_recv().expect("keine Antwort");
        assert_eq!(resolved.id, "foto-1");
        let place = resolved.place.expect("Nominatim kennt den Marienplatz");
        println!("aufgelöst: {place}");
        assert!(place.contains("München"), "unerwartet: {place}");

        // Und der Name liegt jetzt im Katalog, die zweite Anfrage geht nicht
        // mehr ins Netz.
        let catalog = Catalog::open(&db).unwrap();
        let cell = place_cell(48.1372, 11.5756);
        assert_eq!(catalog.place(&cell).unwrap(), Some(Some(place)));
    }

    #[test]
    fn names_the_place_and_the_town() {
        let body = address(
            r#"{"address":{"tourism":"Marienplatz","road":"Rindermarkt",
                "suburb":"Altstadt","city":"München","state":"Bayern",
                "country":"Deutschland"}}"#,
        );
        assert_eq!(place_name(&body).as_deref(), Some("Marienplatz, München"));
    }

    #[test]
    fn falls_back_through_the_hierarchy() {
        // Kein benannter Ort, keine Strasse: dann eben Ortsteil und Stadt.
        let body = address(r#"{"address":{"suburb":"Altstadt","city":"München"}}"#);
        assert_eq!(place_name(&body).as_deref(), Some("Altstadt, München"));

        // Auf dem Land bleibt oft nur das Dorf.
        let body = address(r#"{"address":{"village":"Kochel am See"}}"#);
        assert_eq!(place_name(&body).as_deref(), Some("Kochel am See"));

        // Auf hoher See kennt Nominatim nur das Land — oder nichts.
        let body = address(r#"{"address":{"country":"Norwegen"}}"#);
        assert_eq!(place_name(&body).as_deref(), Some("Norwegen"));
    }

    #[test]
    fn does_not_repeat_the_same_word_twice() {
        // Stadtstaaten liefern denselben Namen auf zwei Ebenen.
        let body = address(r#"{"address":{"suburb":"Monaco","city":"Monaco"}}"#);
        assert_eq!(place_name(&body).as_deref(), Some("Monaco"));
    }

    #[test]
    fn without_an_address_there_is_no_name() {
        assert_eq!(
            place_name(&address(r#"{"error":"Unable to geocode"}"#)),
            None
        );
    }

    #[test]
    fn uses_display_name_when_the_tree_is_unfamiliar() {
        let body = address(r#"{"address":{"leisure_park":"x"},"display_name":"Irgendwo"}"#);
        assert_eq!(place_name(&body).as_deref(), Some("Irgendwo"));
    }
}
