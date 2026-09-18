#!/usr/bin/env python3
"""Liest Foto-Metadaten aus iCloud und gibt sie als NDJSON auf stdout aus.

Das Protokoll ist zeilenweises JSON; jede Zeile hat ein Feld ``type``:

    {"type": "password_required", "username": "..."}  -> erwartet Passwort auf stdin
    {"type": "auth_required", "method": "2fa"}   -> erwartet Code auf stdin
    {"type": "terms_required", "url": "..."}    -> erwartet "accept" auf stdin
    {"type": "auth_ok", "trusted": true}
    {"type": "photo", "id": ..., "filename": ..., "lat": 48.137, "lon": 11.575}
    {"type": "cursor", "token": "..."}          Zone-Token vor dem Durchlauf
    {"type": "unchanged"}                       nichts zu tun (Token unveraendert)
    {"type": "thumb", "id": ..., "path": "...", "bytes": 34567}
    {"type": "deleted", "id": "..."}            in iCloud geloescht
    {"type": "library_count", "value": 19624}   Bestandsabgleich nach einem Delta
    {"type": "ready"}                           Serve-Modus nimmt Befehle entgegen
    {"type": "original", "id": ..., "path": "...", "bytes": 4200000}
    {"type": "original_failed", "id": ..., "kind": "missing"|"error", "message": "..."}
    {"type": "offset", "value": 1200}           Fortschritt des Erstaufbaus
    {"type": "baseline_complete", "count": 17000}
    {"type": "token_expired", "message": "..."} Delta nicht moeglich
    {"type": "thumb_failed", "id": ..., "message": "..."}
    {"type": "done", "count": 1234, "cursor": "2026-01-02T03:04:05+00:00"}
    {"type": "error", "kind": "auth", "message": "..."}
    {"type": "album", "id": ..., "name": ..., "kind": "smart"|"album"|"folder", "parent": ...}
    {"type": "album_members", "id": ..., "size": 3, "members": ["...", ...]}
    {"type": "album_unchanged", "id": ...}      Groesse wie zuletzt, Mitglieder bleiben
    {"type": "albums_done", "count": 158}

stdout gehoert ausschliesslich dem Protokoll. Alles Menschenlesbare geht nach
stderr, sonst zerreisst es den Parser auf der Rust-Seite.

Thumbnails laedt dieser Schritt mit ``--thumbs DIR`` gleich mit. Originale
nicht — die kommen erst beim Oeffnen eines Fotos.
"""

from __future__ import annotations

import argparse
import contextlib
import itertools
import json
import os
import struct
import sys
import time
import zlib
from collections import deque
from collections.abc import Iterator
from concurrent.futures import Future, ThreadPoolExecutor, as_completed
from datetime import UTC, datetime, timedelta
from pathlib import Path
from typing import Any

EXIT_OK = 0
EXIT_ERROR = 1
EXIT_AUTH = 2
# Die App ist weg. Kein Fehler im eigentlichen Sinn — es gibt nur niemanden
# mehr, der zuhoert.
EXIT_PIPE = 3


def emit(payload: dict[str, Any]) -> None:
    """Schreibt eine Protokollzeile und flusht sofort.

    Erst vollstaendig serialisieren, dann schreiben: ``json.dump`` wuerde direkt
    in den Stream schreiben und bei einem nicht serialisierbaren Wert eine halbe
    Zeile hinterlassen. Der Parser auf der Rust-Seite bekaeme dann eine
    zerrissene Zeile und der ganze Lauf waere verloren — statt nur dieses einen
    Feldes.

    Ohne Flush puffert Python bei Pipe-stdout bis zu 8 KB, und die UI zeigt
    minutenlang keinen Fortschritt.
    """
    try:
        line = json.dumps(payload, separators=(",", ":"), ensure_ascii=False)
    except (TypeError, ValueError) as exc:
        line = json.dumps(
            {
                "type": "error",
                "kind": "protocol",
                "message": f"Message not serializable: {exc}",
            },
            separators=(",", ":"),
        )
    try:
        sys.stdout.write(line + "\n")
        sys.stdout.flush()
    except BrokenPipeError:
        _exit_without_a_listener()


def _exit_without_a_listener() -> None:
    """Beendet den Lauf, wenn die App die Leitung geschlossen hat.

    Passiert im Normalbetrieb: die App beendet einen Sidecar, sobald sie ihn
    nicht mehr braucht — beim Schliessen der App, nach einem Fehler, oder wenn
    ein Sync abgebrochen wird. Ohne diesen Griff endet der Prozess mit einem
    Traceback im Terminal, und der Fehlerbehandler wuerde dieselbe kaputte
    Leitung gleich noch einmal beschreiben.

    stdout wird dabei auf /dev/null umgelegt: sonst meldet der Interpreter beim
    Beenden ein zweites Mal denselben BrokenPipeError, diesmal ohne dass ihn
    jemand abfangen koennte.
    """
    try:
        devnull = os.open(os.devnull, os.O_WRONLY)
        os.dup2(devnull, sys.stdout.fileno())
    except OSError:  # pragma: no cover - dann eben ohne
        pass
    log("stdout geschlossen — die App hoert nicht mehr zu, Lauf wird beendet")
    # SystemExit erbt von BaseException und laeuft deshalb durch die
    # `except Exception`-Blocke der Aufrufer hindurch.
    raise SystemExit(EXIT_PIPE)


def log(message: str) -> None:
    # Auch stderr kann weg sein. Dann gibt es nichts mehr zu melden.
    with contextlib.suppress(BrokenPipeError):
        print(message, file=sys.stderr, flush=True)


def iso(value: datetime | None) -> str | None:
    if value is None:
        return None
    if value.tzinfo is None:
        value = value.replace(tzinfo=UTC)
    return value.astimezone(UTC).isoformat()


def parse_since(raw: str | None) -> datetime | None:
    if not raw:
        return None
    parsed = datetime.fromisoformat(raw.replace("Z", "+00:00"))
    if parsed.tzinfo is None:
        parsed = parsed.replace(tzinfo=UTC)
    return parsed


# --------------------------------------------------------------------------
# Authentifizierung
# --------------------------------------------------------------------------


def read_secret() -> str:
    """Liest eine Geheimniszeile von stdin.

    Kein ``strip()`` — nur der Zeilenumbruch faellt weg. Ein Passwort darf
    fuehrende oder folgende Leerzeichen enthalten.
    """
    return sys.stdin.readline().rstrip("\r\n")


# Apples iCloud-Nutzungsbedingungen. Die API liefert nur eine Versionsnummer,
# keinen Text — die App zeigt deshalb diesen Verweis zum Nachlesen.
TERMS_URL = "https://www.apple.com/de/legal/internet-services/icloud/"


