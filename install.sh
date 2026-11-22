#!/usr/bin/env bash
# Build sonos-bridge and install it for the current user.
#   ./install.sh            build + install + systemd user service (enabled)
#   ./install.sh --no-enable   install but do not start at login
set -euo pipefail

cd "$(dirname "$0")"
ENABLE=1
for a in "$@"; do
  case "$a" in
    --no-enable) ENABLE=0 ;;
    *) echo "unknown option: $a" >&2; exit 2 ;;
  esac
done

say() { printf '\033[1m==>\033[0m %s\n' "$*"; }
warn() { printf '\033[1m!!\033[0m %s\n' "$*" >&2; }

missing=()
command -v cargo  >/dev/null || missing+=("cargo (rustup: https://rustup.rs)")
command -v pactl  >/dev/null || missing+=("pactl (sudo apt install pulseaudio-utils)")
command -v ffmpeg >/dev/null || missing+=("ffmpeg (sudo apt install ffmpeg)")
if ! command -v parec >/dev/null && ! command -v pw-record >/dev/null; then
  missing+=("parec or pw-record (sudo apt install pulseaudio-utils pipewire-tools)")
fi
if ((${#missing[@]})); then
  warn "missing prerequisites:"
  for m in "${missing[@]}"; do echo "   - $m"; done
  exit 1
fi

say "building (release)"
cargo build --release

say "offline self checks"
./target/release/sonos-bridge selftest

BINDIR="$HOME/.local/bin"
mkdir -p "$BINDIR"
install -m 755 target/release/sonos-bridge "$BINDIR/sonos-bridge"
say "installed $BINDIR/sonos-bridge"

# The desktop panel looks for the daemon beside itself first, so installing
# both into the same directory is what makes its "Start daemon" button work
# from a menu, with no terminal and no PATH assumptions.
GUI_BIN=""
if [ -d gui ]; then
  say "building the desktop panel (gui)"
  if (cd gui && cargo build --release --bin sonos-bridge-gui >/dev/null); then
    install -m 755 gui/target/release/sonos-bridge-gui "$BINDIR/sonos-bridge-gui"
    GUI_BIN="$BINDIR/sonos-bridge-gui"
    say "installed $BINDIR/sonos-bridge-gui"
    APPS="$HOME/.local/share/applications"
    mkdir -p "$APPS"
    cat > "$APPS/sonos-bridge-gui.desktop" <<EOF
[Desktop Entry]
Type=Application
Name=Sonos Bridge
Comment=Route system audio to Sonos speakers
Exec=$GUI_BIN
TryExec=$GUI_BIN
Terminal=false
Categories=AudioVideo;Utility;
EOF
    update-desktop-database "$APPS" 2>/dev/null || true
    say "menu entry: $APPS/sonos-bridge-gui.desktop"
  else
    warn "desktop panel did not build (needs the gpui system packages); skipping"
  fi
fi

if command -v systemctl >/dev/null && [ "${XDG_SESSION_TYPE:-}" != "" ] || [ -d "$HOME/.config/systemd/user" ]; then
  UNITDIR="$HOME/.config/systemd/user"
  mkdir -p "$UNITDIR"
  sed "s#@EXE@#$BINDIR/sonos-bridge#" systemd/sonos-bridge.service > "$UNITDIR/sonos-bridge.service"
  systemctl --user daemon-reload
  if [ "$ENABLE" = "1" ]; then
    systemctl --user enable --now sonos-bridge
    say "service running (systemctl --user status sonos-bridge)"
  else
    say "unit installed at $UNITDIR/sonos-bridge.service (not enabled)"
  fi
else
  warn "no systemd user session; start it manually with: sonos-bridge start"
fi

# desktop autostart entry (harmless if you use the unit)
AUTOSTART="$HOME/.config/autostart"
mkdir -p "$AUTOSTART"
cat > "$AUTOSTART/sonos-bridge.desktop" <<EOF
[Desktop Entry]
Type=Application
Name=sonos-bridge
Comment=Route system audio to Sonos
Exec=$BINDIR/sonos-bridge start
Hidden=false
NoDisplay=false
X-GNOME-Autostart-enabled=true
EOF

say "next steps"
cat <<EOF
  sonos-bridge check            # does the machine look sane?
  sonos-bridge discover         # find the speakers
  sonos-bridge play "Living Room"
  sonos-bridge open             # web panel, or: sonos-bridge-gui (desktop app)
  sonos-bridge stop             # hand audio back to the normal speakers
  sonos-bridge shutdown         # stop the daemon
EOF
