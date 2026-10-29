//! Fotos aus der App herausbekommen: sichern, kopieren, öffnen.
//!
//! Alle drei Wege brauchen dasselbe: die Originale der ausgewählten Fotos. Die
//! liegen aber nur dann im Cache, wenn sie schon einmal geöffnet wurden — der
//! Rest muss erst aus iCloud geholt werden. Deshalb ist jede dieser Aktionen
//! ein Auftrag, der auf eintreffende Downloads wartet, statt eines Aufrufs,
//! der sofort fertig ist.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// Ein ausgewähltes Foto: seine ID und der Name, unter dem es in iCloud liegt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    pub id: String,
    pub filename: String,
}

/// In welchem Format gesichert wird.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Format {
    /// Unverändert, wie es in iCloud liegt.
    #[default]
    Original,
    Jpeg,
    Png,
    Heic,
}

impl Format {
    pub const ALL: [Format; 4] = [Format::Original, Format::Jpeg, Format::Png, Format::Heic];

    pub fn label(self) -> &'static str {
        match self {
            Format::Original => "Original",
            Format::Jpeg => "JPEG",
            Format::Png => "PNG",
            Format::Heic => "HEIC",
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            Format::Original => "Unmodified, as stored in iCloud",
            Format::Jpeg => "Opens everywhere, lossy",
            Format::Png => "Lossless, but large files",
            Format::Heic => "Small and high quality, not supported everywhere",
        }
    }

    /// Endung der Zieldatei; `None` heisst: die des Originals behalten.
    fn extension(self) -> Option<&'static str> {
        match self {
            Format::Original => None,
            Format::Jpeg => Some("jpg"),
            Format::Png => Some("png"),
            Format::Heic => Some("heic"),
        }
    }
}

/// Wie eine Quelldatei vorliegt, erkannt an der Endung.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Heic,
    Jpeg,
    Png,
    /// Videos und alles Unbekannte: wird nie umgewandelt.
    Other,
}

fn kind_of(path: &Path) -> Kind {
    let extension = path
        .extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    match extension.as_str() {
        "heic" | "heif" => Kind::Heic,
        "jpg" | "jpeg" => Kind::Jpeg,
        "png" => Kind::Png,
        _ => Kind::Other,
    }
}

/// Was mit den Originalen geschehen soll, sobald sie beisammen sind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// In dieses Verzeichnis sichern, dabei ins gewünschte Format bringen.
    SaveTo(PathBuf, Format),
    /// Als Dateiliste in die Zwischenablage legen.
    Clipboard,
    /// Mit der Standardanwendung öffnen.
    Open,
    /// An Retouch übergeben („Edit in Retouch").
    Retouch,
}

impl Action {
    /// Verb für die Fortschrittsanzeige.
    pub fn verb(&self) -> &'static str {
        match self {
            Action::SaveTo(..) => "Exporting",
            Action::Clipboard => "Copying",
            Action::Open => "Opening",
            Action::Retouch => "Preparing for Retouch",
        }
    }
}

/// Ein laufender Auftrag.
pub struct Job {
    pub action: Action,
    /// Fotos, deren Original noch unterwegs ist.
    waiting: Vec<Item>,
    /// Fertige Originale.
    ready: Vec<(Item, PathBuf)>,
    /// Fotos, deren Original nicht zu haben war.
    failed: Vec<Item>,
    total: usize,
}

impl Job {
    /// Legt einen Auftrag an. `cached` liefert den Pfad eines bereits
    /// vorhandenen Originals; alles andere muss nachgeladen werden.
    ///
    /// Gibt den Auftrag und die IDs zurück, die angefordert werden müssen.
    pub fn new(
        action: Action,
        items: Vec<Item>,
        cached: impl Fn(&str) -> Option<PathBuf>,
    ) -> (Self, Vec<String>) {
        let total = items.len();
        let mut job = Self {
            action,
            waiting: Vec::new(),
            ready: Vec::new(),
            failed: Vec::new(),
            total,
        };
        let mut wanted = Vec::new();
        for item in items {
            match cached(&item.id) {
                Some(path) => job.ready.push((item, path)),
                None => {
                    wanted.push(item.id.clone());
                    job.waiting.push(item);
                }
            }
        }
        (job, wanted)
    }

    /// Ein Original ist eingetroffen. Gibt zurück, ob es zu diesem Auftrag
    /// gehörte.
    pub fn resolve(&mut self, id: &str, path: &Path) -> bool {
        let Some(index) = self.waiting.iter().position(|item| item.id == id) else {
            return false;
        };
        let item = self.waiting.remove(index);
        self.ready.push((item, path.to_path_buf()));
        true
    }