def confirm_terms() -> None:
    """Fragt die App, ob die geaenderten Nutzungsbedingungen akzeptiert werden.

    Kehrt nur bei Zustimmung zurueck. Akzeptiert wird nie stillschweigend: die
    Zustimmung gilt fuer das Apple-Konto, nicht nur fuer diese App.
    """
    emit({"type": "terms_required", "url": TERMS_URL})
    if read_secret().strip() != "accept":
        emit(
            {
                "type": "error",
                "kind": "terms",
                "message": "Terms and Conditions not accepted",
            }
        )
        sys.exit(EXIT_AUTH)
    log("Geaenderte iCloud-Nutzungsbedingungen akzeptiert")


def authenticate(username: str, cookie_dir: str | None):
    """Meldet sich an und erledigt Passwort- und 2FA-Abfrage ueber stdin."""
    from pyicloud import PyiCloudService
    from pyicloud.exceptions import PyiCloudAcceptTermsException, PyiCloudFailedLoginException
    from pyicloud.utils import get_password_from_keyring, store_password_in_keyring

    password = get_password_from_keyring(username)
    from_keyring = password is not None

    if password is None:
        # Die App fragt das Passwort ab und schickt es ueber stdin. Bewusst
        # nicht ueber argv — Kommandozeilen sind in /proc fuer jeden Prozess
        # des Nutzers lesbar.
        emit({"type": "password_required", "username": username})
        password = read_secret()
        if not password:
            emit({"type": "error", "kind": "auth", "message": "No password received"})
            sys.exit(EXIT_AUTH)

    try:
        try:
            api = PyiCloudService(username, password, cookie_directory=cookie_dir)
        except PyiCloudAcceptTermsException:
            # Hat Apple die Bedingungen geaendert, scheitert schon der
            # Konstruktor. Nach der Zustimmung also noch einmal von vorn —
            # diesmal mit Erlaubnis, sie zu akzeptieren.
            confirm_terms()
            api = PyiCloudService(
                username, password, cookie_directory=cookie_dir, accept_terms=True
            )
    except PyiCloudFailedLoginException as exc:
        # Eigene Fehlerart, damit die App gezielt erneut nach dem Passwort
        # fragen kann statt den Sync als kaputt zu melden.
        emit({"type": "error", "kind": "credentials", "message": str(exc) or "Sign-in rejected"})
        sys.exit(EXIT_AUTH)

    # Erst nach erfolgreicher Anmeldung speichern — sonst landet ein Tippfehler
    # im Keyring und jeder weitere Lauf scheitert daran.
    if not from_keyring:
        try:
            store_password_in_keyring(username, password)
            log("Passwort im Keyring hinterlegt")
        except Exception as exc:  # noqa: BLE001 - Keyring-Backends scheitern vielfaeltig
            log(f"Passwort konnte nicht im Keyring gespeichert werden: {exc}")

    if api.requires_2fa:
        emit({"type": "auth_required", "method": "2fa"})
        code = read_secret().strip()
        if not code:
            emit({"type": "error", "kind": "auth", "message": "No two-factor code received"})
            sys.exit(EXIT_AUTH)
        try:
            if not api.validate_2fa_code(code):
                emit({"type": "error", "kind": "auth", "message": "Two-factor code rejected"})
                sys.exit(EXIT_AUTH)
            # Ohne trust_session laeuft die Sitzung nach Stunden ab und jeder
            # Sync fragt erneut nach einem Code.
            if not api.is_trusted_session:
                api.trust_session()
        except PyiCloudAcceptTermsException:
            # Nach der 2FA meldet sich pyicloud erneut an und stoesst erst
            # dann auf die Bedingungen. Der Code ist verbraucht; nur die
            # Anmeldung wird wiederholt. `_accept_terms` ist nicht oeffentlich,
            # einen anderen Weg gibt es an diesem Punkt aber nicht.
            confirm_terms()
            api._accept_terms = True
            api.trust_session()

    elif api.requires_2sa:
        emit(
            {
                "type": "error",
                "kind": "auth",
                "message": "Legacy two-step verification (2SA) is not supported",
            }
        )
        sys.exit(EXIT_AUTH)

    emit({"type": "auth_ok", "trusted": bool(api.is_trusted_session)})
    return api


# --------------------------------------------------------------------------
# Metadaten
# --------------------------------------------------------------------------


# pyicloud liest den Aufnahmeort, haengt ihn aber nicht an `PhotoAsset`: er
# steckt als plist im Feld `locationEnc` des Asset-Records. Der Zugriff geht
# deshalb ueber eine nicht-oeffentliche Funktion — faellt sie weg, gibt es eben
# keine Koordinaten. Der Import passiert beim ersten Bedarf, damit `--fake`
# weiterhin ohne pyicloud auskommt.
_LOCATION_READER: Any = None


def location_reader() -> Any:
    global _LOCATION_READER
    if _LOCATION_READER is None:
        try:
            from pyicloud.services.photos_cloudkit.materialize import _extract_location

            _LOCATION_READER = _extract_location
        except Exception as exc:  # noqa: BLE001 - private API, Ausfall ist vorgesehen
            log(f"Ortsdaten nicht lesbar: {type(exc).__name__}: {exc}")
            _LOCATION_READER = False
    return _LOCATION_READER or None


def coordinates_of(asset: Any) -> tuple[float | None, float | None]:
    """Aufnahmeort als Breiten- und Laengengrad, sofern hinterlegt."""
    reader = location_reader()
    if reader is None:
        return None, None
    try:
        location = reader(asset.asset_record)
    except Exception:  # noqa: BLE001 - fremde API
        return None, None
    return location.get("latitude"), location.get("longitude")


def record_for(asset: Any) -> dict[str, Any]:
    """Baut den NDJSON-Datensatz fuer ein Asset."""
    width, height = asset.dimensions

    # Die Pruefsumme haengt am Original-Resource, nicht am Asset selbst.
    checksum = None
    try:
        original = asset.resources.get("original")
        if original is not None:
            checksum = original.checksum
    except Exception:  # pragma: no cover - defensiv, Feld ist optional
        checksum = None

    # Der Master-Record wird beim Full-Res-Download gebraucht; ihn hier
    # mitzunehmen spart dort einen Lookup.
    try:
        master_id = asset.master_id
    except Exception:  # noqa: BLE001 - optionales Feld
        master_id = None

    # "image" oder "movie". Ohne dieses Feld muss die App den Medientyp aus
    # der Dateiendung raten.
    try:
        item_type = asset.item_type
    except Exception:  # noqa: BLE001 - optionales Feld
        item_type = None

    lat, lon = coordinates_of(asset)

    return {
        "type": "photo",
        "id": asset.id,
        "master_id": master_id,
        "filename": asset.filename,
        "added_date": iso(asset.added_date),
        "asset_date": iso(asset.asset_date),
        "checksum": checksum,
        "width": width,
        "height": height,
        "item_type": item_type,
        "lat": lat,
        "lon": lon,
    }


