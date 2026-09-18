# iCloud Photos – nativer Client für Omarchy

## Projektübersicht

Native Desktop-App für Omarchy (Arch/Hyprland/Wayland), die iCloud Photos
read-only synchronisiert. Fotos werden zunächst nur als Low-Res-Thumbnails
geladen; Full-Res wird erst beim Öffnen eines Fotos nachgeladen. Der
Full-Res-Cache ist auf ein festes Speicherlimit begrenzt und evictet nach
LRU (least recently used), damit die App nie unbegrenzt Plattenplatz frisst.

## Ziele v1 (Scope)

- [x] Read-only Zugriff auf iCloud Photos (kein Upload, kein Löschen in iCloud)
- [x] Periodischer Metadaten-Sync (kein Bulk-Download aller Originale)
- [x] Thumbnail-first UI, Full-Res on demand
- [x] Konfigurierbares Full-Res-Cache-Limit in GB mit LRU-Eviction
- [ ] Explizit NICHT in v1: Schreibzugriff, Shared Albums, Live Photos/Videos,
      Multi-Account

## Tech-Stack

| Bereich | Wahl | Begründung |
|---|---|---|
| UI-Toolkit | `gtk4-rs` + `libadwaita` | `GtkGridView` mit virtualisiertem `GListModel` — nötig für tausende Thumbnails ohne hohen Speicherverbrauch. Läuft sauber unter Hyprland/Wayland. |
| Async-Runtime | `tokio` | Standard, gute Integration mit `gtk::glib::spawn_future_local` |
| DB | `rusqlite` | Lokaler Foto-Katalog, keine Server-Abhängigkeit |
| iCloud-Zugriff | Python-Sidecar mit `pyicloud` (Fork `timlaing/pyicloud`, PyPI `pyicloud==2.7.0`) | Keine ausgereifte Rust-Lib für die private Photos-API verfügbar. PyO3-Embedding verworfen, da pyicloud synchron/blockierend ist und den GIL im selben Prozess blockieren würde. **Fork-Wahl revidiert** — siehe unten. |

### Warum nicht der `icloud-photos-downloader`-Fork?

Ursprünglich war `icloud-photos-downloader/pyicloud` gesetzt. Beim Anbinden
stellte sich heraus, dass er nicht mehr benutzbar ist:

- **`pyicloud-ipd`** ist bei 0.10.2 stehengeblieben und hängt an `future`,
  das `imp` importiert — seit Python 3.12 entfernt. Auf dieser Maschine läuft
  Python 3.14, der Import scheitert also schon beim Laden.
- **`icloudpd`** (1.32.3) wird inzwischen als vorkompiliertes Binary
  ausgeliefert und enthält kein importierbares Python-Paket mehr.
- **`timlaing/pyicloud`** (PyPI `pyicloud`, 2.7.0) wird gepflegt, läuft auf
  Python 3.14 und hat genau die benötigten Felder: `PhotoAsset.added_date`,
  `.asset_date`, `.dimensions`, `.filename`, `.id` sowie
  `resources['original'].checksum` für die Prüfsumme.
  Achtung: `pip install pyicloud` allein genügt nicht, `rich` ist eine
  faktisch harte Abhängigkeit (siehe `sidecar/requirements.txt`).

Der Fork bietet zusätzlich `PhotosService.iter_changes(since=...)` und
`sync_cursor` — ein serverseitiger Delta-Sync, der auch **Löschungen** meldet.
Das kann der `added_date`-Cursor prinzipiell nicht. Siehe offene Punkte.

## Architektur

```
iCloud (pyicloud) → Sync-Daemon (Python-Sidecar) → Katalog (SQLite)
                                                          ↓
                                                  Thumbnail-Cache (persistent)
                                                          ↓
                                                  UI (GtkGridView, Low-Res)
                                                          ↓ (on open)
                                                  Full-Res-Cache (LRU, Limit in GB)
```

### Sync-Daemon (Python-Sidecar)

- Separater Subprozess, von Rust via `tokio::process::Command` gestartet
  (nicht dauerhaft laufend — pro Sync-Intervall neu gestartet)
- Kommunikation: NDJSON über stdout, ein JSON-Objekt pro Foto-Metadatensatz
  ```json
  {"id": "...", "filename": "IMG_1234.HEIC", "added_date": "...", "asset_date": "...", "checksum": "...", "width": 4032, "height": 3024, "item_type": "image"}
  ```
- Delta-Sync: nur Assets mit `added_date` > letztem gespeicherten Cursor abfragen
- 2FA: beim ersten Lauf interaktiv — Rust-UI fragt Code ab, gibt ihn über
  stdin an den Sidecar-Prozess weiter. Session wird danach über
  Cookie-Datei/Keyring persistiert (Ablauf nach einigen Stunden einplanen,
  dann erneuter interaktiver Login nötig)
- Kein Bild-Download in diesem Schritt — nur Metadaten

### Datenmodell (SQLite)

```sql
CREATE TABLE photos (
    id              TEXT PRIMARY KEY,
    master_id       TEXT,               -- Schema 3
    filename        TEXT NOT NULL,
    added_date      TEXT NOT NULL,
    asset_date      TEXT,
    checksum        TEXT,
    width           INTEGER,
    height          INTEGER,
    item_type       TEXT,               -- Schema 4: "image" | "movie"
    thumb_path      TEXT,
    thumb_bytes     INTEGER,
    orig_path       TEXT,
    orig_bytes      INTEGER,
    is_orig_cached  INTEGER NOT NULL DEFAULT 0,
    last_accessed_at TEXT
);

CREATE INDEX idx_photos_last_accessed ON photos(last_accessed_at);
CREATE INDEX idx_photos_added_date ON photos(added_date);
CREATE INDEX idx_photos_capture
    ON photos(COALESCE(asset_date, added_date) DESC, id DESC);
```

Migrationen laufen über `Catalog::add_column`; SQLite kennt kein
`ADD COLUMN IF NOT EXISTS`, ein "duplicate column" ist deshalb der Normalfall
und kein Fehler. `crates/cache/tests/migration.rs` baut eine Schema-3-Datei
nach und prüft den Aufstieg.

### Reihenfolge der Bibliothek

Sortiert wird nach **`COALESCE(asset_date, added_date)`**, dem Aufnahme- statt
dem Hinzufügedatum — so wie macOS Photos es hält. An der echten Bibliothek
gemessen weichen die beiden bei 427 von 6.098 Fotos um mehr als 30 Tage ab;
das erste Foto im Raster war vorher eine Aufnahme von 2019, die 2026 aus einem
Backup dazukam. Der Ausdruck muss wörtlich dem Index `idx_photos_capture`
entsprechen, sonst sortiert SQLite die ganze Tabelle statt den Index zu lesen
(`EXPLAIN QUERY PLAN` zeigt sonst ein `USE TEMP B-TREE`).

