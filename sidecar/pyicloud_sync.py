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
import itertools
import json
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
    sys.stdout.write(line + "\n")
    sys.stdout.flush()


def log(message: str) -> None:
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

    return {
        "type": "photo",
        "id": asset.id,
        "filename": asset.filename,
        "added_date": iso(asset.added_date),
        "asset_date": iso(asset.asset_date),
        "checksum": checksum,
        "width": width,
        "height": height,
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
    return {
        "type": "photo",
        "id": f"fake-{index:05d}",
        "filename": f"IMG_{index:04d}.HEIC",
        "added_date": iso(added),
        "asset_date": iso(added),
        "checksum": f"checksum-{index:05d}",
        "width": 4032,
        "height": 3024,
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
    args = parser.parse_args(argv)

    thumbs = Path(args.thumbs).expanduser() if args.thumbs else None

    if args.fake is not None:
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
