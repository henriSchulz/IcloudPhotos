//! Alben im Katalog: welche es gibt und welche Fotos in ihnen liegen.
//!
//! iCloud kennt Alben nur als Liste von Asset-IDs. Der Katalog hält sie in zwei
//! Tabellen: `albums` für Name, Art und Ordner, `album_photos` für die
//! Zugehörigkeit. Ein Album-Sync ersetzt beide vollständig — siehe
//! [`Catalog::replace_albums`].

use std::collections::HashMap;
use std::path::PathBuf;

use icloud_photos_core::model::{AlbumKind, AlbumRecord};
use rusqlite::params;

use crate::CacheError;
use crate::db::{Catalog, from_sql_bytes};

/// Welcher Ausschnitt der Bibliothek gezeigt wird.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Scope {
    /// Alle Fotos.
    #[default]
    Library,
    /// Nur die Fotos eines Albums — auch eines intelligenten wie Favorites.
    Album(String),
}

impl Scope {
    /// `FROM`-Teil samt Filter und dem Wert für seinen Platzhalter.
    ///
    /// Die Bibliothek bekommt bewusst keinen Join und kein `WHERE`: nur so
    /// liest SQLite die Seiten direkt aus `idx_photos_capture`.
    pub(crate) fn source(&self, placeholder: usize) -> (String, Option<&str>) {
        match self {
            Self::Library => ("photos".to_string(), None),
            Self::Album(id) => (
                format!(
                    "photos JOIN album_photos ON album_photos.photo_id = photos.id \
                     WHERE album_photos.album_id = ?{placeholder}"
                ),
                Some(id.as_str()),
            ),
        }
    }
}

/// Was in einem Album liegt, wie der Album-Sync es übergibt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AlbumContent {
    pub id: String,
    /// Größe laut iCloud. Sie entscheidet beim nächsten Lauf, ob die
    /// Mitglieder neu geholt werden müssen.
    pub size: Option<u64>,
    pub members: Vec<String>,
}

/// Ein Album, wie die Seitenleiste es braucht.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Album {
    pub id: String,
    pub name: String,
    pub kind: AlbumKind,
    pub parent: Option<String>,
    /// Wie viele seiner Fotos im Katalog liegen. Ordner zählen 0.
    pub count: u64,
    /// Vorschau des zuletzt aufgenommenen Fotos — das Titelbild.
    pub cover: Option<PathBuf>,
}

impl Catalog {
    /// Ersetzt alle Alben und ihre Zugehörigkeiten in einer Transaktion.
    ///
    /// Vollständig statt Stück für Stück: iCloud meldet keine einzelnen
    /// Album-Änderungen, die sich billig nachvollziehen ließen, und ein
    /// umbenanntes, verschobenes oder gelöschtes Album soll nicht als Leiche
    /// stehen bleiben. Scheitert der Album-Sync unterwegs, kommt dieser Aufruf
    /// gar nicht erst — der alte Stand bleibt dann ganz erhalten.
    pub fn replace_albums(
        &mut self,
        albums: &[AlbumRecord],
        contents: &[AlbumContent],
    ) -> Result<(), CacheError> {
        let sizes: HashMap<&str, u64> = contents
            .iter()
            .filter_map(|content| Some((content.id.as_str(), content.size?)))
            .collect();
        let tx = self.conn.transaction()?;
        tx.execute("DELETE FROM album_photos", [])?;
        tx.execute("DELETE FROM albums", [])?;
        {
            let mut insert = tx.prepare(
                "INSERT OR REPLACE INTO albums (id, name, kind, parent_id, position, size)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            )?;
            for album in albums {
                insert.execute(params![
                    album.id,
                    album.name,
                    album.kind.as_str(),
                    album.parent,
                    crate::db::to_sql_bytes(album.position),
                    sizes
                        .get(album.id.as_str())
                        .copied()
                        .map(crate::db::to_sql_bytes),
                ])?;
            }
            let mut member = tx.prepare(
                "INSERT OR IGNORE INTO album_photos (album_id, photo_id) VALUES (?1, ?2)",
            )?;
            for content in contents {
                for photo in &content.members {
                    member.execute(params![content.id, photo])?;
                }
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Die Größe jedes Albums laut iCloud beim letzten Laden.
    pub fn album_sizes(&self) -> Result<HashMap<String, u64>, CacheError> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, size FROM albums WHERE size IS NOT NULL")?;
        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, from_sql_bytes(row.get(1)?)))
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Die gemerkten Mitglieder eines Albums — für ein unverändertes Album,
    /// dessen Mitglieder nicht neu geholt wurden.
    pub fn album_member_ids(&self, id: &str) -> Result<Vec<String>, CacheError> {
        let mut stmt = self
            .conn
            .prepare("SELECT photo_id FROM album_photos WHERE album_id = ?1")?;
        let rows = stmt.query_map(params![id], |row| row.get(0))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Alle Alben in der Reihenfolge, in der iCloud sie liefert.
    pub fn albums(&self) -> Result<Vec<Album>, CacheError> {
        let mut stmt = self.conn.prepare(
            r#"
            SELECT a.id, a.name, a.kind, a.parent_id,
                   (SELECT COUNT(*)
                      FROM album_photos ap JOIN photos p ON p.id = ap.photo_id
                     WHERE ap.album_id = a.id),
                   (SELECT p.thumb_path
                      FROM album_photos ap JOIN photos p ON p.id = ap.photo_id
                     WHERE ap.album_id = a.id AND p.thumb_path IS NOT NULL
                     ORDER BY COALESCE(p.asset_date, p.added_date) DESC, p.id DESC
                     LIMIT 1)
              FROM albums a
             ORDER BY a.position
            "#,
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, i64>(4)?,
                row.get::<_, Option<String>>(5)?,
            ))
        })?;
        let mut albums = Vec::new();
        for row in rows {
            let (id, name, kind, parent, count, cover) = row?;
            // Eine Art, die diese Version nicht kennt, gehört nicht in die
            // Seitenleiste.
            let Some(kind) = AlbumKind::parse(&kind) else {
                continue;
            };
            albums.push(Album {
                id,
                name,
                kind,
                parent,
                count: from_sql_bytes(count),
                cover: cover.map(PathBuf::from),
            });
        }
        Ok(albums)
    }
}

