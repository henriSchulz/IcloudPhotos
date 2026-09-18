#!/usr/bin/env bash
# Nimmt zurück, was install.sh angelegt hat — und nur das. Katalog, Cookies und
# Caches (~/.local/share/icloud-photos, ~/.cache/icloud-photos) bleiben liegen:
# ein Neuinstallieren soll nicht die ganze Bibliothek neu laden müssen.
set -euo pipefail

APP_ID=de.henri.IcloudPhotos

data=${XDG_DATA_HOME:-$HOME/.local/share}
bin_dir=$data/icloud-photos/bin
desktop_dir=$data/applications

rm -f "$bin_dir/icloud-photos" \
    "$desktop_dir/$APP_ID.desktop" \
    "$data/icons/hicolor/scalable/apps/$APP_ID.svg"

# Nur das eigene bin/ entfernen, und nur wenn es leer ist; das Elternverzeichnis
# enthält den Katalog.
rmdir "$bin_dir" 2>/dev/null || true

if command -v update-desktop-database >/dev/null && [[ -d $desktop_dir ]]; then
    update-desktop-database -q "$desktop_dir" || true
fi
if command -v gtk-update-icon-cache >/dev/null && [[ -d $data/icons/hicolor ]]; then
    gtk-update-icon-cache -q -f -t "$data/icons/hicolor" || true
fi

echo "Entfernt. Katalog und Caches bleiben erhalten."
