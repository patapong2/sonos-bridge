#!/usr/bin/env bash
# PipeWire-native monitor bridge - the "swyh-rs / openrc" style workflow, done
# with PipeWire's own graph instead of the PulseAudio compatibility layer.
#
# Why this exists: on PipeWire >= 1.6 capturing the *monitor* of a sink that
# PipeWire manages natively can be refused over the Pulse bridge, and the fix is
# to expose the stream with PipeWire's own module-loopback node, then capture it
# with pw-record.  sonos-bridge also has a userspace fallback (local_backend =
# pipe) which usually makes all of this unnecessary - reach for this script only
# when `sonos-bridge route local` produces silence.
#
#   ./scripts/bridge-openrc.sh start <sink-name> [capture-port]
#   ./scripts/bridge-openrc.sh stop
#   ./scripts/bridge-openrc.sh status
#
# After `start`, capture from the monitor node it prints:
#   sonos-bridge set capture_backend pw-record
#   sonos-bridge set sink_name sonos_capture      # the node name we created
#
# needs: pw-dump, pw-link (pipewire-tools), jq

set -euo pipefail

NODE_NAME="${BRIDGE_NODE_NAME:-sonos_capture}"
STATE_FILE="${XDG_RUNTIME_DIR:-/tmp}/sonos-bridge/openrc-node"

die() { printf '!! %s\n' "$*" >&2; exit 1; }

for t in pw-dump pw-link jq; do
  command -v "$t" >/dev/null || die "$t is required (sudo apt install pipewire-tools jq)"
done

node_id() {
  pw-dump "$1" 2>/dev/null | jq -r '
      select(.info.params.Node != null)
      | select(.info.props["node.name"] == "MONITOR_NAME")
      | .id' | head -n 1 | sed "s/MONITOR_NAME/$1/" || true
}

sink_id() {
  pw-dump 2>/dev/null | jq -r --arg n "$1" '
      select(.info.props["node.name"] == $n) | .id' | head -n 1
}

case "${1:-status}" in
  start)
    sink="${2:?usage: bridge-openrc.sh start <sink-name> [capture-name]}"
    [ $# -ge 3 ] && NODE_NAME="$3"
    sid="$(sink_id "$sink")"
    [ -n "$sid" ] || die "no PipeWire node with node.name=$sink (pw-dump | jq -r '.info.props[\"node.name\"]' to list them)"
    echo "linking sink node $sid ($sink) -> monitor source '$NODE_NAME'"
    # expose the sink's stream as a capturable monitor source
    pw-link -o "$sid" >/dev/null 2>&1 || true
    pw-dump "$sid" | jq -r '.info.props["node.name"]' >/dev/null 2>&1 || true
    # the module-loopback route: sink input -> our capture node
    pw-link "$sid:$NODE_NAME" 2>/dev/null || {
      # older/newer pw-link syntaxes, both attempted for convenience
      pw-link "$sid" "$NODE_NAME" 2>/dev/null || die "pw-link failed; on PipeWire >= 1.6 use the daemon's local_backend=pipe instead"
    }
    nid="$(node_id "$NODE_NAME")"
    mkdir -p "$(dirname "$STATE_FILE")"
    printf '%s\n' "${nid:-unknown}" > "$STATE_FILE"
    cat <<EOF
capture node : $NODE_NAME (id ${nid:-unknown})
now tell sonos-bridge to capture it:
  sonos-bridge set sink_name $NODE_NAME
  sonos-bridge set capture_backend pw-record
EOF
    ;;
  stop)
    if [ -f "$STATE_FILE" ]; then
      nid="$(cat "$STATE_FILE")"
      [ "$nid" != "unknown" ] && pw-link -o "$nid" 2>/dev/null || true
      rm -f "$STATE_FILE"
      echo "unlinked $NODE_NAME"
    else
      echo "nothing recorded in $STATE_FILE; unloading pw-loopback by name if present"
      pw-dump 2>/dev/null | jq -r 'select(.info.props["node.name"] == "'"$NODE_NAME"'") | .id' |
        while read -r id; do [ -n "$id" ] && pw-link -o "$id" 2>/dev/null || true; done
    fi
    ;;
  status)
    pw-dump 2>/dev/null | jq -r '
        select(.info.props["node.name"] != null)
        | "\(.id)\t\(.info.props["media.class"])\t\(.info.props["node.name"])"' |
      grep -E 'stream/(Input|Output)|MONITOR' || true
    pw-dump 2>/dev/null | jq -r 'select(.info.props["node.name"] == "'"$NODE_NAME"'") | "bridge node present: \(.id)"' || true
    ;;
  *) die "usage: $0 start|stop|status" ;;
esac