# --------------------------------------------------------------------------
# Thumbnails
# --------------------------------------------------------------------------

# Schluessel der Vorschau-Resource in pyicloud.
# Bei Fotos ist "thumb" das JPEG (resJPEGThumb). Bei Videos zeigt "thumb"
# dagegen auf resVidSmall — ein Video. Das Standbild heisst dort "thumb_image".
THUMB_VERSION = "thumb"
VIDEO_THUMB_VERSION = "thumb_image"


def thumb_version_for(asset: Any) -> str:
    try:
        return VIDEO_THUMB_VERSION if asset.item_type == "movie" else THUMB_VERSION
    except Exception:  # noqa: BLE001 - fremde API, im Zweifel das Foto-Verhalten
        return THUMB_VERSION


def thumb_path_for(directory: Path, asset_id: str) -> Path:
    """Dateiname aus der Asset-ID.

    iCloud-IDs koennen ``/`` enthalten, deshalb wird die ID vor der Verwendung
    als Dateiname entschaerft.
    """
    safe = "".join(char if char.isalnum() or char in "-_" else "_" for char in asset_id)
    return directory / f"{safe}.jpg"


def download_thumb(asset: Any, directory: Path) -> dict[str, Any]:
    """Laedt die Vorschau eines Assets. Gibt die zu sendende Protokollzeile zurueck."""
    target = thumb_path_for(directory, asset.id)

    # Bereits vorhanden: nicht erneut laden. Macht abgebrochene Syncs billig.
    if target.is_file() and target.stat().st_size > 0:
        return {
            "type": "thumb",
            "id": asset.id,
            "path": str(target),
            "bytes": target.stat().st_size,
        }

    try:
        data = asset.download(thumb_version_for(asset))
    except Exception as exc:  # noqa: BLE001 - fremde API, jeder Fehler ist hier gleich
        return {"type": "thumb_failed", "id": asset.id, "message": f"{type(exc).__name__}: {exc}"}

    if not data:
        return {"type": "thumb_failed", "id": asset.id, "message": "no thumbnail available"}

    # Erst vollstaendig schreiben, dann umbenennen: ein Abbruch hinterlaesst
    # sonst eine halbe Datei, die spaeter als gueltiger Cache-Treffer gilt.
    tmp = target.with_suffix(".part")
    tmp.write_bytes(data)
    tmp.replace(target)
    return {"type": "thumb", "id": asset.id, "path": str(target), "bytes": len(data)}


def iter_assets(
    api: Any, since: datetime | None, limit: int | None, offset: int = 0
) -> Iterator[Any]:
    """Laeuft die gesamte Bibliothek durch.

    ``photos.all`` ist **nicht** nach ``added_date`` sortiert — gemessen an
    einer echten Bibliothek wechseln die Daten zwischen aufeinanderfolgenden
    Assets beliebig hin und her. Ein vorzeitiger Abbruch beim ersten "zu alten"
    Asset wuerde den Rest der Bibliothek verschlucken; deshalb wird immer
    vollstaendig paginiert.

    ``since`` filtert nur, welche Assets ausgegeben werden, und beendet den
    Durchlauf nie.
    """
    emitted = 0
    source = photos_from(api.photos.all, offset) if offset else api.photos.all
    for asset in source:
        if since is not None and asset.added_date <= since:
            continue
        yield asset
        emitted += 1
        if limit is not None and emitted >= limit:
            log(f"Limit {limit} erreicht")
            return


def photos_from(album: Any, offset: int) -> Iterator[Any]:
    """Die Assets von ``album`` ab Position ``offset`` — mit serverseitigem
    Einstieg.

    ``itertools.islice`` ueber ``album.photos`` sieht nur so aus, als finge es
    bei ``offset`` an: es holt jede Seite davor und wirft sie weg. Bei ~3 s je
    100 Assets hiess das fuer einen bei 8.700 abgebrochenen Erstaufbau rund
    viereinhalb Minuten ohne ein einziges neues Foto — bei jedem Start. Wer die
    App vorher schloss, kam nie weiter.

    Nachgebaut ist die Schleife aus ``PhotoAlbum.photos``, nur mit anderem
    Startrang. ``_get_photos_at`` und ``_direction`` sind nicht oeffentlich;
    fehlen sie, bleibt der langsame, aber richtige Weg.
    """
    try:
        fetch = album._get_photos_at
        direction = album._direction
        page_size = album.page_size
        total = len(album)
    except Exception as exc:  # noqa: BLE001 - nicht-oeffentliche API
        log(f"Serverseitiger Einstieg nicht moeglich ({type(exc).__name__}: {exc})")
        yield from itertools.islice(album, offset, None)
        return

    # `photos.all` laeuft absteigend: Position 0 ist der hoechste Rang.
    descending = getattr(direction, "name", "") == "DESCENDING"
    if offset >= total:
        return
    rank = total - 1 - offset if descending else offset
    seen: set[str] = set()
    while True:
        received = 0
        for asset in fetch(rank, direction, page_size):
            received += 1
            if asset.id in seen:
                continue
            seen.add(asset.id)
            yield asset
        if received < page_size // 2:
            return
        rank = rank - received if descending else rank + received


# Alle so viele Assets meldet der Erstaufbau seinen Fortschritt, damit ein
# Abbruch nicht den ganzen Durchlauf kostet.
OFFSET_REPORT_EVERY = 50

# So viele Vorschauen laden gleichzeitig. Gemessen: 1 → 3,4/s, 4 → 12/s,
# 8 → 22/s; darueber bremst das Seitenholen (~33 Assets/s).
THUMB_WORKERS = 8


def zone_token(api: Any) -> str | None:
    """Aktueller CloudKit-syncToken der Foto-Zone."""
    try:
        return api.photos.sync_cursor()
    except Exception as exc:  # noqa: BLE001 - fremde API
        log(f"syncToken nicht abrufbar: {type(exc).__name__}: {exc}")
        return None


