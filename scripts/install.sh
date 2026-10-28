#!/usr/bin/env bash
# Installiert iCloud Photos für den eigenen Nutzer, sodass der Launcher
# (Omarchy: Super+Space) die App über ihren .desktop-Eintrag findet.
#
# Mehrfach aufrufbar: jeder Lauf baut neu und ersetzt, was schon liegt. Nach
# jedem `git pull` erneut ausführen, sonst startet der Launcher den alten Stand.
set -euo pipefail

APP_ID=de.henri.IcloudPhotos

repo=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)
data=${XDG_DATA_HOME:-$HOME/.local/share}
bin_dir=$data/icloud-photos/bin
binary=$bin_dir/icloud-photos
desktop_dir=$data/applications
icon_dir=$data/icons/hicolor/scalable/apps

die() {
    printf 'install.sh: %s\n' "$*" >&2
    exit 1
}

# Das Binary findet Sidecar und venv über den Projektpfad, der beim Bauen
# einkompiliert wird (`project_root()` in sync_task.rs). Ohne venv fiele es
# still auf das System-Python zurück, dem pyicloud fehlt — der erste Sync
# scheiterte dann mit einem Importfehler statt hier mit einem klaren Hinweis.
if [[ ! -x $repo/.venv/bin/python ]]; then
    die "Python-Umgebung fehlt ($repo/.venv). Erst einrichten:
    cd '$repo' && python3 -m venv .venv && .venv/bin/pip install -r sidecar/requirements.txt"
fi

# Ein Worktree wird irgendwann weggeräumt — und mit ihm der einkompilierte Pfad.
if [[ $repo == */.claude/worktrees/* ]]; then
    printf 'install.sh: Warnung: %s ist ein Worktree; das Binary zeigt dauerhaft dorthin.\n' \
        "$repo" >&2
fi

# Exec wird in Anführungszeichen gesetzt, damit Leerzeichen im Pfad nicht
# stören. Zeichen, die die Desktop-Spezifikation darin zusätzlich escapen
# ließe, kommen in einem Home-Verzeichnis praktisch nie vor — abweisen ist
# ehrlicher als eine halb richtige Escape-Logik.
if [[ $binary == *[\"\`\$\\%]* || $binary == *$'\n'* ]]; then
    die "Pfad enthält Zeichen, die im Exec-Feld nicht unterstützt werden: $binary"
fi

command -v cargo >/dev/null || die "cargo nicht gefunden"

echo "Baue Release-Binary in $repo …"
(cd "$repo" && cargo build --release -p icloud-photos-app)

# Kopie statt Symlink ins target/: ein `cargo clean` oder ein Debug-Build im
# selben Verzeichnis soll den Launcher nicht still brechen. `install` legt die
# Datei neu an, statt sie zu überschreiben — das geht auch, während die App
# läuft („Text file busy").
install -D -m 0755 "$repo/target/release/icloud-photos-app" "$binary"

# Datei nach Application-ID benannt: unter Wayland ist die app_id des Fensters
# die GApplication-ID, nur so ordnen Launcher und Compositor Fenster und Symbol
# dem Eintrag zu.
template=$(<"$repo/data/$APP_ID.desktop")
entry=${template//@EXEC@/"\"$binary\""}
entry=${entry//@BINARY@/"$binary"}
mkdir -p "$desktop_dir"
tmp=$(mktemp "$desktop_dir/.$APP_ID.XXXXXX")
printf '%s\n' "$entry" >"$tmp"
chmod 0644 "$tmp"
mv -f "$tmp" "$desktop_dir/$APP_ID.desktop"

# Ein eigenes Icon, falls unter ~/.config/icloud-photos/ eines liegt. Es
# bleibt bewusst außerhalb des Repos: wer etwa Apples Photos-Symbol nutzen
# will, soll dessen Marke nicht mit dem Code verteilen.
config=${XDG_CONFIG_HOME:-$HOME/.config}/icloud-photos
png_dir=$data/icons/hicolor/512x512/apps
if [[ -f $config/icon.svg ]]; then
    install -D -m 0644 "$config/icon.svg" "$icon_dir/$APP_ID.svg"
    rm -f "$png_dir/$APP_ID.png"
    echo "Eigenes Icon: $config/icon.svg"
elif [[ -f $config/icon.png ]]; then
    mkdir -p "$png_dir" "$icon_dir"
    if command -v magick >/dev/null; then
        magick "$config/icon.png" -resize 512x512 "$png_dir/$APP_ID.png"
    else
        install -m 0644 "$config/icon.png" "$png_dir/$APP_ID.png"
    fi
    # Ein SVG in scalable/ gewinnt gegen jede Rastergrafik — auch das des
    # Arch-Pakets unter /usr/share. Deshalb dieselbe Grafik als SVG-Hülle
    # hier ablegen; sie verdeckt das System-Symbol.
    {
        printf '<svg xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink" width="512" height="512" viewBox="0 0 512 512">'
        printf '<image width="512" height="512" xlink:href="data:image/png;base64,'
        base64 -w0 "$png_dir/$APP_ID.png"
        printf '"/></svg>\n'
    } > "$icon_dir/$APP_ID.svg"
    echo "Eigenes Icon: $config/icon.png"
else
    install -D -m 0644 "$repo/data/icons/$APP_ID.svg" "$icon_dir/$APP_ID.svg"
    rm -f "$png_dir/$APP_ID.png"
fi

if command -v desktop-file-validate >/dev/null; then
    desktop-file-validate "$desktop_dir/$APP_ID.desktop" ||
        printf 'install.sh: Warnung: desktop-file-validate beanstandet den Eintrag\n' >&2
fi

# Beide Caches sind optional; ohne sie findet der Launcher den Eintrag trotzdem,
# nur eventuell erst nach einem Neuaufbau seines eigenen Index.
if command -v update-desktop-database >/dev/null; then
    update-desktop-database -q "$desktop_dir" || true
fi
if command -v gtk-update-icon-cache >/dev/null; then
    gtk-update-icon-cache -q -f -t "$data/icons/hicolor" || true
fi

echo "Installiert:"
echo "  $binary"
echo "  $desktop_dir/$APP_ID.desktop"
echo "  Icon unter $data/icons/hicolor/"
