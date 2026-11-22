# sonos-bridge

Register your Ubuntu machine as a **sound output device**, and then decide where
that audio actually goes: to your **Sonos speakers**, back out to the **normal
speakers**, or to both at the same time.

```
 apps (browser, spotify, games…)
        │  they just see an ordinary output called "Sonos Bridge"
        ▼
 module-null-sink  (the device you select in Sound Settings)
        │  monitor
        ▼
   ┌──────────────────────── sonos-bridge daemon (single binary) ─────────────┐
   │ capture (parec / pw-record / ffmpeg)                                     │
   │    ├─► local path : module-loopback → alsa_output…  (or ffmpeg fallback) │
   │    └─► encoder    : ffmpeg → MP3/FLAC → in-memory stream log             │
   │                        └─► HTTP chunked stream  http://<lan-ip>:<rand>/…  │
   │                              └─► Sonos AVTransport SetAVTransportURI+Play│
   │ control: CLI  +  web panel  +  GPUI desktop app  +  unix socket          │
   └──────────────────────────────────────────────────────────────────────────┘
```

Difference to [swyh-rs](https://github.com/dheijl/swyh-rs): swyh-rs is a player
you have to point at an existing stream. `sonos-bridge` *is* the output device -
it registers the sink, owns the routing switch, and can hand everything back to
the real speakers.

## Why this design

* **One binary, no daemon-manager dance.** Everything is process-local state; a
  single `route` command flips the whole graph.
* **No Rust crates in the core.** std only. `cargo build` needs no network. The
  heavy lifting (PCM capture, MP3 encoding, sink registration) is delegated to
  the tools Ubuntu already ships: `pactl`, `parec`/`pw-record`, `ffmpeg`.
  `pactl` is the one API that behaves the same on PulseAudio **and** PipeWire,
  which is what makes the virtual sink portable.
* **Push, not pull.** The daemon tells Sonos (`SetAVTransportURI` + `Play`) to
  pull our chunked stream, so there is no need to hand-craft a TuneIn URI
  (which modern players restrict anyway). A manual radio-URL fallback is still
  provided (`sonos-bridge url`).
* **Bounded, drop-oldest buffers.** Several speakers, one encoder: each client
  keeps its own read position in a circular log and resyncs on a frame boundary
  instead of stalling the pipeline.

## Requirements

```sh
sudo apt install pipewire pulseaudio-utils ffmpeg
# Rust: rustup is preferred (Ubuntu 22.04's apt rustc is too old)
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh   # rustc >= 1.70
```

## Build

```sh
cd sonos-bridge-rs
cargo build --release
target/release/sonos-bridge selftest      # offline logic checks (no audio needed)
```

## Use it

```sh
# 1. start the daemon (foreground)          … or: sonos-bridge start  (detached)
target/release/sonos-bridge daemon

# 2. find the speakers
target/release/sonos-bridge discover
#   > Living Room [192.168.1.21]
#     Kitchen     [192.168.1.22]

# 3. route everything to Sonos
target/release/sonos-bridge play "Living Room"

# other modes
target/release/sonos-bridge route both "Living Room,Kitchen"   # speakers + Sonos
target/release/sonos-bridge route local                        # straight through
target/release/sonos-bridge stop                               # back to normal

# watch it
target/release/sonos-bridge status
sonos-bridge open          # prints the web panel URL (http://127.0.0.1:8688)
```

`play` / `route sonos|local|both` registers the sink **and** makes it the system
default, so every app is captured without touching each app. Prefer to choose
per app? `sonos-bridge inputs` then `sonos-bridge move <index> sonos_bridge`.

Prefer picking the device by hand, like any other output? Set
`default_capture_only = false` in the config: the sink stays registered while a
route is active but the default output is left alone - select **"Sonos Bridge"**
in GNOME *Settings → Sound → Output*.

### Run at login (systemd user unit)

```sh
./install.sh            # builds, installs ~/.local/bin + ~/.config/systemd/user
systemctl --user enable --now sonos-bridge
```

## Configuration

`~/.config/sonos-bridge/config` (`key = value`), or flags on `daemon`/`start`:

| key | default | meaning |
|---|---|---|
| `sink_name` | `sonos_bridge` | internal name of the virtual sink |
| `sink_description` | `Sonos Bridge` | the name you pick in Sound Settings |
| `capture_backend` | `auto` | `auto` (parec → ffmpeg → pw-record), `parec`, `ffmpeg`, `pw-record`, `alsa` |
| `alsa_capture` | – | e.g. `hw:Loopback,1,0` for the `alsa` backend |
| `rate` / `channels` | `48000` / `2` | capture format |
| `stream_rate` | `44100` | resampled before encoding (Sonos friendly) |
| `stream_format` / `stream_bitrate` | `mp3` / `192k` | `mp3` is the safe choice; `flac` works on most players |
| `http_port` | `0` (random) | stream port; the actual one is written to `$XDG_RUNTIME_DIR/sonos-bridge/stream_port` |
| `http_bind` | – | force the LAN IP advertised to the speakers (multi-homed hosts, VPN) |
| `local_backend` | `auto` | `auto` (loopback module → userspace fallback), `loopback`, `pipe`, `none` |
| `default_capture_only` | `true` | make the virtual sink the system default while routing |
| `player` | – | speaker to use (`route sonos` without arguments uses this) |
| `ui_host` / `ui_port` | `127.0.0.1` / `8688` | web panel + JSON API |

Runtime files (per user, mode 700): control socket `ctl.sock`, `stream.token`,
`stream_port`, `bridge.log`, `daemon.pid` in `$XDG_RUNTIME_DIR/sonos-bridge/`.

## Desktop app (GPUI)

`gui/` is a separate crate (own workspace) so the audio core keeps building with
zero dependencies. It renders with gpui / [gpui-kit](https://gpui-kit.com) and
drives the daemon over the same JSON API.

```sh
cd gui
cargo run --bin apicheck -- selftest   # std-only checks, no GPU needed
# add gpui (+gpui-kit) to gui/Cargo.toml, see gui/NOTES.md
cargo run --release
```

The browser panel (`sonos-bridge open`) offers the same controls and needs no
extra dependency - use it if gpui will not build on your machine/VM.

## Access control

The stream URL carries a per-user secret token (`stream.token`) and the server
only answers **private/LAN** source addresses (`10/8`, `172.16/12`,
`192.168/16`, `127/8`, `169.254/16`), so a random LAN host cannot just pull
your audio. The web panel binds to `127.0.0.1` by default; if you expose it,
`?t=<token>` is required.

## Latency and sync

Expect roughly **1.5–3 s** of delay to a Sonos player (its own buffering), and
that this is *not* designed for lip-sync-critical use; the local path adds
~20–50 ms. Silence is encoded continuously so the "radio" stream never starves
and Sonos does not drop the connection. Sonos re-clocks the stream on its side,
so small clock drift is absorbed; after many hours a hiccup can appear, and
`sonos-bridge play` re-syncs instantly.

A radio stream has to be *constant*, so the pipeline is judged by one number:
does the encoder produce one second of audio per second of wall time? `status`
reports it directly as `produced N s` on the `stream` line, next to the capture
counters. Anything below ~100% shows up as stutter in the room, because the
player is spending buffer that the bridge never refilled - and when the buffer
runs out the player stops asking for the stream altogether. Two buffer sizes are
what hold that ratio at 100%:

* the capture→encoder queue must absorb the sound server's delivery bursts
  (it delivers in chunks, not smoothly) - a queue sized only as a latency
  budget guarantees lost audio, because a consumer this fast keeps it empty
  anyway;
* the broadcast ring must cover a pause in a player's reading - radio players
  fill their own buffer and then stop reading for a while, and a ring shorter
  than that pause cuts the reader off mid-stream (see `LAG_SECONDS`).

If a player stops pulling the stream while still claiming to play, the daemon
notices (`nothing has pulled the stream for 12s while the player says PLAYING`)
and hands it the URI again, so a hiccup recovers without a restart.

## Troubleshooting

| symptom | what to do |
|---|---|
| `no Sonos speakers found` | SSDP must reach udp/1900: same subnet/VLAN, and `sonos-bridge check` shows your LAN IP. Corporate Wi-Fi with client isolation will not work. |
| speaker discovered but stays silent | `sonos-bridge url`, then in the Sonos app add that URL as a radio station (manual path). Also check your host firewall: `sudo ufw allow in on wlan0 to 192.168.1.0/24 port 32768:61000/tcp` |
| `route local` is silent | the sound-server loopback module may be refused (seen on some PipeWire builds). Force the userspace path: `sonos-bridge set local_backend pipe`. `./scripts/pipewire-diag.sh` shows which capture backend can read the monitor |
| stuttering that starts after tens of seconds and then goes silent | first check `sonos-bridge status`: the `capture` line shows `dropped=` (audio thrown away before the encoder) and the `stream` line shows `produced N s`. If `produced` grows more slowly than wall time, the bridge is starving the player and its buffer drains - look at `dropped=`. If it tracks wall time (~100%), the stream is healthy and the room problem is the network. The stream ring's own eviction is *normal* once full and is reported separately as `ring recycled=` |
| `SetAVTransportURI`/`Play` fails with `UPnPError (402/717/749)` | firmware 85.0+ (July 2025) added a **Connection Security** switchboard. In the Sonos app: Account → Privacy & Security → Connection Security → **UPnP ON**, **Authentication OFF**, **Guest access ON**. With UPnP off, control calls are refused and the codes look random |
| desktop is silent after `stop` | the bridge hands the default output back, but only if a hardware output exists. `pactl list short sinks` showing just `auto_null` means your sound card/monitor went away (HDMI sink disappears when the display sleeps) - unrelated to the bridge |
| sound too quiet/loud | Sonos volume is separate from the sink volume: `sonos-bridge volume 40` (players when playing) vs `sinkvolume 80` |
| want the old behaviour back | `sonos-bridge stop` unregisters the sink and restores the previous default output |

Also useful: `sonos-bridge check` (all of the above in one pass), and
`SONOS_BRIDGE_VERBOSE=3 sonos-bridge daemon` for verbose logs.

## Tests

```sh
cargo build --release
./scripts/selftest.sh         # selftest + `check` output + wiring smoke test
./scripts/pipewire-diag.sh    # sinks/monitors + which capture backend works
```
`selftest` covers the parts that can be checked without audio hardware: XML
parsing, zone-group parsing, SOAP envelope building, HTTP request parsing,
MPEG frame alignment, buffer drop-oldest/resync behaviour, access policy and
the command dispatcher.

## Layout

```
src/main.rs      CLI + subcommand dispatch (daemon / start / stop / every verb)
src/daemon.rs    engine threads, signals, pid file, teardown order
src/router.rs    mode switching: sink registration, default sink, volume
src/capture.rs   monitor -> PCM (parec / pw-record / ffmpeg / alsa)
src/encoder.rs   PCM -> MP3|FLAC (ffmpeg), silence keep-alive
src/stream.rs    chunked HTTP stream for the speakers, token + LAN guard
src/sonos.rs     SSDP discovery, groups, AVTransport/RenderingControl, engine
src/local.rs     passthrough: loopback module, userspace fallback
src/pulse.rs     pactl wrappers (sinks, sources, modules, sink-inputs)
src/cmds.rs      the single command handler used by CLI/UI/GUI
src/web.rs       panel + JSON API      src/ctl.rs  unix socket for the CLI
src/queues.rs    drop-oldest queue + broadcast log + level meter
src/{net,xml,json,util,config,sys,api}.rs   plumbing
web/             panel sources (embedded at build time with include_str!)
gui/             GPUI desktop front-end (separate crate)
```
