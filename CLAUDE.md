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
  {"id": "...", "filename": "IMG_1234.HEIC", "added_date": "...", "asset_date": "...", "checksum": "...", "width": 4032, "height": 3024}
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
    filename        TEXT NOT NULL,
    added_date      TEXT NOT NULL,
    asset_date      TEXT,
    checksum        TEXT,
    width           INTEGER,
    height          INTEGER,
    thumb_path      TEXT,
    thumb_bytes     INTEGER,
    orig_path       TEXT,
    orig_bytes      INTEGER,
    is_orig_cached  INTEGER NOT NULL DEFAULT 0,
    last_accessed_at TEXT
);

CREATE INDEX idx_photos_last_accessed ON photos(last_accessed_at);
CREATE INDEX idx_photos_added_date ON photos(added_date);
```

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
│   ├── app/                   # GTK4-UI, Grid-View, Viewer
│   ├── sync/                  # Sync-Orchestrator, spawnt Sidecar, füllt SQLite
│   └── cache/                 # Katalog (SQLite), LRU-Eviction, Housekeeping
├── sidecar/
│   ├── pyicloud_sync.py       # Python-Skript mit pyicloud, gibt NDJSON aus
│   └── requirements.txt
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
{"type": "auth_ok", "trusted": true}
{"type": "photo", "id": "...", "filename": "IMG_1234.HEIC", "added_date": "...", "asset_date": "...", "checksum": "...", "width": 4032, "height": 3024}
{"type": "thumb", "id": "...", "path": "...", "bytes": 34567}
{"type": "thumb_failed", "id": "...", "message": "..."}
{"type": "done", "count": 1234, "cursor": "2026-01-02T03:04:05+00:00"}
{"type": "error", "kind": "auth", "message": "..."}
```

Im Serve-Modus (`--serve`) kommen Befehle als JSON-Zeilen über stdin:
`{"cmd": "original", "id": ..., "dest": ...}` und `{"cmd": "quit"}`.

Auf `password_required` bzw. `auth_required` erwartet der Sidecar das Passwort
bzw. den 2FA-Code als eine Zeile auf stdin — nie über `argv`, denn
Kommandozeilen sind in `/proc` für jeden Prozess des Nutzers lesbar. Nach
erfolgreicher Anmeldung legt der Sidecar das Passwort im Keyring ab
(`pyicloud://icloud-password`), sodass kein weiterer Lauf danach fragt.

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
`baseline_offset`, `last_baseline_at`.

**Zwei Regeln, an denen die Korrektheit hängt:**

- Der Token wird **vor** dem Durchlauf geholt. Änderungen während des Laufs
  kommen dann beim nächsten Mal erneut — andersherum gingen sie verloren.
- Der Token wird **nur übernommen**, wenn der lokale Stand ihn verdient:
  unverändert, vollständiger Durchlauf beendet, oder sauberer Delta-Lauf. Nach
  einem abgebrochenen Erstaufbau wäre er eine Lüge — der nächste Lauf zöge ein
  Delta und sähe den Rest der Bibliothek nie.

Ein durch `--limit` abgeschnittener Durchlauf meldet deshalb bewusst kein
`baseline_complete`, sondern nur seinen `offset`.

Alle 24 Stunden läuft trotzdem ein vollständiger Abgleich (`RECONCILE_AFTER`)
— Sicherheitsnetz gegen langsames Abdriften durch Records, die sich keinem
Asset zuordnen lassen.

## Wiederaufnahme

Ein abgebrochener Sync wird vom nächsten Lauf vervollständigt:

- Metadaten werden per Upsert geschrieben; Cache-Spalten bleiben unangetastet.
- Vorschauen, deren Datei schon existiert, werden nicht erneut geladen.
- Geschrieben wird über `.part` + `rename`, eine abgebrochene Datei gilt also
  nie als gültiger Treffer.
- Ein abgebrochener Erstaufbau setzt bei `baseline_offset` fort, statt von
  vorn zu paginieren. Der Fortschritt wird alle 50 Assets festgehalten.

## Offene Punkte / TODO

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
      Delta-Sync ältere Assets nicht erneut sieht. Die App hängt diesen
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
      Beginn eines Erstaufbaus — daher auch die Anzeige „1.234 / 19.624".
- [x] **Inkrementeller Sync über den CloudKit-Zone-Token** (siehe eigener
      Abschnitt). Löst auch Löschungen, die über `added_date` prinzipiell
      unsichtbar sind.
- [ ] `change_unresolved` beobachten: welche `record_type`-Werte in der Zone
      auftauchen, die kein Asset sind. Bislang nur gegen den Fake-Sidecar
      geprüft, nicht gegen die echte API.
- [ ] Session-Refresh-Handling bei abgelaufener pyicloud-Session
- [ ] `GListModel`-Implementierung für paginierte SQLite-Queries.
      Zwischenlösung: `gio::ListStore`, der beim Scrollen seitenweise nachlädt
      (`PAGE_SIZE = 200`) — der Katalog landet nie ganz im RAM, aber
      Zurückscrollen gibt Speicher nicht frei.
- [ ] Fehlerbehandlung bei Netzwerkausfall während Full-Res-Download

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

Umgebungsvariablen: `ICLOUD_PHOTOS_PYTHON` und `ICLOUD_PHOTOS_SIDECAR`
überschreiben Interpreter und Skriptpfad, `RUST_LOG` steuert das Logging.
`ICLOUD_PHOTOS_FAKE_DELAY_MS` bremst den Fake-Sidecar, damit das Einlaufen der
Kacheln überhaupt beobachtbar ist.

Der Testmodus läuft unter eigener Application-ID (`…​.Fake`, `NON_UNIQUE`).
Ohne das reicht GApplication die Aktivierung per D-Bus an eine bereits
laufende Produktivinstanz weiter und der Testlauf beendet sich wortlos.

## Konventionen für Claude Code

- Rust: `rustfmt` + `clippy -- -D warnings` vor jedem Commit
- Python-Sidecar: `ruff` für Linting
- Commit-Messages: konventionell (`feat:`, `fix:`, `refactor:`, ...)
- Keine Secrets (iCloud-Passwort, Session-Cookies) ins Repo — gehören in
  Keyring bzw. `config.toml` (in `.gitignore`)