def run_baseline(
    api: Any,
    thumbs: Path | None,
    offset: int,
    limit: int | None,
) -> int:
    """Vollstaendiger Durchlauf ab ``offset``.

    Nur fuer den Erstaufbau, einen abgelehnten Token oder den periodischen
    Abgleich. Der laufende Betrieb geht ueber ``run_changes``.
    """
    # Gesamtzahl vorweg: die UI kann damit von Anfang an Platzhalter fuer die
    # noch fehlenden Fotos zeigen, statt eine leere Flaeche.
    try:
        emit({"type": "library_count", "value": len(api.photos.all)})
    except Exception as exc:  # noqa: BLE001 - nur fuer die Anzeige
        log(f"Bestand nicht zaehlbar: {exc}")

    count = 0
    # Vorschauen laden parallel: einzeln kam der Erstaufbau auf ~2 Assets/s,
    # eine Stunde fuer 10.000 Fotos. Ausgegeben wird trotzdem nur hier, in
    # Reihenfolge — und `offset` erst, wenn alles davor samt Vorschau
    # geschrieben ist. Ein Abbruch verliert so nichts, was schon als erledigt
    # gemeldet wurde.
    pending: deque[tuple[int, Future[dict[str, Any]] | None]] = deque()

    def settle(index: int, thumb: Future[dict[str, Any]] | None) -> None:
        if thumb is not None:
            emit(thumb.result())
        if (index + 1 - offset) % OFFSET_REPORT_EVERY == 0:
            emit({"type": "offset", "value": index + 1})

    with ThreadPoolExecutor(max_workers=THUMB_WORKERS) as pool:
        for index, asset in enumerate(iter_assets(api, None, limit, offset=offset), start=offset):
            emit(record_for(asset))
            count += 1
            thumb = pool.submit(download_thumb, asset, thumbs) if thumbs is not None else None
            pending.append((index, thumb))
            while len(pending) > THUMB_WORKERS * 2:
                settle(*pending.popleft())
        while pending:
            settle(*pending.popleft())

    # Ein durch --limit abgeschnittener Lauf ist kein vollstaendiger Abgleich.
    # Wuerde er sich so melden, uebernaehme die App den Zone-Token und sehe den
    # Rest der Bibliothek nie wieder.
    if limit is None or count < limit:
        emit({"type": "baseline_complete", "count": count})
    else:
        emit({"type": "offset", "value": offset + count})
    return count


# Album-Zugehoerigkeit heisst in CloudKit "<asset>-IN-<album>". Ein Tombstone
# darauf bedeutet "aus Album entfernt" — das Foto existiert weiter. Wuerde man
# danach loeschen, verschwaenden Fotos aus dem Katalog, die es noch gibt.
MEMBERSHIP_MARKER = "-IN-"


def run_changes(api: Any, thumbs: Path | None, since_token: str) -> int:
    """Nur die Aenderungen seit ``since_token`` — der Normalfall.

    Kosten haengen an der Zahl der Aenderungen, nicht an der Bibliotheksgroesse.
    """
    album = api.photos.all
    count = 0
    for event in api.photos.iter_changes(since=since_token):
        record_name = event.record_name

        if MEMBERSHIP_MARKER in record_name:
            # Album-Mitgliedschaft, kein Asset. Betrifft uns nicht: Alben sind
            # in v1 ohnehin nicht abgebildet.
            continue

        if event.deleted:
            # Tombstones geben nur den Namen her. Ob der zu einem unserer Fotos
            # gehoert, entscheidet die Rust-Seite anhand ihres Katalogs.
            emit({"type": "deleted", "id": record_name})
            count += 1
            continue

        try:
            asset = album.get(record_name)
        except Exception as exc:  # noqa: BLE001 - fremde API
            emit(
                {
                    "type": "change_unresolved",
                    "id": record_name,
                    "record_type": event.record_type,
                    "message": f"{type(exc).__name__}: {exc}",
                }
            )
            continue

        if asset is None:
            # Nicht jeder Record in der Zone ist ein Asset — Alben und
            # Hilfsrecords tauchen hier ebenfalls auf.
            emit(
                {
                    "type": "change_unresolved",
                    "id": record_name,
                    "record_type": event.record_type,
                    "message": "no asset",
                }
            )
            continue

        emit(record_for(asset))
        count += 1
        if thumbs is not None:
            emit(download_thumb(asset, thumbs))
    return count


def run_sync(
    username: str,
    cookie_dir: str | None,
    since: datetime | None,
    limit: int | None,
    thumbs: Path | None,
    since_token: str | None,
    offset: int,
    force_baseline: bool,
) -> int:
    api = authenticate(username, cookie_dir)
    if thumbs is not None:
        thumbs.mkdir(parents=True, exist_ok=True)

    # Den Token *vor* dem Durchlauf holen: Aenderungen waehrend des Laufs
    # kommen dann beim naechsten Mal erneut. Andersherum gingen sie verloren.
    current = zone_token(api)
    emit({"type": "cursor", "token": current})

    # Schicht 1: gleicher Token heisst, in der Bibliothek hat sich nichts getan.
    if since_token is not None and current is not None and since_token == current:
        emit({"type": "unchanged"})
        emit({"type": "done", "count": 0, "cursor": None})
        return EXIT_OK

    # Schicht 2: nur die Aenderungen.
    if since_token is not None and not force_baseline:
        try:
            count = run_changes(api, thumbs, since_token)
        except Exception as exc:  # noqa: BLE001 - abgelaufener Token sieht je nach Server anders aus
            log(f"Delta-Abgleich fehlgeschlagen, falle auf Vollabgleich zurueck: {exc}")
            emit({"type": "token_expired", "message": f"{type(exc).__name__}: {exc}"})
        else:
            # Bestandsabgleich: eine verpasste Aenderung faellt hier auf, und
            # die App kann einen Vollabgleich nachziehen. Nur im Delta-Fall —
            # die Zaehlung kostet einen Request, den der unveraenderte Fall
            # nicht ausgeben soll.
            try:
                emit({"type": "library_count", "value": len(api.photos.all)})
            except Exception as exc:  # noqa: BLE001 - nur ein Sicherheitsnetz
                log(f"Bestand nicht zaehlbar: {exc}")
            emit({"type": "done", "count": count, "cursor": None})
            return EXIT_OK

    # Schicht 3: vollstaendiger Durchlauf.
    count = run_baseline(api, thumbs, offset, limit)
    emit({"type": "done", "count": count, "cursor": None})
    return EXIT_OK


# --------------------------------------------------------------------------
# Serve-Modus: dauerhaft laufend, fuer Originale auf Abruf
# --------------------------------------------------------------------------

ORIGINAL_VERSION = "original"
VIDEO_ORIGINAL_VERSION = "original"


def original_version_for(asset: Any) -> str:
    """Schluessel der Original-Resource."""
    try:
        return VIDEO_ORIGINAL_VERSION if asset.item_type == "movie" else ORIGINAL_VERSION
    except Exception:  # noqa: BLE001 - fremde API, im Zweifel das Foto-Verhalten
        return ORIGINAL_VERSION


def download_original(asset: Any, dest: Path) -> dict[str, Any]:
    """Laedt das Original nach ``dest``. Gibt die Protokollzeile zurueck."""
    if dest.is_file() and dest.stat().st_size > 0:
        return {
            "type": "original",
            "id": asset.id,
            "path": str(dest),
            "bytes": dest.stat().st_size,
        }

    try:
        data = asset.download(original_version_for(asset))
    except Exception as exc:  # noqa: BLE001 - fremde API
        # Kann ein Netzaussetzer oder eine abgelaufene Sitzung sein.
        return original_failed(asset.id, f"{type(exc).__name__}: {exc}")

    if not data:
        return original_failed(asset.id, "no original available", kind="missing")

    dest.parent.mkdir(parents=True, exist_ok=True)
    # Wie bei den Vorschauen: erst vollstaendig schreiben, dann umbenennen.
    tmp = dest.with_suffix(dest.suffix + ".part")
    tmp.write_bytes(data)
    tmp.replace(dest)
    return {"type": "original", "id": asset.id, "path": str(dest), "bytes": len(data)}