#[cfg(test)]
mod tests {
    use chrono::{DateTime, Utc};
    use icloud_photos_core::model::PhotoRecord;

    use super::*;

    fn record(id: &str, day: u32) -> PhotoRecord {
        PhotoRecord {
            id: id.into(),
            master_id: None,
            filename: format!("{id}.HEIC"),
            added_date: DateTime::parse_from_rfc3339(&format!("2026-01-{day:02}T00:00:00Z"))
                .unwrap()
                .with_timezone(&Utc),
            asset_date: None,
            checksum: None,
            width: None,
            height: None,
            item_type: None,
            lat: None,
            lon: None,
        }
    }

    fn album(id: &str, kind: AlbumKind, parent: Option<&str>, position: u64) -> AlbumRecord {
        AlbumRecord {
            id: id.into(),
            name: format!("Name {id}"),
            kind,
            parent: parent.map(Into::into),
            position,
        }
    }

    fn content(id: &str, members: &[&str]) -> AlbumContent {
        AlbumContent {
            id: id.into(),
            size: Some(members.len() as u64),
            members: members.iter().map(|m| m.to_string()).collect(),
        }
    }

    #[test]
    fn sizes_and_members_survive_for_the_next_run() {
        let mut catalog = catalog();
        catalog
            .replace_albums(
                &[album("trip", AlbumKind::Album, None, 0)],
                &[content("trip", &["a", "c", "missing"])],
            )
            .unwrap();
        assert_eq!(catalog.album_sizes().unwrap().get("trip"), Some(&3));
        let mut members = catalog.album_member_ids("trip").unwrap();
        members.sort();
        // Auch ein noch fehlendes Foto bleibt gemerkt — es kommt vielleicht
        // mit dem nächsten Sync.
        assert_eq!(members, ["a", "c", "missing"]);
    }

    fn catalog() -> Catalog {
        let mut catalog = Catalog::open_in_memory().unwrap();
        catalog
            .upsert_batch(&[record("a", 1), record("b", 2), record("c", 3)])
            .unwrap();
        catalog
    }

    #[test]
    fn album_scope_shows_only_its_photos_newest_first() {
        let mut catalog = catalog();
        catalog
            .replace_albums(
                &[album("trip", AlbumKind::Album, None, 0)],
                &[content("trip", &["a", "c"])],
            )
            .unwrap();

        let scope = Scope::Album("trip".into());
        let ids: Vec<String> = catalog
            .page_in(&scope, 0, 10)
            .unwrap()
            .iter()
            .map(|p| p.id().to_string())
            .collect();
        assert_eq!(ids, ["c", "a"]);
        assert_eq!(catalog.count_in(&scope).unwrap(), 2);
        assert_eq!(catalog.count_in(&Scope::Library).unwrap(), 3);

        let months = catalog.month_groups_in(&scope).unwrap();
        assert_eq!(months.len(), 1);
        assert_eq!(months[0].count, 2);
    }

    #[test]
    fn replacing_drops_albums_that_are_gone() {
        let mut catalog = catalog();
        catalog
            .replace_albums(
                &[
                    album("old", AlbumKind::Album, None, 0),
                    album("keep", AlbumKind::Album, None, 1),
                ],
                &[content("old", &["a"])],
            )
            .unwrap();
        catalog
            .replace_albums(
                &[album("keep", AlbumKind::Album, None, 0)],
                &[content("keep", &["b"])],
            )
            .unwrap();

        let albums = catalog.albums().unwrap();
        assert_eq!(albums.len(), 1);
        assert_eq!(albums[0].id, "keep");
        assert_eq!(catalog.count_in(&Scope::Album("old".into())).unwrap(), 0);
    }

    #[test]
    fn albums_come_with_count_cover_and_order() {
        let mut catalog = catalog();
        catalog
            .set_thumbnail("a", std::path::Path::new("/thumbs/a.jpg"), 1)
            .unwrap();
        catalog
            .set_thumbnail("c", std::path::Path::new("/thumbs/c.jpg"), 1)
            .unwrap();
        catalog
            .replace_albums(
                &[
                    album("Favorites", AlbumKind::Smart, None, 0),
                    album("2024", AlbumKind::Folder, None, 1),
                    album("trip", AlbumKind::Album, Some("2024"), 2),
                ],
                &[
                    content("Favorites", &["a", "b"]),
                    // Ein Mitglied, das (noch) nicht im Katalog liegt, zählt nicht.
                    content("trip", &["c", "missing"]),
                ],
            )
            .unwrap();

        let albums = catalog.albums().unwrap();
        let ids: Vec<_> = albums.iter().map(|a| a.id.as_str()).collect();
        assert_eq!(ids, ["Favorites", "2024", "trip"]);
        assert_eq!(albums[0].count, 2);
        // Titelbild: das neueste Foto mit Vorschau — "b" hat keine.
        assert_eq!(albums[0].cover, Some(PathBuf::from("/thumbs/a.jpg")));
        assert_eq!(albums[1].count, 0);
        assert_eq!(albums[2].parent.as_deref(), Some("2024"));
        assert_eq!(albums[2].count, 1);
    }
}
