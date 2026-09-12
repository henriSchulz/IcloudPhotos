#!/usr/bin/env python3
"""Liest Foto-Metadaten aus iCloud und gibt sie als NDJSON auf stdout aus.

Das Protokoll ist zeilenweises JSON; jede Zeile hat ein Feld ``type``:

    {"type": "password_required", "username": "..."}  -> erwartet Passwort auf stdin
    {"type": "auth_required", "method": "2fa"}   -> erwartet Code auf stdin
    {"type": "auth_ok", "trusted": true}
    {"type": "photo", "id": ..., "filename": ...}
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
from collections.abc import Iterator
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
                "message": f"Nachricht nicht serialisierbar: {exc}",
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


def authenticate(username: str, cookie_dir: str | None):
    """Meldet sich an und erledigt Passwort- und 2FA-Abfrage ueber stdin."""
    from pyicloud import PyiCloudService
    from pyicloud.exceptions import PyiCloudFailedLoginException
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
            emit({"type": "error", "kind": "auth", "message": "Kein Passwort erhalten"})
            sys.exit(EXIT_AUTH)

    try:
        api = PyiCloudService(username, password, cookie_directory=cookie_dir)
    except PyiCloudFailedLoginException as exc:
        # Eigene Fehlerart, damit die App gezielt erneut nach dem Passwort
        # fragen kann statt den Sync als kaputt zu melden.
        emit({"type": "error", "kind": "credentials", "message": str(exc) or "Anmeldung abgelehnt"})
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
            emit({"type": "error", "kind": "auth", "message": "Kein 2FA-Code erhalten"})
            sys.exit(EXIT_AUTH)
        if not api.validate_2fa_code(code):
            emit({"type": "error", "kind": "auth", "message": "2FA-Code abgelehnt"})
            sys.exit(EXIT_AUTH)
        # Ohne trust_session laeuft die Sitzung nach Stunden ab und jeder Sync
        # fragt erneut nach einem Code.
        if not api.is_trusted_session:
            api.trust_session()

    elif api.requires_2sa:
        emit(
            {
                "type": "error",
                "kind": "auth",
                "message": "Konto nutzt 2SA (alte Zwei-Schritt-Pruefung), nicht unterstuetzt",
            }
        )
        sys.exit(EXIT_AUTH)

    emit({"type": "auth_ok", "trusted": bool(api.is_trusted_session)})
    return api


# --------------------------------------------------------------------------
# Metadaten
# --------------------------------------------------------------------------


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
        return {"type": "thumb_failed", "id": asset.id, "message": "keine Vorschau verfuegbar"}

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
    source = api.photos.all
    if offset:
        # Serverseitiger Versatz: ein abgebrochener Erstaufbau muss nicht die
        # bereits erledigten Seiten erneut holen.
        source = itertools.islice(source, offset, None)
    for asset in source:
        if since is not None and asset.added_date <= since:
            continue
        yield asset
        emitted += 1
        if limit is not None and emitted >= limit:
            log(f"Limit {limit} erreicht")
            return


# Alle so viele Assets meldet der Erstaufbau seinen Fortschritt, damit ein
# Abbruch nicht den ganzen Durchlauf kostet.
OFFSET_REPORT_EVERY = 50


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
    for index, asset in enumerate(iter_assets(api, None, limit, offset=offset), start=offset):
        emit(record_for(asset))
        count += 1
        if thumbs is not None:
            emit(download_thumb(asset, thumbs))
        if count % OFFSET_REPORT_EVERY == 0:
            emit({"type": "offset", "value": index + 1})

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
                    "message": "kein Asset",
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
        return original_failed(asset.id, "kein Original verfuegbar", kind="missing")

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

    def __init__(self, api: Any) -> None:
        self._api = api
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
            emit({"type": "error", "kind": "protocol", "message": f"unlesbarer Befehl: {exc}"})
            continue

        name = command.get("cmd")
        if name == "quit":
            break
        if name != "original":
            emit({"type": "error", "kind": "protocol", "message": f"unbekannter Befehl: {name}"})
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
            emit(original_failed(asset_id, "Asset nicht gefunden", kind="missing"))
            continue

        emit(download_original(asset, Path(dest)))

    return EXIT_OK


# Eine Asset-ID, die im ersten Prozess scheitert und im zweiten liefert.
# Damit laesst sich die abgelaufene Sitzung nachstellen: die App muss den
# Sidecar wegwerfen und frisch anmelden, sonst kommt das Foto nie.
FAKE_FLAKY_ID = "fake-flaky"


def run_fake_serve(count: int, needs_password: bool) -> int:
    """Serve-Modus ohne iCloud: liefert erzeugte Originale."""
    if needs_password:
        emit({"type": "password_required", "username": "fake@example.com"})
        if read_secret() != FAKE_PASSWORD:
            emit({"type": "error", "kind": "credentials", "message": "Anmeldung abgelehnt"})
            return EXIT_AUTH
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
                emit(original_failed(asset_id, "Sitzung abgelaufen (nachgestellt)"))
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
            emit(original_failed(asset_id, "Asset nicht gefunden", kind="missing"))
            continue
        number = int(asset_id.removeprefix("fake-"))
        if number >= count:
            emit(original_failed(asset_id, "Asset nicht gefunden", kind="missing"))
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
    album = api.photos.all

    count = 0
    for line in sys.stdin:
        asset_id = line.strip()
        if not asset_id:
            continue
        try:
            asset = album.get(asset_id)
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
            emit({"type": "thumb_failed", "id": asset_id, "message": "Asset nicht gefunden"})
            continue
        emit(download_thumb(asset, thumbs))
        count += 1

    emit({"type": "done", "count": count, "cursor": None})
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
    }


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
) -> int:
    """Erzeugt synthetische Datensaetze ohne iCloud-Zugriff.

    Damit laesst sich die gesamte Rust-Seite inklusive 2FA-Pfad testen, ohne
    Zugangsdaten und ohne Netz.
    """
    if needs_password:
        emit({"type": "password_required", "username": "fake@example.com"})
        if read_secret() != FAKE_PASSWORD:
            emit({"type": "error", "kind": "credentials", "message": "Anmeldung abgelehnt"})
            return EXIT_AUTH
    if needs_2fa:
        emit({"type": "auth_required", "method": "2fa"})
        if read_secret().strip() != FAKE_2FA_CODE:
            emit({"type": "error", "kind": "auth", "message": "2FA-Code abgelehnt"})
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
    emit({"type": "library_count", "value": count})
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


def main(argv: list[str] | None = None) -> int:
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
        "--thumbs",
        metavar="DIR",
        help="Vorschaubilder in dieses Verzeichnis laden",
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
        "--serve",
        action="store_true",
        help="Dauerhaft laufen und Befehle von stdin beantworten (Originale auf Abruf)",
    )
    args = parser.parse_args(argv)

    thumbs = Path(args.thumbs).expanduser() if args.thumbs else None

    if args.fake is not None:
        if args.serve:
            return run_fake_serve(args.fake, args.fake_password)
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
        )

    if not args.username:
        parser.error("--username ist ohne --fake erforderlich")
    if args.fetch_thumbs and thumbs is None:
        parser.error("--fetch-thumbs braucht --thumbs")

    try:
        if args.serve:
            return run_serve(args.username, args.cookie_dir)
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