def original_failed(asset_id: str, message: str, kind: str = "error") -> dict[str, Any]:
    """Fehlschlag beim Holen eines Originals.

    ``kind`` trennt, was ein zweiter Anlauf beheben kann, von dem, was er nicht
    behebt: ``missing`` heisst, das Asset gibt es nicht oder es hat kein
    Original — da hilft auch eine frische Anmeldung nicht. Alles andere
    (Netzfehler, abgelaufene Sitzung, Ausnahme im Lookup) ist ``error``, und
    die App setzt einmal neu auf.
    """
    return {"type": "original_failed", "id": asset_id, "kind": kind, "message": message}


def master_record_name(record: Any) -> str | None:
    """Name des CPLMaster-Records, auf den ein CPLAsset zeigt."""
    field = (getattr(record, "fields", None) or {}).get("masterRef")
    value = getattr(field, "value", field)
    if isinstance(value, dict):
        return value.get("recordName")
    return getattr(value, "recordName", None)


class AssetResolver:
    """Loest Asset-IDs auf — direkt ueber CloudKit statt ueber das Album.

    ``PhotoAlbum.get`` faellt intern auf einen vollstaendigen Durchlauf der
    Bibliothek zurueck, wenn seine gezielte Abfrage nichts findet. Gemessen an
    einer Bibliothek mit ~19.600 Fotos kostet das rund 15 Sekunden pro Foto,
    waehrend der eigentliche Download unter einer Sekunde braucht.

    Ein direkter ``records/lookup`` liegt bei ~0,3 s. Dafuer werden allerdings
    nicht-oeffentliche Teile von pyicloud benutzt, deshalb gibt es den
    Album-Weg weiterhin als Rueckfall.
    """

    def __init__(self, api: Any, album_fallback: bool = True) -> None:
        self._api = api
        # Fuer ein einzelnes geoeffnetes Foto ist der langsame Weg besser als
        # keiner. Fuer einen Stapel nicht: jeder Fehlschlag kostete dort einen
        # Durchlauf der ganzen Bibliothek.
        self._album_fallback = album_fallback
        self._album: Any = None
        self._fast: Any = None
        try:
            from pyicloud.common.cloudkit.models import CKZoneIDReq

            library = api.photos._root_library
            self._fast = (
                library,
                api.photos.private_client,
                CKZoneIDReq(**library.zone_id),
            )
        except Exception as exc:  # noqa: BLE001 - private API, Rueckfall ist vorgesehen
            log(f"Direkter Record-Zugriff nicht verfuegbar, nutze Album: {exc}")

    def _album_lookup(self, asset_id: str) -> Any:
        if not self._album_fallback:
            return None
        if self._album is None:
            self._album = self._api.photos.all
        return self._album.get(asset_id)

    def get(self, asset_id: str, master_id: str | None = None) -> Any:
        if self._fast is None:
            return self._album_lookup(asset_id)

        from pyicloud.services.photos import PhotoAsset

        library, client, zone = self._fast
        try:
            if master_id:
                # Master schon bekannt: ein einziger Lookup genuegt.
                records = client.lookup(record_names=[asset_id, master_id], zone_id=zone).records
            else:
                first = client.lookup(record_names=[asset_id], zone_id=zone).records
                if not first:
                    return None
                name = master_record_name(first[0])
                if not name:
                    return self._album_lookup(asset_id)
                records = client.lookup(record_names=[asset_id, name], zone_id=zone).records

            by_type = {record.recordType: record for record in records}
            asset_record = by_type.get("CPLAsset")
            master_record = by_type.get("CPLMaster")
            if asset_record is None or master_record is None:
                return self._album_lookup(asset_id)
            return PhotoAsset(self._api.photos, master_record, asset_record, library)
        except Exception as exc:  # noqa: BLE001 - jeder Fehler heisst: Album-Weg
            log(f"Direkter Lookup fuer {asset_id} fehlgeschlagen ({exc}), nutze Album")
            return self._album_lookup(asset_id)


def run_serve(username: str, cookie_dir: str | None) -> int:
    """Bleibt laufen und beantwortet Befehle von stdin.

    Fuer Originale auf Abruf: ein Prozessstart samt Anmeldung kostet Sekunden,
    das kann nicht bei jedem geoeffneten Foto anfallen.

    Befehle sind JSON-Zeilen: ``{"cmd": "original", "id": ..., "dest": ...}``
    und ``{"cmd": "quit"}``.
    """
    api = authenticate(username, cookie_dir)
    resolver = AssetResolver(api)
    emit({"type": "ready"})

    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue
        try:
            command = json.loads(line)
        except json.JSONDecodeError as exc:
            emit({"type": "error", "kind": "protocol", "message": f"unreadable command: {exc}"})
            continue

        name = command.get("cmd")
        if name == "quit":
            break
        if name != "original":
            emit({"type": "error", "kind": "protocol", "message": f"unknown command: {name}"})
            continue

        asset_id = command.get("id")
        dest = command.get("dest")
        if not asset_id or not dest:
            emit(original_failed(asset_id or "", "id oder dest fehlt", kind="missing"))
            continue

        try:
            asset = resolver.get(asset_id, command.get("master"))
        except Exception as exc:  # noqa: BLE001 - fremde API
            emit(original_failed(asset_id, f"{type(exc).__name__}: {exc}"))
            continue

        if asset is None:
            emit(original_failed(asset_id, "Asset not found", kind="missing"))
            continue

        emit(download_original(asset, Path(dest)))

    return EXIT_OK


# Eine Asset-ID, die im ersten Prozess scheitert und im zweiten liefert.
# Damit laesst sich die abgelaufene Sitzung nachstellen: die App muss den
# Sidecar wegwerfen und frisch anmelden, sonst kommt das Foto nie.
FAKE_FLAKY_ID = "fake-flaky"


