# sonos-bridge-gui (GPUI front-end)

A GPU-accelerated desktop panel for the daemon. It is a **thin client**: it only
reads `GET /api/state` and posts commands to the daemon, so the audio path never
depends on the window.

```
sonos-bridge daemon  ── HTTP/JSON 127.0.0.1:8688 ──>  sonos-bridge-gui (gpui)
                                        └──────────>  browser panel / CLI
```

## 1. Verify the std-only half first (no gpui needed)

```sh
cd gui
cargo run --bin apicheck -- selftest     # JSON + daemon-lookup checks, offline
cargo run --bin apicheck                 # prints the live daemon state
cargo run --bin apicheck -- check        # daemon diagnostics
cargo run --bin apicheck -- daemon       # which daemon binary the panel will use
cargo run --bin apicheck -- start        # start it, exactly as the button does
cargo run --bin apicheck -- "route both Living Room"
```

`apicheck` compiles without gpui, so you can prove the daemon contract works
before touching the GPU stack. `src/locate.rs` (where the daemon binary is) and
`src/jsonlite.rs` (the parser) are std-only and both are covered by `selftest`,
which is why the two window bugs below can be pinned down from a terminal.

## 2. Build / run (gpui enabled)

`Cargo.toml` already depends on `gpui = "0.2.2"` (the released crates.io build,
verified compiling on Ubuntu 25.04 / Wayland) plus `libc = "=0.2.177"` (pinned
because xattr 0.2.3, pulled in by gpui's dep chain, breaks against
libc >= 0.2.178).

### Ubuntu system packages

```sh
sudo apt install build-essential pkg-config \
  libwayland-dev libwayland-bin libxkbcommon-dev \
  libgbm-dev libegl1-mesa-dev libgl1-mesa-dev libvulkan-dev \
  libasound2-dev libx11-xcb-dev libxcursor-dev libxtst-dev libxrandr-dev \
  libxcb1-dev libxkbcommon-x11-dev
```

### ~/.cargo owned by root?

On this box `~/.cargo` was created root-owned by an earlier `sudo cargo` run, so
crate downloads fail with "permission denied". Workaround: point cargo at a
user-writable directory (the dev build below does this automatically):

```sh
export CARGO_HOME=/tmp/gpui-probe/cargo
export CARGO_TARGET_DIR=/tmp/gpui-probe/probe/target
export RUSTFLAGS="-C link-arg=-L/tmp/gpui-probe/lib"
```

`/tmp/gpui-probe/lib` contains unversioned `.so` symlinks (libxcb.so →
libxcb.so.1, etc.) extracted from the matching `-dev` debs, so the linker can
find `-lxcb` etc. without root.  At *runtime* only the normal runtime packages
are needed (already installed), so the installed binary runs with no flags.

## 3. Build / run

```sh
cargo build --release          # gui/target/release/sonos-bridge-gui
cargo run --release            # starts a window on the daemon's API
cargo run --release -- http://127.0.0.1:8688   # explicit base URL
```

`./install.sh` (in the project root) builds both binaries, installs them side by
side into `~/.local/bin`, and writes the app-menu entry. The installed GUI then
lives at `~/.local/bin/sonos-bridge-gui`; `gui/sonos-bridge-gui.desktop` is a
copy of the menu entry for manual installs.

## 3b. Two bugs worth knowing about

**"Start daemon" says it cannot find `sonos-bridge`.** The GUI and the daemon are
two different binaries, and the two ways of running them imply different layouts:
a checkout puts the GUI in `gui/target/release` but the daemon one workspace up,
while an installed GUI in `~/.local/bin` has no sibling binary at all. `locate.rs`
searches, in order: `$SONOS_BRIDGE_BIN` (override, never silently fallen back
from), the GUI's own directory, each ancestor's `target/release` (four levels up,
which is what reaches the workspace from `gui/target/release`), `~/.local/bin`,
`~/.local/share/sonos-bridge`, the system bin dirs, then PATH. Run
`apicheck -- daemon` next to the GUI binary you are actually running: it prints
the resolved path, or every place it searched. Fix = install both into one
directory, or export `SONOS_BRIDGE_BIN`.

**No titlebar: the window cannot be moved or closed.** GNOME on Wayland offers no
server-side decorations, and gpui does not draw a titlebar either (Zed draws its
own). `WindowOptions { titlebar, window_decorations }` only *declares* a
configuration - it produces no pixels and no grab area on Linux. So `view.rs`
draws the strip itself. Three facts that cost time to discover:

