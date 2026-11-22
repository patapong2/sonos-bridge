#!/usr/bin/env bash
# Audio-graph diagnostic: what the sound server exposes, and whether each
# capture backend sonos-bridge knows about can actually read our sink monitor.
#
#   ./scripts/pipewire-diag.sh [sink-name]     (default: sonos_bridge)
#
# Run `sonos-bridge route sonos <player>` first, so the sink exists.

set -uo pipefail

SINK="${1:-sonos_bridge}"
MON="$SINK.monitor"
BYTES=48000   # 0.5 s of 48 kHz stereo s16le

hr() { printf -- '- %.0s' {1..64}; echo; }
have() { command -v "$1" >/dev/null 2>&1; }

hr; echo "server"; hr
pactl info 2>&1 | grep -E 'Server Name|Default Sink|Default Source' || echo "pactl not usable"

hr; echo "sinks"; hr
pactl list short sinks 2>&1 || true

hr; echo "sources (looking for $MON)"; hr
pactl list short sources 2>&1 | sed -n '1,40p' || true
if pactl list short sources 2>/dev/null | awk '{print $2}' | grep -qx "$MON"; then
  echo "OK   monitor source $MON is present"
else
  echo "FAIL no $MON - start a route first:  sonos-bridge route sonos"
fi

try() {  # try() <label> <command...>
  local label="$1"; shift
  if ! have "$1"; then printf 'skip %-26s (%s not installed)\n' "$label" "$1"; return; fi
  local n
  n=$( { timeout 3 "$@" 2>/dev/null || true; } | wc -c )
  if [ "${n:-0}" -gt 1000 ]; then
    printf 'OK   %-26s captured %s bytes in 3 s\n' "$label" "$n"
  else
    printf 'FAIL %-26s captured %s bytes\n' "$label" "${n:-0}"
  fi
}

hr; echo "capture backends against $MON (make some noise, e.g. play a video)"; hr
# generate a little noise of our own so the test is not dependent on timing
( paplay /usr/share/sounds/freedesktop/stereo/bell.oga 2>/dev/null & ) 
sleep 0.2

try "parec"      parec --format=s16le --rate=48000 --channels=2 --device="$MON"
try "pw-record"  pw-record --rate 48000 --channels 2 --format s16le --target "$MON" --file -
try "ffmpeg"     ffmpeg -hide_banner -loglevel error -f pulse -i "$MON" -t 2 -ac 2 -ar 48000 -f s16le -

hr; echo "sink-inputs (app streams) and our sink"; hr
pactl list sink-inputs 2>&1 | grep -E 'Index:|application.name|Sink:' | tr '\n' ' ' ; echo

hr; echo "firewall (Sonos must connect back to us)"
if have ufw; then ufw status numbered 2>/dev/null | head -n 3 || true; fi
have firewall-cmd && firewall-cmd --state 2>/dev/null || true
echo "hint: sudo ufw allow in on <iface> to 192.168.0.0/16 port 32768:61000/tcp"
hr