def run_fake_serve(count: int, needs_password: bool, needs_terms: bool = False) -> int:
    """Serve-Modus ohne iCloud: liefert erzeugte Originale."""
    if needs_password:
        emit({"type": "password_required", "username": "fake@example.com"})
        if read_secret() != FAKE_PASSWORD:
            emit({"type": "error", "kind": "credentials", "message": "Sign-in rejected"})
            return EXIT_AUTH
    if needs_terms:
        confirm_terms()
    emit({"type": "auth_ok", "trusted": True})
    emit({"type": "ready"})

    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue
        command = json.loads(line)
        if command.get("cmd") == "quit":
            break
        asset_id = command.get("id", "")
        dest = Path(command["dest"])

        if asset_id == FAKE_FLAKY_ID:
            # Der Merker liegt ausserhalb des Prozesses, damit der zweite
            # Sidecar weiss, dass der erste schon gescheitert ist.
            dest.parent.mkdir(parents=True, exist_ok=True)
            marker = dest.parent / ".flaky-seen"
            if not marker.exists():
                marker.write_text("1")
                emit(original_failed(asset_id, "Session expired (simulated)"))
                continue
            dest.write_bytes(fake_png(320, 1))
            emit(
                {
                    "type": "original",
                    "id": asset_id,
                    "path": str(dest),
                    "bytes": dest.stat().st_size,
                }
            )
            continue

        if not asset_id.startswith("fake-"):
            emit(original_failed(asset_id, "Asset not found", kind="missing"))
            continue
        number = int(asset_id.removeprefix("fake-"))
        if number >= count:
            emit(original_failed(asset_id, "Asset not found", kind="missing"))
            continue

        dest.parent.mkdir(parents=True, exist_ok=True)
        # Deutlich groesser als eine Vorschau, damit die Eviction etwas zu tun hat.
        dest.write_bytes(fake_png(320, number))
        emit(
            {
                "type": "original",
                "id": asset_id,
                "path": str(dest),
                "bytes": dest.stat().st_size,
            }
        )

    return EXIT_OK


def run_fetch_thumbs(username: str, cookie_dir: str | None, thumbs: Path) -> int:
    """Laedt Vorschauen fuer die IDs, die zeilenweise auf stdin kommen.

    Noetig, weil der Delta-Sync aeltere Assets nicht erneut sieht: fehlt dort
    eine Vorschau, holt dieser Modus sie nach.
    """
    api = authenticate(username, cookie_dir)
    thumbs.mkdir(parents=True, exist_ok=True)
    # Nicht `photos.all.get`: findet dessen Abfrage ein Asset nicht, laeuft es
    # die ganze Bibliothek durch — gemessen gut zehn Minuten je fehlender
    # Vorschau. Ein Nachlauf fuer 13 Fotos hing so ueber zwei Stunden und hielt
    # alles dahinter auf, auch das Laden der Alben.
    resolver = AssetResolver(api, album_fallback=False)

    count = 0
    for line in sys.stdin:
        asset_id = line.strip()
        if not asset_id:
            continue
        try:
            asset = resolver.get(asset_id)
        except Exception as exc:  # noqa: BLE001 - fremde API
            emit(
                {
                    "type": "thumb_failed",
                    "id": asset_id,
                    "message": f"{type(exc).__name__}: {exc}",
                }
            )
            continue
        if asset is None:
            emit({"type": "thumb_failed", "id": asset_id, "message": "Asset not found"})
            continue
        emit(download_thumb(asset, thumbs))
        count += 1

    emit({"type": "done", "count": count, "cursor": None})
    return EXIT_OK


# --------------------------------------------------------------------------
# Alben
# --------------------------------------------------------------------------

# Intelligente Alben, die nicht in die Seitenleiste gehoeren: "Library" ist die
# Bibliothek selbst, und versteckte oder geloeschte Fotos stehen nicht im
# Katalog — ihre Alben blieben leer.
SKIPPED_SMART_ALBUMS = {"Library", "Hidden", "Recently Deleted"}

# So viele Alben werden gleichzeitig durchgegangen. Jedes kostet ~3 s je 100
# Fotos; bei rund 25.000 Zugehoerigkeiten waeren das einzeln zwoelf Minuten.
ALBUM_WORKERS = 6


def album_kind(album: Any) -> str:
    """Art eines Albums anhand seiner Klasse in pyicloud."""
    name = type(album).__name__
    if name == "SmartPhotoAlbum":
        return "smart"
    if name == "PhotoAlbumFolder":
        return "folder"
    return "album"


def album_content(album: Any, known: dict[str, int]) -> dict[str, Any]:
    """Die Protokollzeile fuer ein Album: seine Mitglieder, oder nur der Hinweis,
    dass es sich nicht veraendert hat.

    Die Groesse kostet eine Anfrage, die Mitglieder eine je 100 Fotos. Stimmt
    die Groesse mit dem letzten Stand, bleiben die gemerkten Mitglieder
    gueltig — an der echten Bibliothek der Unterschied zwischen fuenf Minuten
    und ein paar Sekunden.
    """
    size = len(album)
    if known.get(album.id) == size:
        return {"type": "album_unchanged", "id": album.id}
    members = [asset.id for asset in album.photos]
    return {"type": "album_members", "id": album.id, "size": size, "members": members}


def read_known_sizes() -> dict[str, int]:
    """Groessen vom letzten Mal — eine JSON-Zeile auf stdin, nach der Anmeldung."""
    line = sys.stdin.readline().strip()
    if not line:
        return {}
    try:
        known = json.loads(line)
    except ValueError:
        log("Bekannte Album-Groessen unlesbar — alle Alben werden neu geladen")
        return {}
    return {str(key): int(value) for key, value in known.items()}


def run_albums(username: str, cookie_dir: str | None) -> int:
    """Meldet alle Alben mit ihren Mitgliedern.

    Ausgabe: je Album eine ``album``-Zeile in der Reihenfolge, in der iCloud
    sie liefert, danach je Album mit Fotos eine ``album_members``-Zeile, zum
    Schluss ``albums_done``. Erst diese Zeile erlaubt der App, ihren Stand zu
    ersetzen — ein abgebrochener Lauf laesst den alten unangetastet.
    """
    api = authenticate(username, cookie_dir)
    known = read_known_sizes()
    albums = [
        album
        for album in api.photos.albums
        if not (album_kind(album) == "smart" and album.name in SKIPPED_SMART_ALBUMS)
    ]

    for position, album in enumerate(albums):
        emit(
            {
                "type": "album",
                "id": album.id,
                "name": album.name,
                "kind": album_kind(album),
                # Nicht oeffentlich, aber die einzige Stelle, an der pyicloud
                # den Ordner eines Albums fuehrt.
                "parent": getattr(album, "_parent_id", None),
                "position": position,
            }
        )

    # Ordner haben selbst keine Fotos — eine Abfrage waere verschenkt.
    with_photos = [album for album in albums if album_kind(album) != "folder"]
    with ThreadPoolExecutor(max_workers=ALBUM_WORKERS) as pool:
        futures = [pool.submit(album_content, album, known) for album in with_photos]
        for future in as_completed(futures):
            emit(future.result())

    emit({"type": "albums_done", "count": len(albums)})
    emit({"type": "done", "count": len(albums), "cursor": None})
    return EXIT_OK


# --------------------------------------------------------------------------
# Fake-Modus
# --------------------------------------------------------------------------