`item_type` kommt vom Sidecar. Bestandszeilen haben es nicht, und ein
Vollabgleich nur deshalb wäre teuer — `PhotoRecord::kind` fällt darum auf die
Dateiendung zurück. Die 407 Videos im Katalog sind so auch ohne Re-Sync als
Videos erkennbar.

### Cache-Strategie

**Thumbnail-Cache**
- Persistent, kein aktives Limit (Thumbnails sind klein, ~20–50 KB/Stück)
- Wird beim Sync für jedes neue Asset im Hintergrund nachgeladen

**Full-Res-Cache**
- Festes Limit in GB, konfigurierbar (`config.toml`, siehe unten)
- Eviction-Trigger: nach jedem Full-Res-Download Summe aller
  `orig_bytes WHERE is_orig_cached = 1` prüfen
- Bei Überschreitung: Zeilen `ORDER BY last_accessed_at ASC` löschen
  (Datei entfernen + `is_orig_cached = 0`, `orig_bytes = NULL`), bis Summe
  bei **90 % des Limits** liegt (Hysterese — verhindert Eviction-Thrashing
  bei jedem einzelnen neuen Foto)
- **Wichtig:** `last_accessed_at` muss auch beim reinen Ansehen eines
  bereits gecachten Fotos aktualisiert werden, nicht nur beim Download —
  sonst verhält sich der Cache wie FIFO statt echtem LRU
- Umgekehrt gilt: ein **Prefetch ist kein Zugriff**. Der Viewer lädt die
  Nachbarn des offenen Fotos im Voraus; die bekommen über
  `set_original_prefetched` keinen Zeitstempel und fliegen damit als erste
  wieder raus (`lru_candidates` sortiert `NULL` nach vorn). Ohne diese
  Trennung stünde ein nie angesehenes Foto vor einem betrachteten
- Zusätzlicher periodischer Housekeeping-Check (z.B. alle 10 Min) als
  Sicherheitsnetz gegen verwaiste Dateien nach Abstürzen

## Config (`~/.config/icloud-photos/config.toml`)

```toml
[icloud]
username = ""
sync_interval_minutes = 15

[cache]
full_res_cache_limit_gb = 5
eviction_target_ratio = 0.9

[paths]
thumbnail_dir = "~/.cache/icloud-photos/thumbnails"
full_res_dir = "~/.cache/icloud-photos/fullres"
db_path = "~/.local/share/icloud-photos/catalog.sqlite"
```

## Vorgeschlagene Projektstruktur

```
icloud-photos/
├── Cargo.toml                 # Workspace
├── config.example.toml        # Vorlage für ~/.config/icloud-photos/config.toml
├── crates/
│   ├── core/                  # Config, Foto-Modell — von allen anderen genutzt
│   ├── app/                   # GTK4-UI: window (Raster), viewer, info_panel, export
│   ├── sync/                  # Sync-Orchestrator, spawnt Sidecar, füllt SQLite
│   └── cache/                 # Katalog (SQLite), LRU-Eviction, Housekeeping
├── sidecar/
│   ├── pyicloud_sync.py       # Python-Skript mit pyicloud, gibt NDJSON aus
│   └── requirements.txt
├── data/                      # .desktop-Vorlage und App-Symbol
├── scripts/                   # install.sh / uninstall.sh für den Launcher
└── CLAUDE.md
```

`crates/core` kam dazu, weil Config und Foto-Modell von `cache`, `sync` und
`app` gebraucht werden; sie in `cache` zu legen hätte den Crate-Namen zur Lüge
gemacht.

## Protokoll Sidecar → App

Zeilenweises JSON auf stdout, jede Zeile mit `type`. stdout gehört allein dem
Protokoll, alles Menschenlesbare geht nach stderr.

```json
{"type": "password_required", "username": "..."}
{"type": "auth_required", "method": "2fa"}
{"type": "terms_required", "url": "https://www.apple.com/de/legal/internet-services/icloud/"}
{"type": "auth_ok", "trusted": true}
{"type": "photo", "id": "...", "filename": "IMG_1234.HEIC", "added_date": "...", "asset_date": "...", "checksum": "...", "width": 4032, "height": 3024, "item_type": "image"}
{"type": "thumb", "id": "...", "path": "...", "bytes": 34567}
{"type": "thumb_failed", "id": "...", "message": "..."}
{"type": "done", "count": 1234, "cursor": "2026-01-02T03:04:05+00:00"}
{"type": "error", "kind": "auth", "message": "..."}
{"type": "album", "id": "...", "name": "...", "kind": "smart|album|folder", "parent": "...", "position": 3}
{"type": "album_members", "id": "...", "size": 49, "members": ["...", ...]}
{"type": "album_unchanged", "id": "..."}
{"type": "albums_done", "count": 155}
```

Im Serve-Modus (`--serve`) kommen Befehle als JSON-Zeilen über stdin:
`{"cmd": "original", "id": ..., "dest": ...}` und `{"cmd": "quit"}`.

`original_failed` trägt ein `kind`: **`missing`** heisst, das Asset gibt es
nicht oder es hat kein Original — ein zweiter Anlauf wäre vergeblich. Alles
andere ist `error`: Netzfehler, Ausnahme im Lookup, abgelaufene Sitzung. Die
App wirft dann die Sitzung weg und meldet sich neu an. Ohne diese
Unterscheidung kostete jedes fehlende Asset eine überflüssige Neuanmeldung.

Der Sidecar setzt beim Start `PR_SET_PDEATHSIG`: Wird die App geschlossen,
ohne aufzuräumen, schickt der Kernel ihm SIGTERM. Vorher lief ein Sidecar, der
gerade in einer langen Anfrage steckte, verwaist weiter. Das Signal hängt am
*Thread*, der ihn gestartet hat — beide Sidecar-Arten starten aus eigenen,
langlebigen Threads (Sync bzw. Downloader), das passt.

Bricht die Leitung ab — die App wurde geschlossen, ein Sync abgebrochen, der
Sidecar nach einem Fehler beendet —, endet der Lauf still mit Exit-Code 3
(`EXIT_PIPE`) und einer Zeile auf stderr. Ohne das schreibt `emit` in eine
geschlossene Pipe, der Fehlerbehandler versucht dasselbe gleich noch einmal,
und im Terminal steht ein doppelter `BrokenPipeError`-Traceback. stdout wird
dabei auf `/dev/null` umgelegt, sonst meldet der Interpreter beim Beenden
denselben Fehler ein drittes Mal — dann unabfangbar.