| Attempt | Reality in gpui 0.2.2 |
|---|---|
| `window_decorations: Client` | tells the compositor not to decorate; draws nothing itself |
| `.window_control_area(WindowControlArea::Drag/Min/Max/Close)` | honoured on macOS/Windows only - Linux `on_hit_test_window_control` is an empty function, so it is inert |
| `window.start_window_move()` | works on Wayland/X11, but the backend uses the serial of the **last mouse press**, so it must be called from an `on_mouse_down` handler |

That is why the strip combines a `WindowControlArea` (for the platforms that use
it) with real handlers: mouse-down on the bar starts the compositor move, and the
two glyphs call `minimize_window()` / `zoom_window()`, with close calling
`cx.quit()`. The daemon survives that because `sonos-bridge start` detaches with
`setsid`. GNOME also accepts Super+middle-drag to move and Super+right-drag to
resize any window, which is the fallback if a build ever loses the strip.

**Three symptoms, one command-string bug.** Clicking *Sonos* produced no sound and
the log filled with `selected speaker not found`, while the status line read
`wanted: """Bedroom", Bedroom""`. The panel wrapped names in double quotes
(`route sonos "Bedroom"`) because that is what a shell wants, but `cmds::run`
split the line with plain `split_whitespace`, so the quote became part of the
speaker name. Fixed on both sides: the shared parser is now quote-aware
(`cmds::split_args`, also covering `route sonos "Bedroom Beam"` for every
non-shell front-end), and the panel sends plain text.

*Stop daemon* did nothing for a simpler reason: `shutdown` was a *client* command
in `main.rs` (it sends SIGTERM to the pid), so the API had no such verb and the
panel's request was an unknown command. `shutdown` / `quit` / `exit` are now
handled by the shared parser: the stop is deferred 400 ms so the reply is
flushed, then the daemon loop runs the same cleanup as SIGTERM (children killed,
sink unregistered, default output restored, pid file removed).

The speaker rows tick from the daemon's wanted list, which arrives on the next
poll, so a click looked dead. `picked()` now lives in `lib.rs` (rules: local pick
wins for instant feedback, `all`/`*`/empty means every row while routed, nothing
while off) and is covered by `apicheck selftest` under `panel:`.

## 4. If it does not compile: API drift

Everything gpui specific lives in `src/main.rs` and `src/view.rs`.
`grep -n API-DRIFT src/*.rs` lists the whole surface:

| Spot | Typical variants |
|---|---|
| `Render::render` signature | `(self, window: &mut Window, cx: &mut Context<Self>)` (new) vs `(self, cx: &mut ViewContext<Self>)` (old) |
| `on_click` handler arity | `|this, &ClickEvent, &mut Window, &mut Context<T>|` (new) vs `|this, _, &mut Context<T>|` (old) |
| `open_window` closure return | `view.clone()` vs `view.clone().into_any()` |
| `WindowOptions` fields | `titlebar` / `window_min_size` / `window_background` appear in different versions |
| refresh task | `cx.spawn_in(None, ...)` vs `cx.spawn(...)`, `Entity::update(&mut cx)` vs `AsyncApp::update_entity` |
| colours / text | `gpui::rgb/rgba`, or `ui::Label`/`ui::Button` from gpui-kit instead of the local `label()`/`button()` helpers |

### Using gpui-kit components instead of the local widgets

`view.rs` builds buttons/labels from primitives so the crate stays usable even
without the kit. To switch to kit components, replace the two helpers at the
bottom of `view.rs` (`label()`, `button()`) with the kit equivalents — the call
sites in `impl Render` stay exactly as they are.

## 5. What the window shows

- **Title bar (drawn by the app):** drag to move, minimize, maximize, close.
- **Header:** daemon online/offline + uptime, and a big "Start daemon" / "Stop
  daemon" button (the window is how you start the daemon; a daemon that isn't
  running can't serve a panel).
- **Output:** Off / PC speakers / Sonos / Both mode buttons, plus a player
  picker (one row per discovered speaker; subs/surrounds are flagged and
  selecting one picks its zone coordinator).
- **Live:** virtual-output registration state, capture backend + pid, encoder
  pid, stream URL + client count, audio peak/rms meters, ring-recycled bytes.
- **Diagnostics:** one-click `sonos-bridge check` rerun, shown as ok/fail rows.