def fake_png(size: int, index: int) -> bytes:
    """Minimales, gueltiges Truecolor-PNG in einer Farbe.

    Handgeschrieben, damit der Fake-Modus keine Bildbibliothek braucht und
    weiterhin allein mit der Standardbibliothek laeuft.
    """
    red = (index * 67) % 256
    green = (index * 113) % 256
    blue = (index * 197) % 256
    row = b"\x00" + bytes([red, green, blue]) * size
    raw = row * size

    def chunk(tag: bytes, data: bytes) -> bytes:
        body = tag + data
        return struct.pack(">I", len(data)) + body + struct.pack(">I", zlib.crc32(body))

    header = struct.pack(">IIBBBBB", size, size, 8, 2, 0, 0, 0)
    return (
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", header)
        + chunk(b"IDAT", zlib.compress(raw))
        + chunk(b"IEND", b"")
    )


FAKE_PASSWORD = "geheim"
FAKE_2FA_CODE = "123456"


def fake_token(count: int) -> str:
    """Der Token haengt an der Bibliotheksgroesse.

    Damit laesst sich jeder Fall nachstellen: gleicher Token heisst unveraendert,
    ein hoeherer Stand bedeutet neue Fotos, ein niedrigerer Loeschungen.
    """
    return f"fake-token-{count}"


def parse_fake_token(token: str | None) -> int | None:
    if token is None or not token.startswith("fake-token-"):
        return None
    try:
        return int(token.removeprefix("fake-token-"))
    except ValueError:
        return None


def fake_record(index: int) -> dict[str, Any]:
    # Bewusst unsortiert: die echte Bibliothek liefert ebenfalls keine
    # monotone Folge von added_date. Ein Sidecar, der das voraussetzt,
    # faellt hier auf.
    added = datetime(2024, 1, 1, tzinfo=UTC) + timedelta(
        days=(index * 7919) % 365, minutes=index % 60
    )
    # Aufnahme- und Hinzufuegedatum fallen bewusst auseinander: in einer echten
    # Bibliothek tun sie das auch, und eine Ansicht, die nach dem falschen der
    # beiden sortiert, faellt hier auf.
    shot = added - timedelta(days=(index * 37) % 900)
    # Jedes neunte Asset ist ein Video — damit Kachel-Badge und Viewer-Zweig
    # ueberhaupt einen Fall bekommen.
    movie = index % 9 == 8
    # Jedes dritte Asset bekommt einen Ort, der Rest keinen — beide Faelle
    # kommen in einer echten Bibliothek vor.
    located = index % 3 == 0
    return {
        "type": "photo",
        "id": f"fake-{index:05d}",
        "filename": f"IMG_{index:04d}." + ("MOV" if movie else "HEIC"),
        "added_date": iso(added),
        "asset_date": iso(shot),
        "checksum": f"checksum-{index:05d}",
        "width": 4032,
        "height": 3024,
        "item_type": "movie" if movie else "image",
        # Streut um den Marienplatz, damit mehrere Fotos dieselbe Zelle teilen
        # und der Ortscache etwas zu tun bekommt.
        "lat": round(48.137 + (index % 7) * 0.002, 6) if located else None,
        "lon": round(11.575 + (index % 5) * 0.002, 6) if located else None,
    }


def run_fake_albums(count: int) -> int:
    """Alben fuer den Testmodus: je ein intelligentes Album, ein Ordner mit
    einem Album darin und ein Album auf oberster Ebene."""
    emit({"type": "auth_ok", "trusted": True})
    known = read_known_sizes()
    ids = [f"fake-{index:05d}" for index in range(count)]
    albums = [
        ("Favorites", "Favorites", "smart", None, ids[::5]),
        ("Videos", "Videos", "smart", None, [i for n, i in enumerate(ids) if n % 9 == 8]),
        ("fake-folder-trips", "Trips", "folder", None, None),
        ("fake-album-summer", "Summer", "album", "fake-folder-trips", ids[:20]),
        ("fake-album-food", "Food", "album", None, ids[20:32]),
    ]
    for position, (album_id, name, kind, parent, _) in enumerate(albums):
        emit(
            {
                "type": "album",
                "id": album_id,
                "name": name,
                "kind": kind,
                "parent": parent,
                "position": position,
            }
        )
    for album_id, _, _, _, members in albums:
        if members is None:
            continue
        if known.get(album_id) == len(members):
            emit({"type": "album_unchanged", "id": album_id})
        else:
            emit(
                {
                    "type": "album_members",
                    "id": album_id,
                    "size": len(members),
                    "members": members,
                }
            )
    emit({"type": "albums_done", "count": len(albums)})
    emit({"type": "done", "count": len(albums), "cursor": None})
    return EXIT_OK


def fake_thumb(index: int, thumbs: Path) -> dict[str, Any]:
    target = thumb_path_for(thumbs, f"fake-{index:05d}").with_suffix(".png")
    target.write_bytes(fake_png(64, index))
    return {
        "type": "thumb",
        "id": f"fake-{index:05d}",
        "path": str(target),
        "bytes": target.stat().st_size,
    }


def run_fake(
    count: int,
    needs_2fa: bool,
    needs_password: bool,
    thumbs: Path | None,
    since: datetime | None,
    since_token: str | None,
    offset: int,
    limit: int | None,
    delay_ms: int,
    missing: int = 0,
    needs_terms: bool = False,
) -> int:
    """Erzeugt synthetische Datensaetze ohne iCloud-Zugriff.

    Damit laesst sich die gesamte Rust-Seite inklusive 2FA-Pfad testen, ohne
    Zugangsdaten und ohne Netz.
    """
    if needs_password:
        emit({"type": "password_required", "username": "fake@example.com"})
        if read_secret() != FAKE_PASSWORD:
            emit({"type": "error", "kind": "credentials", "message": "Sign-in rejected"})
            return EXIT_AUTH
    if needs_terms:
        confirm_terms()
    if needs_2fa:
        emit({"type": "auth_required", "method": "2fa"})
        if read_secret().strip() != FAKE_2FA_CODE:
            emit({"type": "error", "kind": "auth", "message": "Two-factor code rejected"})
            return EXIT_AUTH
    emit({"type": "auth_ok", "trusted": True})
    if thumbs is not None:
        thumbs.mkdir(parents=True, exist_ok=True)

    current = fake_token(count)
    emit({"type": "cursor", "token": current})

    # Schicht 1: unveraendert.
    if since_token is not None and since_token == current:
        emit({"type": "unchanged"})
        emit({"type": "done", "count": 0, "cursor": None})
        return EXIT_OK

    # Schicht 2: Aenderungen gegenueber dem alten Stand.
    previous = parse_fake_token(since_token)
    if previous is not None:
        changed = 0
        for index in range(previous, count):
            emit(fake_record(index))
            changed += 1
            if thumbs is not None:
                emit(fake_thumb(index, thumbs))
        for index in range(count, previous):
            emit({"type": "deleted", "id": f"fake-{index:05d}"})
            changed += 1
        emit({"type": "library_count", "value": count})
        emit({"type": "done", "count": changed, "cursor": None})
        return EXIT_OK

    # Schicht 3: vollstaendiger Durchlauf ab offset.
    # `missing` meldet mehr Assets, als geliefert werden — so verhaelt sich die
    # echte Bibliothek, in der ein Teil der gezaehlten Records nie als Asset
    # ankommt. Die App darf daraus keine Endlosschleife machen.
    emit({"type": "library_count", "value": count + missing})
    newest = None
    emitted = 0
    truncated = False
    for index in range(offset, count):
        if limit is not None and emitted >= limit:
            truncated = True
            break
        # Bewusst unsortiert: die echte Bibliothek liefert ebenfalls keine
        # monotone Folge von added_date. Ein Sidecar, der das voraussetzt,
        # faellt hier auf.
        record = fake_record(index)
        added = parse_since(record["added_date"])
        # Wie im Echtbetrieb: filtern, aber den Durchlauf nicht abbrechen.
        if since is not None and added is not None and added <= since:
            continue
        emitted += 1
        newest = added if newest is None or (added is not None and added > newest) else newest
        emit(record)
        if thumbs is not None:
            emit(fake_thumb(index, thumbs))
        if emitted % OFFSET_REPORT_EVERY == 0:
            emit({"type": "offset", "value": index + 1})
        if delay_ms:
            # Macht das Einlaufen in der UI ueberhaupt beobachtbar; ein echter
            # Sync ist ohnehin langsamer als der Fake.
            time.sleep(delay_ms / 1000.0)

    if truncated:
        emit({"type": "offset", "value": offset + emitted})
    else:
        emit({"type": "baseline_complete", "count": emitted})
    emit({"type": "done", "count": emitted, "cursor": iso(newest)})
    return EXIT_OK