Auf `password_required` bzw. `auth_required` erwartet der Sidecar das Passwort
bzw. den 2FA-Code als eine Zeile auf stdin — nie über `argv`, denn
Kommandozeilen sind in `/proc` für jeden Prozess des Nutzers lesbar. Nach
erfolgreicher Anmeldung legt der Sidecar das Passwort im Keyring ab
(`pyicloud://icloud-password`), sodass kein weiterer Lauf danach fragt.

Ändert Apple die iCloud-Nutzungsbedingungen, lässt iCloud keine Anmeldung
mehr zu (`PyiCloudAcceptTermsException`). pyicloud könnte sie per
`accept_terms=True` einfach akzeptieren — das tut der Sidecar bewusst nicht
ungefragt, denn die Zustimmung gilt für das ganze Apple-Konto. Er meldet
`terms_required` und wartet auf `accept` auf stdin; die App fragt per Dialog
mit Verweis auf den Text. Alles andere, auch ein fehlender `Prompts::terms`-
Kanal, ist eine Ablehnung (`SyncError::TermsDeclined`). Die API liefert nur
eine Versionsnummer, keinen Text, daher der feste Verweis. Stößt pyicloud erst
nach der 2FA auf die Bedingungen, wird nur `trust_session` wiederholt — über
das nicht-öffentliche `_accept_terms`. `--fake-terms` bzw.
`ICLOUD_PHOTOS_FAKE_TERMS=1` spielt den Weg ohne Apple-Konto durch.

`--fake N` erzeugt N synthetische Datensätze ohne iCloud-Zugriff und ohne
Import von `pyicloud`; damit läuft die komplette Rust-Seite im Test gegen einen
echten Subprozess. Die erzeugten `added_date` sind bewusst **unsortiert**, weil
die echte API das auch ist — ein Sidecar, der Sortierung voraussetzt, fällt im
Test auf.

## Sync-Strategie

`photos.all` ist **nicht** nach `added_date` sortiert — an einer echten
Bibliothek gemessen springen die Daten zwischen aufeinanderfolgenden Assets
(rowid 1: 2024-06, rowid 3: 2023-01, Ende: 2022-07). Sortiert ist das Album
nach `asset_date` aufsteigend (1748 von 1756 Paaren). Ein `added_date`-Cursor
als Abbruchbedingung ist deshalb unbrauchbar.

Der Abgleich läuft stattdessen so, wie Apples eigene Clients es tun — über den
CloudKit-**syncToken** der Foto-Zone:

| Schicht | Wann | Kosten bei 17.000 Fotos |
|---|---|---|
| 1. Token-Vergleich (`sync_cursor`) | jeder Sync | **1 Request**, wenn unverändert |
| 2. `iter_changes(since=token)` | Token geändert | proportional zur Zahl der Änderungen |
| 3. Vollständiger Durchlauf | Erstaufbau, abgelehnter Token, 24-h-Abgleich | ~170 Seitenabrufe, einmalig |

Zustand liegt in `sync_state` (einzeilig): `zone_sync_token`, `baseline_done`,
`baseline_offset`, `last_baseline_at`, `last_sync_at` — letzteres für die
Anzeige „zuletzt aktualisiert", die jeden Lauf zählt, nicht nur vollständige —
und `baseline_gap`, die hingenommene Bestandslücke.

**Zwei Regeln, an denen die Korrektheit hängt:**

- Der Token wird **vor** dem Durchlauf geholt. Änderungen während des Laufs
  kommen dann beim nächsten Mal erneut — andersherum gingen sie verloren.
- Der Token wird **nur übernommen**, wenn der lokale Stand ihn verdient:
  unverändert, vollständiger Durchlauf beendet, oder sauberer Delta-Lauf. Nach
  einem abgebrochenen Erstaufbau wäre er eine Lüge — der nächste Lauf zöge ein
  Delta und sähe den Rest der Bibliothek nie.

Ein durch `--limit` abgeschnittener Durchlauf meldet deshalb bewusst kein
`baseline_complete`, sondern nur seinen `offset`.

Einmal pro Woche läuft trotzdem ein vollständiger Abgleich (`RECONCILE_AFTER`)
— Sicherheitsnetz gegen langsames Abdriften. Öfter lohnt nicht: bei 19.646
Assets sind das rund 170 Seitenabrufe, während der laufende Abgleich über den
Token bei unveränderter Bibliothek einen einzigen Request kostet.

### Die Bestandslücke

`photos.all` zählt Dinge mit, die nie als Asset ankommen — an einer echten
Bibliothek mit 19.646 gezählten Assets fehlten dauerhaft 77. Daraus wurde eine
Schleife: Durchlauf fertig → Zahlen passen nicht → Erstaufbau-Merker zurück →
nächster Lauf fängt von vorn an. Bei **jeder** Synchronisation, für immer; für
zehn neue Fotos lief die App durch die ganze Bibliothek.

Gemerkt wird deshalb in `sync_state.baseline_gap` (Schema 7), wie groß die
Lücke nach einem vollständigen Durchlauf war. Was danach *darüber hinaus*
fehlt, ist eine verpasste Änderung und erzwingt einen neuen Durchlauf — die
bekannte Lücke selbst nicht mehr. Nur ein Delta-Lauf darf das auslösen: ein
abgeschnittener Durchlauf hat schlicht noch nicht alles gesehen und verwürfe
sonst seinen eigenen Fortschritt.

## Alben

`--albums` meldet alle Alben aus `api.photos.albums`: die intelligenten
(Favorites, Videos, Live, Panoramas, Screenshots, Slo-mo, Time-lapse, Bursts —
ohne Library, Hidden und Recently Deleted, deren Fotos nicht im Katalog
stehen), die eigenen Alben und die Ordner, samt `parent`. Danach je Album
seine Mitglieder. Der Katalog hält sie in `albums` und `album_photos`
(Schema 9) und ersetzt beide erst bei `albums_done` in einer Transaktion — ein
abgebrochener Lauf lässt den alten Stand ganz stehen.

An der echten Bibliothek: 155 Alben, knapp 39.000 Zugehörigkeiten, **knapp
fünf Minuten** mit sechs parallelen Alben (`ALBUM_WORKERS`). Das darf nicht
bei jedem Sync anfallen:

- Der Album-Lauf hängt am Zone-Token (`sync_state.albums_token`). Stimmt er
  mit dem Stand der letzten Album-Runde, kostet er nichts. Ohne Token (der
  Erstaufbau läuft noch) wartet er, sonst fehlten den Alben die Fotos.
- Der Token ändert sich aber mit **jedem** neuen Foto. Deshalb schickt die App
  nach `auth_ok` die gemerkten Größen (`albums.size`) als JSON-Zeile. Der
  Sidecar fragt je Album nur `len()` ab (eine Anfrage) und meldet bei gleicher
  Größe `album_unchanged` — die App übernimmt dann die gespeicherten
  Mitglieder. Ein Tausch bei gleicher Größe fällt so durch; das ist der Preis.

