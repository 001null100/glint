#!/usr/bin/env bash
# Build Glint and install it for the current user.
#   ./install.sh              build + install binary, desktop entry and icon
#   ./install.sh --uninstall  remove those three files
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
data="${XDG_DATA_HOME:-$HOME/.local/share}"
bin="$HOME/.local/bin/glint"
desktop="$data/applications/glint.desktop"
icon="$data/icons/hicolor/scalable/apps/glint.svg"

refresh() {
    if command -v update-desktop-database >/dev/null 2>&1; then
        update-desktop-database "$data/applications" >/dev/null 2>&1 || true
    fi
}

case "${1:-}" in
    --uninstall)
        for f in "$bin" "$desktop" "$icon"; do
            if [ -e "$f" ]; then
                rm -f "$f"
                echo "removed $f"
            fi
        done
        refresh
        echo "Glint uninstalled. Settings (~/.config/glint) and session data (~/.local/share/glint) were left alone."
        exit 0
        ;;
    "" ) ;;
    -h|--help)
        sed -n '2,5p' "$0" | sed 's/^# \{0,1\}//'
        exit 0
        ;;
    *)
        echo "unknown option: $1 (try --help)" >&2
        exit 2
        ;;
esac

cd "$here"
cargo build --release

install -Dm755 target/release/glint "$bin"
echo "installed $bin"
install -Dm644 assets/glint.desktop "$desktop"
echo "installed $desktop"
install -Dm644 assets/glint.svg "$icon"
echo "installed $icon"
refresh

case ":$PATH:" in
    *":$HOME/.local/bin:"*) ;;
    *) echo "note: $HOME/.local/bin is not on your PATH" ;;
esac
echo "Done. Run 'glint' or pick Glint from the application launcher."
