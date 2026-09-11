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
{"type": "auth_required", "method": "2fa"}
{"type": "auth_ok", "trusted": true}
{"type": "photo", "id": "...", "filename": "IMG_1234.HEIC", "added_date": "...", "asset_date": "...", "checksum": "...", "width": 4032, "height": 3024}
{"type": "done", "count": 1234, "cursor": "2026-01-02T03:04:05+00:00"}
{"type": "error", "kind": "auth", "message": "..."}
```

Auf `auth_required` erwartet der Sidecar den 2FA-Code als eine Zeile auf stdin.

`--fake N` erzeugt N synthetische Datensätze ohne iCloud-Zugriff und ohne
Import von `pyicloud`; damit läuft die komplette Rust-Seite im Test gegen einen
echten Subprozess.

## Offene Punkte / TODO

- [x] 2FA-UX: blockierender `AdwAlertDialog`, Code geht über stdin an den
      Sidecar. Notification verworfen — der Code ist nur Minuten gültig.
- [x] Entscheidung: Sidecar pro Sync-Lauf neu gestartet. Für Metadaten reicht
      das. **Aber:** für Full-Res on demand ist der Auth-Overhead pro Prozess
      zu hoch, dafür braucht es einen dauerhaften `--serve`-Modus.
- [ ] **Bild-Download fehlt komplett** — weder Thumbnails noch Full-Res werden
      geladen. Der Katalog hat die Spalten, die UI zeigt vorhandene Dateien an,
      aber nichts füllt sie. Grösster offener Brocken.
- [ ] Löschungen: `added_date`-Cursor bemerkt in iCloud gelöschte Fotos nie.
      `PhotosService.iter_changes(since=...)` liefert sie — umstellen.
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

# Sync gegen synthetische Daten statt iCloud
ICLOUD_PHOTOS_FAKE=500 cargo run -p icloud-photos-app
```

Umgebungsvariablen: `ICLOUD_PHOTOS_PYTHON` und `ICLOUD_PHOTOS_SIDECAR`
überschreiben Interpreter und Skriptpfad, `RUST_LOG` steuert das Logging.

## Konventionen für Claude Code

- Rust: `rustfmt` + `clippy -- -D warnings` vor jedem Commit
- Python-Sidecar: `ruff` für Linting
- Commit-Messages: konventionell (`feat:`, `fix:`, `refactor:`, ...)
- Keine Secrets (iCloud-Passwort, Session-Cookies) ins Repo — gehören in
  Keyring bzw. `config.toml` (in `.gitignore`)