Die Seitenleiste steckt in einer `AdwOverlaySplitView`, nicht in einer
`NavigationSplitView` — nur jene lässt sich auch im breiten Fenster
ausblenden (Knopf ganz links in der Kopfzeile, F9). Unter 600 sp legt sie sich
über den Inhalt und schließt nach einer Wahl von selbst.

Die Seitenleiste (`crates/app/src/sidebar.rs`) ist aufgebaut wie in Photos:
Library, Collections, dann „Pinned" (Favorites, Videos), „Albums" als Baum
mit auf- und zuklappbaren Ordnern und „Media Types". Leere intelligente Alben
fehlen. Ordner zeigen nichts selbst, sie klappen nur auf. „Collections"
(`crates/app/src/collections.rs`) ist eine Übersicht aus Karten mit Titelbild
(neueste Vorschau des Albums) und Anzahl; ein Ordner öffnet sich dort mit
Zurück-Knopf.

Ein gewähltes Album ist ein `Scope` des Katalogs: `page_in`, `count_in` und
`month_groups_in` hängen dann einen Join auf `album_photos` an. Die Bibliothek
bekommt bewusst keinen — nur so liest SQLite ihre Seiten direkt aus
`idx_photos_capture`. Skelette gibt es nur in der Bibliothek; in einem Album
weiß niemand, wie viele Fotos noch hinein gehören.

## Full-Res-Download: warum nicht `PhotoAlbum.get`

`PhotoAlbum.get(id)` sieht nach einer gezielten Abfrage aus, fällt intern aber
auf `for photo in self.photos` zurück, wenn seine Query nichts findet — ein
vollständiger Durchlauf der Bibliothek. An ~19.600 Fotos gemessen:

| Schritt | über `PhotoAlbum` | über `records/lookup` |
|---|---|---|
| Sitzungsaufbau | 8,6 s (`photos.all`) | 1,0 s (`_root_library`) |
| Asset auflösen | **15,2 s** | **0,3–0,8 s** |
| Download (~1–2 MB) | 0,6 s | 0,6 s |
| **Gesamt, erstes Foto** | **~25 s** | **~4 s** |

Der Download war nie das Problem — das Auflösen der ID war es. `AssetResolver`
im Sidecar geht deshalb direkt über `records/lookup` und baut den `PhotoAsset`
aus Asset- und Master-Record selbst zusammen.

Dafür werden **nicht-öffentliche** Teile von pyicloud benutzt
(`_root_library`, `private_client`). Jeder Fehlschlag fällt deshalb auf den
Album-Weg zurück — langsam, aber funktionierend.

Ist `master_id` im Katalog bekannt (seit Schema 3 mitgeführt), genügt ein
einziger Lookup statt zwei.

## Wiederaufnahme

Ein abgebrochener Sync wird vom nächsten Lauf vervollständigt:

- Metadaten werden per Upsert geschrieben; Cache-Spalten bleiben unangetastet.
- Vorschauen, deren Datei schon existiert, werden nicht erneut geladen.
- Geschrieben wird über `.part` + `rename`, eine abgebrochene Datei gilt also
  nie als gültiger Treffer.
- Ein abgebrochener Erstaufbau setzt bei `baseline_offset` fort, statt von
  vorn zu paginieren. Der Fortschritt wird alle 50 Assets festgehalten.

Der Einstieg bei `baseline_offset` muss **serverseitig** sein
(`photos_from` im Sidecar). `itertools.islice` über `photos.all` holt jede
Seite davor und wirft sie weg — bei ~3 s je 100 Assets hing ein bei 8.700
fortgesetzter Erstaufbau bei jedem Start viereinhalb Minuten, ohne ein Foto zu
liefern; wer die App vorher schloss, kam nie weiter. `photos_from` baut die
Schleife aus `PhotoAlbum.photos` mit anderem Startrang nach (`photos.all` läuft
**absteigend**: Position 0 ist Rang `len − 1`) und fällt ohne die
nicht-öffentlichen `_get_photos_at`/`_direction` auf `islice` zurück.

Vorschauen lädt der Erstaufbau mit acht Threads (`THUMB_WORKERS`): einzeln
~2 Assets/s, parallel ~12,5 — sonst dauerten 10.000 Fotos über eine Stunde.
Ausgegeben wird nur aus dem Hauptthread und in Reihenfolge; `offset` erst,
wenn alles davor samt Vorschau geschrieben ist.

## Bedienung (Phase 2)

**Betrachter** (`crates/app/src/viewer.rs`) — keine eigene Fensterebene,
sondern eine **Seite desselben Fensters**: ein geöffnetes Foto füllt das
Fenster, das Raster tritt dahinter zurück. Ein zweites Fenster wäre unter
Hyprland fehl am Platz — es landete als eigene Kachel neben der App statt über
ihr. Die Seite kennt ihre Position im Raster, nicht das einzelne Foto; nur so
lässt sich blättern, ohne zwischendurch ins Raster zurückzumüssen.

Drei Fallen dieser Bauweise:

- Die Seite wird beim Schließen wieder **ausgehängt** und beim nächsten Öffnen
  neu gebaut. Bleibt sie stehen, meldet GTK „duplicate child name in GtkStack"
  und zeigt beim zweiten Öffnen die alte Seite. `open` entfernt deshalb
  zusätzlich eine etwaige Restseite, bevor es die neue einhängt — der Stack
  gibt die alte während einer laufenden Blende nicht immer sofort los.
- Die **Tastatursteuerung hängt am Fenster**, nicht an der Seite: solange der
  Fokus im Raster steht, bekäme die Seite sonst keine Taste. Sie läuft in der
  Erfassungsphase und gibt jede Taste weiter, solange der Betrachter nicht die
  sichtbare Seite ist. Beim Schließen wird sie wieder abgenommen, sonst sammelt
  das Fenster mit jedem geöffneten Foto eine weitere an.
- Die **Aktionsgruppe `viewer`** (Sichern, Kopieren, Öffnen) muss an der Seite
  hängen, **bevor** die Seite in den Stack kommt. Später eingehängt, löst GTK
  die Aktionen zwar noch auf, das Menü hinter ⋮ erfährt davon aber nichts und
  zeigt alle Einträge ausgegraut. Aktionen, die erst danach in die schon
  eingehängte Gruppe kommen, bemerkt es dagegen.
- Zurück im Raster wird auf die Position des zuletzt gezeigten Fotos gescrollt
  (`AppState::return_to_grid`) — sonst steht man nach dem Schließen irgendwo.

