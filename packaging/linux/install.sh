#!/bin/sh
# Installs Roch CPU for the current user: the binary to ~/.local/bin/rochcpu, the launcher entry and
# its icon. Nothing runs as root here; the program asks for administrator access (pkexec) itself at
# each launch. Works from the release tarball (everything beside this script) and from the
# repository after `cargo build --release --locked`; or pass the binary.
set -eu
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
if [ -x "$here/rochcpu" ]; then default_bin="$here/rochcpu"; else default_bin="$root/target/release/roch-cpu"; fi
if [ -f "$here/RochCPU.png" ]; then icon="$here/RochCPU.png"; else icon="$root/assets/mark.png"; fi
bin=${1:-"$default_bin"}
data=${XDG_DATA_HOME:-"$HOME/.local/share"}
install -Dm755 "$bin" "$HOME/.local/bin/rochcpu"
sed "s|^Exec=.*|Exec=\"$HOME/.local/bin/rochcpu\"|" "$here/RochCPU.desktop" > "$here/.RochCPU.desktop.tmp"
install -Dm644 "$here/.RochCPU.desktop.tmp" "$data/applications/RochCPU.desktop"
rm -f "$here/.RochCPU.desktop.tmp"
install -Dm644 "$icon" "$data/icons/hicolor/256x256/apps/RochCPU.png"
command -v update-desktop-database >/dev/null && update-desktop-database "$data/applications" || true
echo "Installed: $HOME/.local/bin/rochcpu (launcher: Roch CPU)"
if [ -e /usr/local/bin/rochcpu ]; then
  echo "Run 'sudo $HOME/.local/bin/rochcpu setup' again so the password-free helper matches this version."
fi