    /// Ein Original ist nicht zu haben.
    pub fn reject(&mut self, id: &str) -> bool {
        let Some(index) = self.waiting.iter().position(|item| item.id == id) else {
            return false;
        };
        let item = self.waiting.remove(index);
        self.failed.push(item);
        true
    }

    pub fn is_done(&self) -> bool {
        self.waiting.is_empty()
    }

    /// Wie viele Originale schon beisammen sind.
    pub fn progress(&self) -> (usize, usize) {
        (self.total - self.waiting.len(), self.total)
    }

    pub fn ready(&self) -> &[(Item, PathBuf)] {
        &self.ready
    }

    pub fn failed(&self) -> usize {
        self.failed.len()
    }
}

/// Was beim Sichern herauskam.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Saved {
    pub saved: usize,
    /// Videos, die unabhängig vom gewählten Format im Original gesichert
    /// wurden — ein Video lässt sich nicht in ein Bildformat bringen.
    pub kept_as_video: usize,
    pub errors: Vec<String>,
}

/// Sichert die Originale in ein Verzeichnis, auf Wunsch umgewandelt.
///
/// Blockiert: Umwandeln kostet je Foto einen Bruchteil einer Sekunde, bei
/// einer grossen Auswahl also spürbar lange. Gehört in einen Hilfsthread.
pub fn save_into(dir: &Path, files: &[(Item, PathBuf)], format: Format) -> Saved {
    let mut result = Saved::default();
    let mut taken: HashSet<PathBuf> = HashSet::new();

    for (item, source) in files {
        let kind = kind_of(source);
        let wanted = match format.extension() {
            Some(extension) if kind != Kind::Other => Some(extension),
            _ => None,
        };
        if kind == Kind::Other && format != Format::Original {
            result.kept_as_video += 1;
        }

        // Liegt es schon im Zielformat vor, genügt eine Kopie — ein zweites
        // Kodieren kostete nur Qualität.
        let convert = wanted.is_some() && !already_in(kind, format);
        let filename = match wanted {
            Some(extension) if convert => renamed(&item.filename, extension),
            _ => item.filename.clone(),
        };
        let target = unique_target(dir, &filename, &taken);

        let outcome = if convert {
            convert_file(source, kind, &target, format)
        } else {
            std::fs::copy(source, &target)
                .map(|_| ())
                .map_err(|error| error.to_string())
        };
        match outcome {
            Ok(()) => {
                taken.insert(target);
                result.saved += 1;
            }
            Err(error) => {
                // Keine halbe Datei liegen lassen.
                let _ = std::fs::remove_file(&target);
                result.errors.push(format!("{}: {error}", item.filename));
            }
        }
    }
    result
}

fn already_in(kind: Kind, format: Format) -> bool {
    matches!(
        (kind, format),
        (Kind::Heic, Format::Heic) | (Kind::Jpeg, Format::Jpeg) | (Kind::Png, Format::Png)
    )
}

/// `IMG_0001.HEIC` → `IMG_0001.jpg`.
fn renamed(filename: &str, extension: &str) -> String {
    let stem = Path::new(filename)
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "foto".to_string());
    format!("{stem}.{extension}")
}

/// Qualität für verlustbehaftete Ziele. 92 ist bei Fotos von einer
/// verlustfreien Kopie kaum zu unterscheiden und deutlich kleiner.
const QUALITY: &str = "92";

/// Wandelt eine Datei um.
///
/// HEIC geht über die Werkzeuge von libheif, das die App ohnehin braucht:
/// `heif-dec` schreibt die EXIF-Daten — Aufnahmezeit, Ort — mit in das JPEG,
/// was der Umweg über gdk-pixbuf nicht täte. JPEG und PNG untereinander
/// erledigt gdk-pixbuf direkt.
fn convert_file(source: &Path, kind: Kind, target: &Path, format: Format) -> Result<(), String> {
    match (kind, format) {
        (Kind::Heic, Format::Jpeg | Format::Png) => run(std::process::Command::new("heif-dec")
            .args(["--quiet", "-q", QUALITY])
            .arg(source)
            .arg(target)),
        (Kind::Jpeg | Kind::Png, Format::Heic) => run(std::process::Command::new("heif-enc")
            .args(["-q", QUALITY, "-o"])
            .arg(target)
            .arg(source)),
        (Kind::Jpeg | Kind::Png, Format::Jpeg | Format::Png) => {
            use gtk::gdk_pixbuf::Pixbuf;
            let pixbuf = Pixbuf::from_file(source).map_err(|error| error.to_string())?;
            // Ohne das stünde ein hochkant aufgenommenes Foto quer.
            let pixbuf = pixbuf.apply_embedded_orientation().unwrap_or(pixbuf);
            let result = if format == Format::Jpeg {
                pixbuf.savev(target, "jpeg", &[("quality", QUALITY)])
            } else {
                pixbuf.savev(target, "png", &[])
            };
            result.map_err(|error| error.to_string())
        }
        _ => Err("cannot convert".to_string()),
    }
}