| Taste | Wirkung |
|---|---|
| ← → ↑ ↓ | ein Foto zurück/vor |
| Bild ↑/↓ | zehn Fotos |
| Pos1 / Ende | erstes / letztes Foto |
| Mausrad, Strg + Touchpad-Scrollen | zoomen, am Zeiger |
| Pinch auf dem Touchpad | zoomen und verschieben, den Fingern nach |
| `+` `−` | zoomen, um die Fenstermitte |
| `0` / `1` | einpassen / 1:1 |
| Doppelklick | eingepasst ↔ 1:1 an der geklickten Stelle |
| Ziehen, Touchpad-Scrollen | Ausschnitt verschieben (nur gezoomt) |
| `F` / F11 | Vollbild |
| `I` | Info-Panel ein/aus |
| Esc | übergelegtes Info-Panel schließen, sonst Vollbild verlassen, sonst zurück zur Übersicht |

Gezoomt wird in einem eigenen Widget (`crates/app/src/zoom_view.rs`), nicht
mehr über `GtkPicture` in einem `GtkScrolledWindow`. Dort stehen die
Scrollgrenzen erst nach der nächsten Zuteilung fest; der Ausschnitt wurde
geraten und sprang, und im eingepassten Zustand rechnete der Anker die Ränder
um das Bild nicht mit. `ZoomView` führt Maßstab (relativ zur Einpassung) und
Mittelpunkt (normiert auf 0…1) selbst und zeichnet die Textur direkt. Beides
überlebt eine Größenänderung und den Wechsel von der Vorschau aufs Original.
Die Rechnung liegt GTK-frei in `View` und ist getestet. Das Touchpad
unterscheidet sich über `EventControllerScroll::unit()`: Mausräder liefern
Rasterungen und zoomen, das Touchpad liefert Pixel und verschiebt — früher
zoomte jedes seiner Dutzenden Ereignisse pro Wischer um einen vollen Schritt.

Solange das Original noch lädt, zeigt der Betrachter die Vorschau hochskaliert
— eine unscharfe Fläche ist besser als eine leere. Videos (`item_type ==
"movie"` bzw. Videoendung) laufen über `GtkVideo`; GTK 4.22 bringt den
GStreamer-Rückfall als `GtkGstMediaFileBuiltin` mit, ein Media-Modul muss nicht
installiert sein. Beim Blättern wird das Video angehalten, sonst spielte es im
Hintergrund weiter.

**Info-Panel** (`crates/app/src/info_panel.rs`, ⓘ in der Kopfzeile oder `I`)
— rechts neben dem Foto, wie in Photos: Titel (Ortsname, sonst Dateiname),
Aufnahmezeit, dann Details (Dateiname, Typ, Maße mit Megapixeln, Dateigröße),
Kamera (Modell, Objektiv, Brennweite samt Kleinbild-Äquivalent, Blende,
Belichtungszeit, ISO, ausgelöster Blitz) und Ort (Name, Koordinaten). Zeilen
ohne Wert fehlen, ebenso ganze Gruppen. Es steckt in einer
`AdwOverlaySplitView` (Seitenleiste rechts), die unter 720 sp über dem Foto
liegt statt daneben. Der Umbruch hängt an einer eigenen `AdwBreakpointBin` in
der Seite, nicht am Fenster — die Seite wird bei jedem Öffnen neu gebaut, ein
Breakpoint am Fenster hielte die alte Teilung fest. Offen oder zu bleibt beim
Blättern und über das Schließen des Betrachters hinweg (`INFO_OPEN`).

**EXIF kommt aus dem Original**, nicht aus dem Katalog: iCloud liefert mit den
Metadaten weder Kamera noch Belichtung, und die JPEG-Vorschau trägt kein EXIF.
Gelesen wird die Datei im Full-Res-Cache mit `kamadak-exif` (reines Rust, kann
JPEG und den HEIF-Container; der Test baut ein HEIC per `heif-enc` und liest es
zurück). Die Formatierung liegt GTK-frei in `crates/app/src/exif_info.rs` und
ist getestet. Gelesen wird in `gio::spawn_blocking`; eine Zählnummer verwirft
Ergebnisse, die erst nach dem Weiterblättern eintreffen. Fehlt das Original
noch, zeigt das Panel den Katalogstand und „Camera details appear when the
original has loaded" und liest nach, sobald `Viewer::deliver` es bringt.
Geschlossen arbeitet das Panel gar nicht — sonst kostete jedes Blättern eine
Katalogabfrage und einen Dateizugriff, den niemand sieht. Videos zeigen nur
den Katalogstand; ihre Metadaten stecken im Container und bräuchten GStreamer.

**Raster** — Die Kachel ist nur das Bild: **kein Dateiname darunter**. Er sagt
über ein Foto nichts, was man nicht sieht, kostet aber in jeder Reihe eine
Zeile Höhe; wer ihn braucht, findet ihn im Tooltip und im Betrachter. Dazu
Mehrfachauswahl (`GtkMultiSelection`), das Video-Kennzeichen als Overlay auf
der Kachel (nicht daneben, sonst verschöbe es das Raster) und eine mitlaufende
Datumszeile über dem Raster. `GtkGridView` kennt keine
Abschnittsköpfe — nur `GtkListView` hat eine `header_factory`. Die Datumszeile
liest deshalb den Monat der obersten sichtbaren Kachel ab, die ihn in ihrem
Widget-Namen trägt; die Virtualisierung bleibt unangetastet. Findet sie nichts
(direkt nach einem Sprung sind die Kacheln am Ziel noch nicht zugeteilt),
bleibt die letzte Angabe stehen, statt zu verschwinden.

**Sync-Manager** (`crates/app/src/sync_manager.rs`) — Bibliothek und
Synchronisation sind getrennt. In der Kopfzeile der Bibliothek steht nur, wie
viele Fotos da sind (`1,234 Photos`), und ein Knopf, der den Manager öffnet
(Strg+Umschalt+R). Der Knopf zeigt ein Drehsymbol, solange ein Lauf arbeitet,
und ein Warnzeichen, wenn der letzte gescheitert ist und noch niemand
hineingesehen hat.

Im Manager (ein `AdwDialog`, kein zweites Fenster — unter Hyprland landete das
als Kachel neben der App) stehen Phase und Balken des laufenden Laufs,
„Sync Now", der letzte Fehler, Bestand lokal und in iCloud, die
Zeitpunkte des letzten und des letzten vollständigen Abgleichs, Apple-ID sowie
die Belegung von Vorschauen und Originalen — je mit einem Knopf, der den Ordner
im Dateimanager öffnet (`gtk::FileLauncher`; ein noch fehlender Ordner wird
angelegt, statt dass der Knopf wortlos nichts tut).

Früher lag der Fortschritt als Streifen über der Bibliothek und ein
gescheiterter Sync öffnete einen Fehlerdialog — beides riss aus dem
Betrachten. Ein Fehlschlag meldet sich jetzt als Toast mit „Details", der den
Manager öffnet. Nur abgelehnte Zugangsdaten bekommen weiter einen Dialog, weil
sie ein Handeln verlangen.

