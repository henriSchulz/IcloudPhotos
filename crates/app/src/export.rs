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

/// Was mit den Originalen geschehen soll, sobald sie beisammen sind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// In dieses Verzeichnis kopieren.
    SaveTo(PathBuf),
    /// Als Dateiliste in die Zwischenablage legen.
    Clipboard,
    /// Mit der Standardanwendung öffnen.
    Open,
}

impl Action {
    /// Verb für die Fortschrittsanzeige.
    pub fn verb(&self) -> &'static str {
        match self {
            Action::SaveTo(_) => "Sichere",
            Action::Clipboard => "Kopiere",
            Action::Open => "Öffne",
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

/// Kopiert die Originale in ein Verzeichnis. Gibt zurück, wie viele ankamen
/// und welche Fehler auftraten.
pub fn save_into(dir: &Path, files: &[(Item, PathBuf)]) -> (usize, Vec<String>) {
    let mut saved = 0;
    let mut errors = Vec::new();
    let mut taken: HashSet<PathBuf> = HashSet::new();

    for (item, source) in files {
        let target = unique_target(dir, &item.filename, &taken);
        match std::fs::copy(source, &target) {
            Ok(_) => {
                taken.insert(target);
                saved += 1;
            }
            Err(error) => errors.push(format!("{}: {error}", item.filename)),
        }
    }
    (saved, errors)
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
        let (saved, errors) = save_into(&target, &files);

        assert_eq!(saved, 3);
        assert!(errors.is_empty(), "{errors:?}");
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
        let (saved, errors) = save_into(dir.path(), &files);
        assert_eq!(saved, 0);
        assert_eq!(errors.len(), 1);
        assert!(errors[0].starts_with("IMG_0001.HEIC:"));
    }
}