fn run(command: &mut std::process::Command) -> Result<(), String> {
    let program = command.get_program().to_string_lossy().into_owned();
    let output = command.output().map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            format!("{program} is missing — install the libheif package")
        } else {
            format!("{program}: {error}")
        }
    })?;
    if output.status.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        Err(format!("{program}: {}", stderr.trim()))
    }
}

/// Ein freier Dateiname im Zielverzeichnis.
///
/// iCloud vergibt Dateinamen nicht eindeutig — `IMG_0001.HEIC` gibt es in einer
/// grossen Bibliothek mehrfach. Ohne diesen Schritt überschriebe ein Export
/// stillschweigend, was er gerade selbst geschrieben hat. `taken` deckt die
/// Dateien dieses Laufs ab, die auf der Platte schon existierenden deckt
/// `exists` ab.
fn unique_target(dir: &Path, filename: &str, taken: &HashSet<PathBuf>) -> PathBuf {
    let stem = Path::new(filename)
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "foto".to_string());
    let suffix = Path::new(filename)
        .extension()
        .map(|e| format!(".{}", e.to_string_lossy()))
        .unwrap_or_default();

    let mut candidate = dir.join(filename);
    let mut counter = 1;
    while taken.contains(&candidate) || candidate.exists() {
        candidate = dir.join(format!("{stem}-{counter}{suffix}"));
        counter += 1;
    }
    candidate
}

#[cfg(test)]
mod tests {
    use super::*;

    fn items(ids: &[&str]) -> Vec<Item> {
        ids.iter()
            .map(|id| Item {
                id: (*id).to_string(),
                filename: format!("{id}.HEIC"),
            })
            .collect()
    }

    #[test]
    fn cached_originals_need_no_download() {
        let (job, wanted) = Job::new(Action::Clipboard, items(&["a", "b"]), |id| {
            (id == "a").then(|| PathBuf::from("/cache/a.heic"))
        });
        assert_eq!(wanted, ["b"]);
        assert!(!job.is_done());
        assert_eq!(job.progress(), (1, 2));
    }

    #[test]
    fn a_job_finishes_when_every_original_is_settled() {
        let (mut job, wanted) = Job::new(Action::Clipboard, items(&["a", "b", "c"]), |_| None);
        assert_eq!(wanted, ["a", "b", "c"]);

        assert!(job.resolve("a", Path::new("/cache/a.heic")));
        assert_eq!(job.progress(), (1, 3));
        // Fremde Meldungen gehören nicht zu diesem Auftrag.
        assert!(!job.resolve("fremd", Path::new("/cache/x.heic")));

        assert!(job.reject("b"));
        assert!(!job.is_done());
        assert!(job.resolve("c", Path::new("/cache/c.heic")));

        assert!(job.is_done());
        assert_eq!(job.ready().len(), 2);
        // Das abgelehnte bleibt als Fehlschlag stehen.
        assert_eq!(job.failed(), 1);
    }

    #[test]
    fn saving_never_overwrites() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("quelle.bin");
        std::fs::write(&source, b"inhalt").unwrap();

        // Dreimal derselbe Dateiname, wie ihn iCloud tatsächlich vergibt.
        let files: Vec<(Item, PathBuf)> = (0..3)
            .map(|index| {
                (
                    Item {
                        id: format!("id-{index}"),
                        filename: "IMG_0001.HEIC".to_string(),
                    },
                    source.clone(),
                )
            })
            .collect();

        let target = dir.path().join("ziel");
        std::fs::create_dir(&target).unwrap();
        let result = save_into(&target, &files, Format::Original);