**Raster während eines Syncs** — die Bibliothek muss benutzbar bleiben,
während Fotos einlaufen. Drei Regeln:

- **Nie das ganze Modell ersetzen.** `apply_diff` spielt nur Einfüge- und
  Entfernschritte ein; unveränderte Fotos bleiben dasselbe `PhotoObject`. Nur
  so behält GTK Scrollhalt und Auswahl. Früher wurde alle zwei Sekunden das
  komplette Modell ausgetauscht — jede Kachel neu gebaut, Position und Auswahl
  weg, und nach dem Sync sprang die Ansicht ganz nach oben.
- **Eine Vorschau ist keine Strukturänderung.** `thumb-path` ist eine
  beobachtbare Eigenschaft; trifft eine Vorschau ein, zeichnet nur ihre Kachel
  neu.
- **Umgebaut wird nur, wenn der Nutzer oben steht** (`refresh_live`). Neue
  Fotos fallen meist *oberhalb* des Sichtbereichs ein — beim Erstaufbau fast
  alle, weil iCloud alt vor neu liefert. GTK hält dabei zwar das Foto fest, im
  Raster fließt aber alles um so viele Spalten weiter, wie oben dazukam. Wer
  weiter unten liest, bekommt deshalb nur Vorschauen; der Umbau wartet, bis er
  wieder oben ist oder der Sync endet. Wer oben steht, bleibt oben
  (`scroll_to(0)`) und sieht die neuen Fotos einlaufen.
- **Bei offenem Betrachter wird gar nicht umgebaut.** Das Raster sieht dann
  niemand, ein Umbau hielte nur den Hauptthread an — und ein laufendes Video
  stockte bei jedem Takt. `model_dirty` bleibt stehen und wird nach dem
  Schließen nachgeholt.

`last_seen_count` startet mit dem Katalogbestand, nicht mit 0. Sonst hielt der
erste `sync_model` jedes Foto für neu, lud den ganzen Katalog ins Modell
(gemessen: 8.935 Zeilen, 200–600 ms im Hauptthread) und jeder weitere Abgleich
danach ebenso.

Vorschauen werden in einem Hilfsthread dekodiert (`gio::spawn_blocking`) und
die letzten 200 im Speicher gehalten (~140 MB bei 360×480). Eine Kachel merkt
sich über ihren Widget-Namen, welches Bild sie erwartet — ist sie inzwischen
recycelt, verfällt das Ergebnis.

Drei Dinge, an denen die Fortschrittsanzeige im Manager sonst lügt:

- **Ein fortgesetzter Erstaufbau muss als solcher erkennbar sein.** Der Balken
  bekommt den `baseline_offset` als Sockel (`SyncMessage::Started.resuming`),
  sonst fängt er bei null an, obwohl der halbe Katalog schon steht. Der Titel
  sagt dann „Loading Photos · Resumed".
- **Metadaten und Vorschauen zählen dieselben Assets**, nur unterschiedlich
  fein: `Progress` kommt alle 256 Datensätze (`BATCH_SIZE`), `Thumbnails` pro
  Bild. Beide speisen denselben Zähler, und der höhere Stand gewinnt — sonst
  zuckt die Anzeige zwischen grob und fein hin und her.
- **Ein Delta-Lauf hat keine Bezugsgröße.** `full_pass` unterscheidet das; ohne
  vollständigen Durchlauf gibt es nur einen wandernden Balken und einen
  Zähler, kein „x / y" gegen den Gesamtbestand.

Der Nachlauf für fehlende Vorschauen ist eine eigene Phase
(`SyncMessage::BackfillStarted`) mit eigener Bezugsgröße — seine bis zu 500
Bilder haben mit dem Bibliotheksbestand nichts zu tun.

„Last Synced" im Manager liest `sync_state.last_sync_at` (Schema 5) — `last_baseline_at` taugt nicht, das
steht nur nach einem vollständigen Durchlauf, und den gibt es alle 24 Stunden
einmal.

**Aktionen** (`crates/app/src/export.rs`) — Sichern, Kopieren und Öffnen
brauchen alle dasselbe: die Originale der Auswahl. Die liegen aber nur im
Cache, wenn sie schon einmal geöffnet wurden. Jede Aktion ist deshalb ein
`Job`, der auf eintreffende Downloads wartet und erst am Ende ausgeführt wird.
Dateinamen aus iCloud sind nicht eindeutig (`IMG_0001.HEIC` gibt es mehrfach),
deshalb weicht `save_into` auf `IMG_0001-1.HEIC` aus, statt zu überschreiben.

Beim **Sichern** wird zuerst das Format gewählt — Original, JPEG, PNG oder
HEIC; die letzte Wahl ist vorausgewählt. Umgewandelt wird über die Werkzeuge
von libheif, das ohnehin Laufzeitabhängigkeit ist: `heif-dec` für HEIC → JPEG/
PNG, weil es EXIF (Aufnahmezeit, Ort) mitschreibt und die Drehung in die Pixel
übernimmt; `heif-enc` für JPEG/PNG → HEIC. JPEG ↔ PNG erledigt gdk-pixbuf.
Liegt ein Foto schon im Zielformat vor, wird es nur kopiert — ein zweites
Kodieren kostete Qualität. Videos bleiben immer im Original. Das Sichern läuft
in `gio::spawn_blocking`, sonst stünde die App bei hundert HEICs sekundenlang;
eine gescheiterte Umwandlung hinterlässt keine halbe Datei.

| Kürzel | Wirkung |
|---|---|
| Strg+A | alles auswählen |
| Strg+S | Auswahl in ein Verzeichnis sichern |
| Strg+C | Auswahl als Dateiliste in die Zwischenablage |
| Strg+O | mit der Standardanwendung öffnen (nur bei genau einem Foto) |
| Strg+J | Monatsliste zum Springen |
| Strg+R / F5 | jetzt synchronisieren |
| Strg+Umschalt+R | Sync-Manager öffnen |

Strg+J gibt es, weil `GtkGridView` die Tabulatortaste für seine Kacheln behält
— die Kopfzeile ist von der Auswahl aus sonst gar nicht erreichbar.

## Offene Punkte / TODO

- [x] **Start über den Launcher** (Super+Space) statt `cargo run`:
      `scripts/install.sh` legt Binary, `.desktop`-Eintrag und Symbol im
      Nutzerverzeichnis ab — siehe „Entwicklung".
- [x] **Anmeldung in der App**, kein Terminal nötig: Beim ersten Start zeigt die
      App eine Maske für Apple-ID und Passwort. Die Apple-ID landet in
      `config.toml` (0600), das Passwort geht über stdin an den Sidecar und von
      dort in den Keyring — nie in eine Datei. Lehnt Apple die Anmeldung ab,
      führt `SyncError::Credentials` zurück auf die Maske statt in einen
      generischen Fehler.
