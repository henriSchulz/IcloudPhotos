#!/usr/bin/env python3
"""Liest Foto-Metadaten aus iCloud und gibt sie als NDJSON auf stdout aus.

Das Protokoll ist zeilenweises JSON; jede Zeile hat ein Feld ``type``:

    {"type": "password_required", "username": "..."}  -> erwartet Passwort auf stdin
    {"type": "auth_required", "method": "2fa"}   -> erwartet Code auf stdin
    {"type": "auth_ok", "trusted": true}
    {"type": "photo", "id": ..., "filename": ...}
    {"type": "thumb", "id": ..., "path": "...", "bytes": 34567}
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
import json
import struct
import sys
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

    Ohne Flush puffert Python bei Pipe-stdout bis zu 8 KB, und die UI zeigt
    minutenlang keinen Fortschritt.
    """
    json.dump(payload, sys.stdout, separators=(",", ":"), ensure_ascii=False)
    sys.stdout.write("\n")
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


def iter_assets(api: Any, since: datetime | None, limit: int | None) -> Iterator[Any]:
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
    for asset in api.photos.all:
        if since is not None and asset.added_date <= since:
            continue
        yield asset
        emitted += 1
        if limit is not None and emitted >= limit:
            log(f"Limit {limit} erreicht")
            return


def run_sync(
    username: str,
    cookie_dir: str | None,
    since: datetime | None,
    limit: int | None,
    thumbs: Path | None,
) -> int:
    api = authenticate(username, cookie_dir)
    if thumbs is not None:
        thumbs.mkdir(parents=True, exist_ok=True)

    count = 0
    newest: datetime | None = None
    for asset in iter_assets(api, since, limit):
        emit(record_for(asset))
        count += 1
        if newest is None or asset.added_date > newest:
            newest = asset.added_date
        # Erst die Metadaten, dann die Vorschau: faellt der Download aus, steht
        # das Foto trotzdem schon im Katalog.
        if thumbs is not None:
            emit(download_thumb(asset, thumbs))

    emit({"type": "done", "count": count, "cursor": iso(newest) or iso(since)})
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


def run_fake(
    count: int,
    needs_2fa: bool,
    needs_password: bool,
    thumbs: Path | None,
    since: datetime | None,
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

    newest = None
    emitted = 0
    for index in range(count):
        # Bewusst unsortiert: die echte Bibliothek liefert ebenfalls keine
        # monotone Folge von added_date. Ein Sidecar, der das voraussetzt,
        # faellt hier auf.
        added = datetime(2024, 1, 1, tzinfo=UTC) + timedelta(
            days=(index * 7919) % 365, minutes=index % 60
        )
        # Wie im Echtbetrieb: filtern, aber den Durchlauf nicht abbrechen.
        if since is not None and added <= since:
            continue
        emitted += 1
        newest = added if newest is None or added > newest else newest
        emit(
            {
                "type": "photo",
                "id": f"fake-{index:05d}",
                "filename": f"IMG_{index:04d}.HEIC",
                "added_date": iso(added),
                "asset_date": iso(added),
                "checksum": f"checksum-{index:05d}",
                "width": 4032,
                "height": 3024,
            }
        )
        if thumbs is not None:
            target = thumb_path_for(thumbs, f"fake-{index:05d}").with_suffix(".png")
            target.write_bytes(fake_png(64, index))
            emit(
                {
                    "type": "thumb",
                    "id": f"fake-{index:05d}",
                    "path": str(target),
                    "bytes": target.stat().st_size,
                }
            )
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
        "--fetch-thumbs",
        action="store_true",
        help="Nur Vorschauen nachladen; Asset-IDs kommen zeilenweise ueber stdin",
    )
    args = parser.parse_args(argv)

    thumbs = Path(args.thumbs).expanduser() if args.thumbs else None

    if args.fake is not None:
        return run_fake(
            args.fake, args.fake_2fa, args.fake_password, thumbs, parse_since(args.since)
        )

    if not args.username:
        parser.error("--username ist ohne --fake erforderlich")
    if args.fetch_thumbs and thumbs is None:
        parser.error("--fetch-thumbs braucht --thumbs")

    try:
        if args.fetch_thumbs:
            assert thumbs is not None
            return run_fetch_thumbs(args.username, args.cookie_dir, thumbs)
        return run_sync(args.username, args.cookie_dir, parse_since(args.since), args.limit, thumbs)
    except SystemExit:
        raise
    except Exception as exc:  # pragma: no cover - Netz-/API-Fehler
        emit({"type": "error", "kind": "other", "message": f"{type(exc).__name__}: {exc}"})
        return EXIT_ERROR


if __name__ == "__main__":
    sys.exit(main())