def die_with_parent() -> None:
    """Endet mit der App, statt sie zu ueberleben.

    Die App beendet ihre Sidecars beim Aufraeumen. Wird sie aber einfach
    geschlossen, laeuft dieses Aufraeumen nicht mehr — ein Sidecar, der gerade
    in einer langen Anfrage steckte, lief dann verwaist weiter, einmal ueber
    eine halbe Stunde. ``PR_SET_PDEATHSIG`` laesst den Kernel ihm SIGTERM
    schicken, sobald die App weg ist.
    """
    try:
        import ctypes
        import signal

        pr_set_pdeathsig = 1
        libc = ctypes.CDLL("libc.so.6", use_errno=True)
        libc.prctl(pr_set_pdeathsig, signal.SIGTERM)
    except (OSError, AttributeError):  # pragma: no cover - nicht Linux
        return
    # War die App schon weg, bevor das griff, kommt kein Signal mehr.
    if os.getppid() == 1:
        sys.exit(EXIT_PIPE)


def main(argv: list[str] | None = None) -> int:
    die_with_parent()
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--username", help="Apple-ID")
    parser.add_argument("--cookie-dir", help="Verzeichnis fuer die Session-Cookies")
    parser.add_argument("--since", help="Nur Assets neuer als dieser ISO-8601-Zeitpunkt")
    parser.add_argument("--limit", type=int, help="Hoechstens so viele Assets ausgeben")
    parser.add_argument(
        "--fake",
        type=int,
        metavar="N",
        help="Kein iCloud-Zugriff, sondern N synthetische Datensaetze (Tests)",
    )
    parser.add_argument(
        "--fake-2fa",
        action="store_true",
        help="Im Fake-Modus den 2FA-Dialog durchspielen (Code: 123456)",
    )
    parser.add_argument(
        "--fake-password",
        action="store_true",
        help=f"Im Fake-Modus nach dem Passwort fragen (korrekt: {FAKE_PASSWORD})",
    )
    parser.add_argument(
        "--fake-terms",
        action="store_true",
        help="Im Fake-Modus die Zustimmung zu geaenderten Nutzungsbedingungen verlangen",
    )
    parser.add_argument(
        "--thumbs",
        metavar="DIR",
        help="Vorschaubilder in dieses Verzeichnis laden",
    )
    parser.add_argument(
        "--fake-missing",
        type=int,
        default=0,
        metavar="N",
        help="Im Fake-Modus N Assets mehr zaehlen als liefern (Bestandsluecke)",
    )
    parser.add_argument(
        "--fake-delay-ms",
        type=int,
        default=0,
        help="Im Fake-Modus je Asset so lange warten (macht das Einlaufen sichtbar)",
    )
    parser.add_argument(
        "--since-token",
        metavar="TOKEN",
        help="CloudKit-syncToken des letzten Laufs; gleich = nichts zu tun",
    )
    parser.add_argument(
        "--offset",
        type=int,
        default=0,
        help="Erstaufbau an dieser Position fortsetzen",
    )
    parser.add_argument(
        "--force-baseline",
        action="store_true",
        help="Delta ueberspringen und vollstaendig durchlaufen",
    )
    parser.add_argument(
        "--fetch-thumbs",
        action="store_true",
        help="Nur Vorschauen nachladen; Asset-IDs kommen zeilenweise ueber stdin",
    )
    parser.add_argument(
        "--albums",
        action="store_true",
        help="Nur Alben und ihre Mitglieder melden",
    )
    parser.add_argument(
        "--serve",
        action="store_true",
        help="Dauerhaft laufen und Befehle von stdin beantworten (Originale auf Abruf)",
    )
    args = parser.parse_args(argv)

    thumbs = Path(args.thumbs).expanduser() if args.thumbs else None

    if args.fake is not None:
        if args.albums:
            return run_fake_albums(args.fake)
        if args.serve:
            return run_fake_serve(args.fake, args.fake_password, args.fake_terms)
        return run_fake(
            args.fake,
            args.fake_2fa,
            args.fake_password,
            thumbs,
            parse_since(args.since),
            args.since_token,
            args.offset,
            args.limit,
            args.fake_delay_ms,
            args.fake_missing,
            args.fake_terms,
        )

    if not args.username:
        parser.error("--username ist ohne --fake erforderlich")
    if args.fetch_thumbs and thumbs is None:
        parser.error("--fetch-thumbs braucht --thumbs")

    try:
        if args.serve:
            return run_serve(args.username, args.cookie_dir)
        if args.albums:
            return run_albums(args.username, args.cookie_dir)
        if args.fetch_thumbs:
            assert thumbs is not None
            return run_fetch_thumbs(args.username, args.cookie_dir, thumbs)
        return run_sync(
            args.username,
            args.cookie_dir,
            parse_since(args.since),
            args.limit,
            thumbs,
            args.since_token,
            args.offset,
            args.force_baseline,
        )
    except SystemExit:
        raise
    except Exception as exc:  # pragma: no cover - Netz-/API-Fehler
        emit({"type": "error", "kind": "other", "message": f"{type(exc).__name__}: {exc}"})
        return EXIT_ERROR


if __name__ == "__main__":
    sys.exit(main())
