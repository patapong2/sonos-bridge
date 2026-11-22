#!/usr/bin/env bash
# Build, run the offline checks, and (optionally) smoke-test the live graph.
#
#   ./scripts/selftest.sh          build + selftest + diagnostics (changes nothing)
#   ./scripts/selftest.sh --live   also flips routes for a few seconds
#
# --live needs a desktop session with PulseAudio/PipeWire running.

set -uo pipefail
cd "$(dirname "$0")/.."

BIN=./target/release/sonos-bridge
rc=0

echo "== build"
cargo build --release || { echo "build failed"; exit 1; }

echo
echo "== offline selftest"
$BIN selftest || rc=1

echo
echo "== version / help sanity"
$BIN version || rc=1
$BIN help >/dev/null || rc=1
$BIN defaults >/dev/null || rc=1

echo
echo "== live diagnostics (needs a running daemon)"
$BIN start >/dev/null 2>&1 || true
sleep 1
$BIN check || true
$BIN status || rc=1

if [ "${1:-}" = "--live" ]; then
  echo
  echo "== live routing (5 s each)"
  echo "-- route local"
  $BIN route local || rc=1
  sleep 3
  $BIN status
  echo "-- route sonos"
  $BIN discover || true
  $BIN route sonos || rc=1
  sleep 3
  $BIN status
  echo "-- route both"
  $BIN route both || rc=1
  sleep 2
  $BIN status
  echo "-- stop"
  $BIN stop || rc=1
  sleep 1
  $BIN status
else
  echo
  echo "(pass --live to also flip routes for a few seconds)"
fi

echo
if [ "$rc" = 0 ]; then echo "selftest: OK"; else echo "selftest: problems above"; fi
exit $rc