- [x] 2FA-UX: blockierender `AdwAlertDialog`, Code geht über stdin an den
      Sidecar. Notification verworfen — der Code ist nur Minuten gültig.
- [x] Periodischer Sync: beim Start einmal, danach im Takt von
      `sync_interval_minutes`. Ein laufender Sync verwirft Nachforderungen.
- [x] Entscheidung: Sidecar pro Sync-Lauf neu gestartet. Für Metadaten reicht
      das. **Aber:** für Full-Res on demand ist der Auth-Overhead pro Prozess
      zu hoch, dafür braucht es einen dauerhaften `--serve`-Modus.
- [x] **Thumbnail-Download** — `--thumbs DIR` lädt die Vorschau (`resJPEGThumb`)
      direkt beim Sync mit. Vorhandene Dateien werden übersprungen, geschrieben
      wird über `.part` + `rename`, damit ein Abbruch keine halbe Datei als
      gültigen Treffer hinterlässt.
      `--fetch-thumbs` holt Vorschauen für IDs von stdin nach — nötig, weil der
      Delta-Sync ältere Assets nicht erneut sieht. Aufgelöst wird über
      `AssetResolver` **ohne** Album-Rückfall: `photos.all.get` läuft bei
      jedem nicht gefundenen Asset die ganze Bibliothek durch, und ein
      Nachlauf für 13 Fotos hing so über zwei Stunden — samt allem dahinter. Die App hängt diesen
      Durchgang automatisch an jeden Sync an (max. 500 pro Lauf).
- [x] **Full-Res-Download auf Abruf.** Der Sidecar hat dafür einen dauerhaft
      laufenden `--serve`-Modus: Prozessstart plus Anmeldung kosten Sekunden
      und dürfen nicht bei jedem geöffneten Foto anfallen. Er wird erst beim
      ersten Auftrag gestartet — wer nie ein Foto öffnet, zahlt keine Anmeldung.
      Damit läuft die LRU-Eviction erstmals im Echtbetrieb: nach jedem Download
      wird das Limit durchgesetzt, wie in der Cache-Strategie festgelegt.
- [x] **Fotos laufen live ein.** Die Ansicht zieht alle 2 Sekunden nach,
      solange ein Sync läuft (`LIVE_REFRESH`), statt erst am Ende umzuschalten.
      Fotos ohne Vorschau und noch nicht synchronisierte Plätze erscheinen als
      Skelett-Kacheln, sodass das Raster von Anfang an seine spätere Form hat.
      Der Sidecar meldet dafür den Gesamtbestand (`library_count`) gleich zu
      Beginn eines Erstaufbaus — daher auch die Anzeige „1,234 / 19,624".
- [x] **Inkrementeller Sync über den CloudKit-Zone-Token** (siehe eigener
      Abschnitt). Löst auch Löschungen, die über `added_date` prinzipiell
      unsichtbar sind.
- [x] **Betrachter, Raster und Aktionen aus Phase 2** — siehe „Bedienung".
      Sortierung nach Aufnahmedatum (Schema 4), Blättern/Zoom/Vollbild,
      Videowiedergabe, Mehrfachauswahl, Sichern/Kopieren/Öffnen.
- [ ] `change_unresolved` beobachten: welche `record_type`-Werte in der Zone
      auftauchen, die kein Asset sind. Bislang nur gegen den Fake-Sidecar
      geprüft, nicht gegen die echte API.
- [x] **Abgelaufene Sitzung beim Full-Res-Download.** Der `--serve`-Prozess
      lebt so lange wie die App; lief seine Sitzung ab, meldete Apple das als
      gewöhnlichen Fehlschlag, und die tote Sitzung blieb stehen — ab da
      scheiterte *jedes* geöffnete Foto bis zum Neustart. Ein Fehlschlag
      verwirft die Sitzung jetzt und setzt einmal mit frischer Anmeldung neu
      auf; das deckt den kurzen Netzaussetzer gleich mit ab.
- [ ] Drag-and-drop aus dem Raster. Bräuchte einen `GdkContentProvider`, der
      den Download abwarten kann — ein Zug darf die Oberfläche nicht blockieren,
      und ein noch nicht geladenes Original hat keinen Pfad, den man mitgeben
      könnte.
- [x] Favoriten — über das intelligente Album „Favorites", nicht über ein Feld
      am `PhotoAsset` (das hat keins). Siehe „Alben".
- [x] Alben-Seitenleiste — siehe „Alben".
- [x] Info-Panel mit EXIF — siehe „Bedienung".
- [ ] Suche
- [ ] Alben ohne Titelbild: die Übersicht dekodiert ihre Titelbilder noch im
      UI-Thread (`Picture::set_filename`). Bei 160 Vorschauen à 30 KB unkritisch,
      bei tausend Alben nicht mehr.
- [x] Vorschauen im Hintergrund dekodieren — siehe „Raster während eines
      Syncs".
- [ ] `GListModel`-Implementierung für paginierte SQLite-Queries.
      Zwischenlösung: `gio::ListStore`, der beim Scrollen seitenweise nachlädt
      (`PAGE_SIZE = 200`) — der Katalog landet nie ganz im RAM, aber
      Zurückscrollen gibt Speicher nicht frei.
- [ ] `glycin` statt `gdk::Texture::from_filename`: würde die undeklarierte
      `libheif`-Abhängigkeit auflösen und das Dekodieren aus dem UI-Thread
      holen — zwei offene Punkte mit einem Schritt.

## Laufzeitabhängigkeiten

Neben GTK4 und libadwaita braucht die App **`libheif`**. Es ist in Arch nur eine
*optionale* Abhängigkeit von `gdk-pixbuf2`:

```
gdk-pixbuf2 → Optional Deps: libheif: Load .avif, .heic and .heif
```

Fehlt es, lädt das Raster weiter seine JPEG-Vorschauen, und der Betrachter
zeigt für jedes HEIC nichts — in einer typischen iPhone-Bibliothek sind das
drei von vier Fotos. Der Fehler ist von aussen nicht als Paketproblem zu
erkennen, deshalb benennt die Fehlerseite im Betrachter die Ursache.

**Videos** spielt GTK über GStreamer ab (`GtkGstMediaFileBuiltin`). Dessen
Grundausstattung (`gstreamer`, `gst-plugins-base`) reicht für iPhone-Videos
nicht: `.mov` braucht `qtdemux` aus **`gst-plugins-good`**, und iPhones filmen
in HEVC — dafür **`gst-libav`** (Software) und für flüssiges 4K
**`gst-plugin-va`** (Hardware, hier über `intel-media-driver`). Fehlt etwas,
meldet GTK „Your GStreamer installation is missing a plug-in." nur am Stream;
der Betrachter liest das ab und nennt die Pakete (`video_failure`), statt eine
schwarze Fläche mit totem Abspielknopf zu zeigen.

