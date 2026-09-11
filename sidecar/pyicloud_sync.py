#!/usr/bin/env python3
"""Liest Foto-Metadaten aus iCloud und gibt sie als NDJSON auf stdout aus.

Das Protokoll ist zeilenweises JSON; jede Zeile hat ein Feld ``type``:

    {"type": "auth_required", "method": "2fa"}   -> erwartet Code auf stdin
    {"type": "auth_ok", "trusted": true}
    {"type": "photo", "id": ..., "filename": ...}
    {"type": "done", "count": 1234, "cursor": "2026-01-02T03:04:05+00:00"}
    {"type": "error", "kind": "auth", "message": "..."}

stdout gehoert ausschliesslich dem Protokoll. Alles Menschenlesbare geht nach
stderr, sonst zerreisst es den Parser auf der Rust-Seite.

Kein Bilddownload — dieser Schritt liefert nur Metadaten.
"""

from __future__ import annotations

import argparse
import json
import sys
from collections.abc import Iterator
from datetime import UTC, datetime
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


def authenticate(username: str, cookie_dir: str | None):
    """Meldet sich an und erledigt 2FA interaktiv ueber stdin."""
    from pyicloud import PyiCloudService
    from pyicloud.utils import get_password_from_keyring

    password = get_password_from_keyring(username)
    if password is None:
        emit(
            {
                "type": "error",
                "kind": "auth",
                "message": (
                    f"Kein Passwort im Keyring fuer {username}. "
                    "Einmalig hinterlegen, z. B. mit `icloud --username ...`."
                ),
            }
        )
        sys.exit(EXIT_AUTH)

    api = PyiCloudService(username, password, cookie_directory=cookie_dir)

    if api.requires_2fa:
        emit({"type": "auth_required", "method": "2fa"})
        code = sys.stdin.readline().strip()
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


def iter_assets(api: Any, since: datetime | None, limit: int | None) -> Iterator[Any]:
    """Liefert Assets, neueste zuerst, und bricht am Cursor ab.

    ``photos.all`` liefert absteigend nach ``added_date``. Sobald ein Asset
    aelter als der Cursor ist, sind alle folgenden es auch — dann kann der
    Durchlauf abbrechen, statt die ganze Bibliothek zu paginieren.
    """
    for seen, asset in enumerate(api.photos.all):
        if since is not None and asset.added_date <= since:
            log(f"Cursor erreicht nach {seen} Assets")
            return
        yield asset
        if limit is not None and seen + 1 >= limit:
            log(f"Limit {limit} erreicht")
            return


def run_sync(
    username: str, cookie_dir: str | None, since: datetime | None, limit: int | None
) -> int:
    api = authenticate(username, cookie_dir)

    count = 0
    newest: datetime | None = None
    for asset in iter_assets(api, since, limit):
        record = record_for(asset)
        emit(record)
        count += 1
        if newest is None or asset.added_date > newest:
            newest = asset.added_date

    emit({"type": "done", "count": count, "cursor": iso(newest) or iso(since)})
    return EXIT_OK


# --------------------------------------------------------------------------
# Fake-Modus
# --------------------------------------------------------------------------


def run_fake(count: int, needs_2fa: bool) -> int:
    """Erzeugt synthetische Datensaetze ohne iCloud-Zugriff.

    Damit laesst sich die gesamte Rust-Seite inklusive 2FA-Pfad testen, ohne
    Zugangsdaten und ohne Netz.
    """
    if needs_2fa:
        emit({"type": "auth_required", "method": "2fa"})
        code = sys.stdin.readline().strip()
        if code != "123456":
            emit({"type": "error", "kind": "auth", "message": "2FA-Code abgelehnt"})
            return EXIT_AUTH
    emit({"type": "auth_ok", "trusted": True})

    newest = None
    for index in range(count):
        added = datetime(2026, 1, 1, tzinfo=UTC).replace(minute=index % 60, hour=(index // 60) % 24)
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
    emit({"type": "done", "count": count, "cursor": iso(newest)})
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
    args = parser.parse_args(argv)

    if args.fake is not None:
        return run_fake(args.fake, args.fake_2fa)

    if not args.username:
        parser.error("--username ist ohne --fake erforderlich")

    try:
        return run_sync(args.username, args.cookie_dir, parse_since(args.since), args.limit)
    except SystemExit:
        raise
    except Exception as exc:  # pragma: no cover - Netz-/API-Fehler
        emit({"type": "error", "kind": "other", "message": f"{type(exc).__name__}: {exc}"})
        return EXIT_ERROR


if __name__ == "__main__":
    sys.exit(main())