        assert_eq!(result.saved, 3);
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        assert!(target.join("IMG_0001.HEIC").is_file());
        assert!(target.join("IMG_0001-1.HEIC").is_file());
        assert!(target.join("IMG_0001-2.HEIC").is_file());
    }

    #[test]
    fn saving_reports_what_went_wrong() {
        let dir = tempfile::tempdir().unwrap();
        let files = vec![(
            Item {
                id: "a".into(),
                filename: "IMG_0001.HEIC".into(),
            },
            dir.path().join("gibt-es-nicht"),
        )];
        let result = save_into(dir.path(), &files, Format::Original);
        assert_eq!(result.saved, 0);
        assert_eq!(result.errors.len(), 1);
        assert!(result.errors[0].starts_with("IMG_0001.HEIC:"));
    }

    fn item(filename: &str) -> Item {
        Item {
            id: filename.to_string(),
            filename: filename.to_string(),
        }
    }

    /// Ein kleines echtes Bild, damit die Umwandlung etwas zu tun hat.
    fn sample_png(path: &Path) {
        let pixbuf =
            gtk::gdk_pixbuf::Pixbuf::new(gtk::gdk_pixbuf::Colorspace::Rgb, false, 8, 32, 24)
                .unwrap();
        pixbuf.fill(0x3366_99ff);
        pixbuf.savev(path, "png", &[]).unwrap();
    }

    fn has_tool(name: &str) -> bool {
        std::process::Command::new(name)
            .arg("--version")
            .output()
            .is_ok()
    }

    #[test]
    fn converts_between_jpeg_and_png() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("quelle.png");
        sample_png(&source);
        let out = dir.path().join("out");
        std::fs::create_dir(&out).unwrap();

        let result = save_into(&out, &[(item("IMG_0001.PNG"), source)], Format::Jpeg);
        assert_eq!(result.saved, 1, "{:?}", result.errors);

        let jpeg = out.join("IMG_0001.jpg");
        let loaded = gtk::gdk_pixbuf::Pixbuf::from_file(&jpeg).unwrap();
        assert_eq!((loaded.width(), loaded.height()), (32, 24));
    }

    #[test]
    fn heic_round_trip_keeps_the_image() {
        if !has_tool("heif-enc") || !has_tool("heif-dec") {
            eprintln!("übersprungen: libheif-Werkzeuge fehlen");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("quelle.png");
        sample_png(&source);

        let heic_dir = dir.path().join("heic");
        std::fs::create_dir(&heic_dir).unwrap();
        let result = save_into(&heic_dir, &[(item("IMG_0002.PNG"), source)], Format::Heic);
        assert_eq!(result.saved, 1, "{:?}", result.errors);
        let heic = heic_dir.join("IMG_0002.heic");
        assert!(heic.is_file());

        let png_dir = dir.path().join("png");
        std::fs::create_dir(&png_dir).unwrap();
        let result = save_into(&png_dir, &[(item("IMG_0002.HEIC"), heic)], Format::Png);
        assert_eq!(result.saved, 1, "{:?}", result.errors);
        let loaded = gtk::gdk_pixbuf::Pixbuf::from_file(png_dir.join("IMG_0002.png")).unwrap();
        assert_eq!((loaded.width(), loaded.height()), (32, 24));
    }

    #[test]
    fn same_format_and_videos_are_copied_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let photo = dir.path().join("a.heic");
        let video = dir.path().join("b.mov");
        std::fs::write(&photo, b"heic-bytes").unwrap();
        std::fs::write(&video, b"mov-bytes").unwrap();
        let out = dir.path().join("out");
        std::fs::create_dir(&out).unwrap();

        let result = save_into(
            &out,
            &[(item("IMG_1.HEIC"), photo), (item("IMG_2.MOV"), video)],
            Format::Heic,
        );

        assert_eq!(result.saved, 2, "{:?}", result.errors);
        assert_eq!(result.kept_as_video, 1);
        // Byte fuer Byte das Original — kein zweites Kodieren.
        assert_eq!(
            std::fs::read(out.join("IMG_1.HEIC")).unwrap(),
            b"heic-bytes"
        );
        assert_eq!(std::fs::read(out.join("IMG_2.MOV")).unwrap(), b"mov-bytes");
    }

    #[test]
    fn a_failed_conversion_leaves_no_half_file() {
        let dir = tempfile::tempdir().unwrap();
        let broken = dir.path().join("kaputt.png");
        std::fs::write(&broken, b"kein bild").unwrap();
        let out = dir.path().join("out");
        std::fs::create_dir(&out).unwrap();

        let result = save_into(&out, &[(item("IMG_3.PNG"), broken)], Format::Jpeg);

        assert_eq!(result.saved, 0);
        assert_eq!(result.errors.len(), 1);
        assert!(std::fs::read_dir(&out).unwrap().next().is_none());
    }
}