```
sudo pacman -S --needed gst-plugins-good gst-libav gst-plugin-va
```

Die App setzt **`GSK_RENDERER=gl`**, wenn der Nutzer nichts vorgibt
(`prefer_gl_renderer` in `main.rs`). iPhone-Videos sind meist 4K-HDR; der
VA-Decoder liefert sie als P010-DMABuf, und der Vulkan-Renderer — GTKs Vorgabe
— importiert die nicht direkt. Gemessen an einem Tiger Lake (Iris Xe): 4K-HDR
mit Vulkan ~6 Bilder/s, mit GL die vollen 30. 1080p (NV12) läuft mit beiden
flüssig, deshalb fällt es nur bei den großen Videos auf.

Die saubere Lösung wäre `glycin` als eigene Abhängigkeit statt des Umwegs über
den Pixbuf-Loader — das würde zugleich das Dekodieren aus dem UI-Thread holen.
Siehe offene Punkte.

## Entwicklung

```bash
# Einmalig: Python-Umgebung für den Sidecar
python3 -m venv .venv && .venv/bin/pip install -r sidecar/requirements.txt

# Tests (brauchen nur python3, kein Apple-Konto)
cargo test --workspace

# UI ohne Apple-Konto ausprobieren: erzeugt 240 Fotos mit Thumbnails
cargo run -p icloud-photos-app -- --demo

# Sync gegen synthetische Daten statt iCloud — erzeugt echte Thumbnail-Dateien
ICLOUD_PHOTOS_FAKE=500 cargo run -p icloud-photos-app

# Kompletten Anmeldeweg ohne Apple-Konto durchspielen (Passwort: geheim)
ICLOUD_PHOTOS_FAKE=40 ICLOUD_PHOTOS_FAKE_PASSWORD=1 cargo run -p icloud-photos-app

# Sidecar direkt, gegen die echte Bibliothek
.venv/bin/python sidecar/pyicloud_sync.py --username "$APPLE_ID" \
    --cookie-dir ~/.local/share/icloud-photos/cookies \
    --thumbs ~/.cache/icloud-photos/thumbnails --limit 5
```

### Installieren für den Launcher

```bash
scripts/install.sh      # baut --release und installiert; nach jedem Pull erneut
scripts/uninstall.sh    # entfernt Binary, Eintrag und Symbol wieder
```

`install.sh` kopiert das Binary nach `~/.local/share/icloud-photos/bin/icloud-photos`,
den Eintrag `data/de.henri.IcloudPhotos.desktop` (mit eingesetztem absolutem
Pfad) nach `~/.local/share/applications/` und das Symbol nach
`~/.local/share/icons/hicolor/scalable/apps/`; `XDG_DATA_HOME` wird beachtet.
Bewusst **nicht** nach `~/.local/bin` — dort liegen die per stow verwalteten
Skripte aus den Dotfiles. Kopie statt Symlink ins `target/`, damit ein
`cargo clean` den Launcher nicht still bricht. `uninstall.sh` lässt Katalog,
Cookies und Caches stehen.

Der Eintrag heisst nach der Application-ID und setzt
`StartupWMClass=de.henri.IcloudPhotos`: unter Wayland ist die `app_id` des
Fensters die GApplication-ID, nur so ordnen Launcher und Compositor Fenster und
Symbol dem Eintrag zu.

**Das Projekt darf danach nicht verschoben werden.** Das Binary findet Sidecar
und venv über den beim Bauen einkompilierten Projektpfad (`project_root()` in
`sync_task.rs`, aus `CARGO_MANIFEST_DIR`). Nach einem Umzug `install.sh` neu
ausführen oder `ICLOUD_PHOTOS_PYTHON`/`ICLOUD_PHOTOS_SIDECAR` setzen. Aus
demselben Grund nie aus einem Worktree installieren — das Skript warnt dann;
der Worktree wird weggeräumt, der Pfad im Binary bleibt. Fehlt `.venv`, bricht
`install.sh` ab: das Binary fiele sonst still auf das System-Python ohne
pyicloud zurück.

Umgebungsvariablen: `ICLOUD_PHOTOS_PYTHON` und `ICLOUD_PHOTOS_SIDECAR`
überschreiben Interpreter und Skriptpfad, `RUST_LOG` steuert das Logging.
`ICLOUD_PHOTOS_FAKE_DELAY_MS` bremst den Fake-Sidecar, damit das Einlaufen der
Kacheln überhaupt beobachtbar ist.

Der Testmodus läuft unter eigener Application-ID (`…​.Fake`, `NON_UNIQUE`).
Ohne das reicht GApplication die Aktivierung per D-Bus an eine bereits
laufende Produktivinstanz weiter und der Testlauf beendet sich wortlos.

Jedes neunte Fake-Asset ist ein `.MOV` mit `item_type: "movie"` — damit
Video-Kennzeichen und Betrachter-Zweig überhaupt einen Fall bekommen. Die Datei
dahinter ist ein PNG, der Betrachter zeigt für sie also seine Fehlerseite;
das ist gewollt und kein Defekt. Aufnahme- und Hinzufügedatum fallen im Fake
bewusst auseinander, sonst fiele eine Ansicht, die nach dem falschen der beiden
sortiert, im Test nicht auf.

Ein isolierter Lauf braucht eine eigene `config.toml` mit gesetzten `[paths]`
— `XDG_CONFIG_HOME` allein genügt nicht, weil die Vorgabepfade fest auf
`~/.cache` und `~/.local/share` zeigen und der Testlauf sonst in die echten
Verzeichnisse schreibt.

## Konventionen für Claude Code

- **Die Oberfläche ist englisch**, Code-Kommentare, Logs (`tracing`) und diese
  Datei bleiben deutsch. Zahlen mit Komma als Tausendertrenner (`thousands`),
  Datumsangaben englisch („September 18, 2026 at 14:03"), Ortsnamen per
  `accept-language=en` von Nominatim. Fehlermeldungen, die bis in die
  Oberfläche durchreichen — `SyncError`, `ConfigError`, `CacheError` und die
  `message`-Felder des Sidecars —, sind ebenfalls englisch.

- Rust: `rustfmt` + `clippy -- -D warnings` vor jedem Commit
- Python-Sidecar: `ruff` für Linting
- Commit-Messages: konventionell (`feat:`, `fix:`, `refactor:`, ...)
- Keine Secrets (iCloud-Passwort, Session-Cookies) ins Repo — gehören in
  Keyring bzw. `config.toml` (in `.gitignore`)
