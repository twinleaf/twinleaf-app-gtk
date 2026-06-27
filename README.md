# twinleaf-app-gtk

A modern, cross-platform (Linux / Windows / macOS) **GTK4** application for
discovering, streaming, plotting, logging, and configuring Twinleaf quantum
sensors — a [gtk-rs](https://gtk-rs.org/) port of the SwiftUI Twinleaf app,
built on the same `twinleaf-rust` core.

## Architecture

A Cargo workspace with two crates:

- **`twinleaf-core`** — UI-agnostic runtime ported from the SwiftUI app's
  `tio-bridge` Rust core, with the C-FFI boundary removed. It owns all device
  communication (via the `twinleaf` crate), FPCS decimation, Welch FFT, `.tio`
  packet logging, CSV/HDF5 export, playback, firmware updates, and stream
  health. The UI drives it by sending `CoreCommand`s and consuming `CoreEvent`s
  over an `async-channel`. No serialization on the hot path: plot frames travel
  in-process as typed `PlotFrame`s.
- **`twinleaf-gtk`** — the user interface, built with gtk4-rs + **libadwaita**
  (Adwaita styling, automatic system light/dark via `AdwStyleManager`, a separate
  `AdwPreferencesWindow`). A worker thread runs the core; the GTK main loop
  drains its events with `glib::spawn_future_local` and renders the plot on a
  theme-aware Cairo `DrawingArea` (colours + font follow the active theme).

`twinleaf-rust` is vendored as a git submodule under `vendor/` (pinned to match
the SwiftUI app for API parity).

```
twinleaf-app-gtk/
  twinleaf-core/      # runtime (device I/O, decimation, FFT, logging, export)
  twinleaf-gtk/       # GTK4 UI
  vendor/twinleaf-rust/   # submodule
```

## Prerequisites

- A recent Rust toolchain (`rustup`), edition 2021.
- **GTK4** development libraries.
  - **macOS:** `brew install gtk4 libadwaita pkg-config`. Homebrew keeps some
    deps "keg-only" (`libffi`, `zlib`, `bzip2`, `expat`, …), so their `.pc`
    files aren't on the default pkg-config path. Generate a local
    `.cargo/config.toml` (gitignored) pointing the build at them + the data dir:
    ```sh
    PCP="/opt/homebrew/lib/pkgconfig:/opt/homebrew/share/pkgconfig"
    PCP="$PCP:/opt/homebrew/Homebrew/Library/Homebrew/os/mac/pkgconfig/$(sw_vers -productVersion | cut -d. -f1)"
    for d in /opt/homebrew/opt/*/lib/pkgconfig; do PCP="$PCP:$d"; done
    mkdir -p .cargo
    { echo '[env]'
      echo "PKG_CONFIG_PATH = { value = \"$PCP\", force = true }"
      echo 'XDG_DATA_DIRS = { value = "/opt/homebrew/share:/usr/local/share:/usr/share", force = true }'
    } > .cargo/config.toml
    ```
    If you hit a `g_settings_schema_source_lookup` CRITICAL at runtime, the
    GSettings schemas weren't compiled — do it once into your XDG home:
    ```sh
    mkdir -p ~/.local/share/glib-2.0/schemas
    cp /opt/homebrew/share/glib-2.0/schemas/*.gschema.xml ~/.local/share/glib-2.0/schemas/
    glib-compile-schemas ~/.local/share/glib-2.0/schemas
    ```
  - **Linux:** install `libgtk-4-dev` + `libadwaita-1-dev` (and `pkg-config`).
  - **Windows:** build GTK4 with [gvsbuild](https://github.com/wingtk/gvsbuild)
    and set `PKG_CONFIG_PATH` accordingly.
- Optional: `cmake` (only for the off-by-default `hdf5` export feature).

## Build & run

```sh
git submodule update --init --recursive    # first checkout
cargo build                                 # whole workspace
cargo run -p twinleaf-gtk                   # launch the app
```

The window opens with a sidebar; click **Refresh** to enumerate devices (serial
+ mDNS), **Connect** to a device (or type a URL like `udp://127.0.0.1:7855` or
`serial:///dev/cu...`), then tick a channel to plot it live.

### Smoke-testing without hardware

`twinleaf-rust` ships a device simulator. Run it (it serves UDP on 7855), then
connect the app to `udp://127.0.0.1:7855` (use `127.0.0.1`, not `localhost` —
the simulator binds IPv4):

```sh
./vendor/twinleaf-rust/target/debug/tio simulate          # 10 Hz sine @ 1 kHz
cargo run -p twinleaf-gtk                                  # connect to udp://127.0.0.1:7855
```

Headless end-to-end checks:

```sh
# Core only: connect, auto-select a channel, print decimated plot frames.
cargo run -p twinleaf-core --bin twinleaf-core-headless -- udp://127.0.0.1:7855

# Full UI, headless: auto-connect + auto-select + auto-quit (env-gated test hooks).
TWINLEAF_SELFTEST_CONNECT=udp://127.0.0.1:7855 TWINLEAF_SELFTEST_QUIT=8 \
  cargo run -p twinleaf-gtk
```

Run the core's unit tests with `cargo test -p twinleaf-core`.

## Status

Built and verified milestone by milestone (see the implementation plan).

The whole workspace builds clean (0 warnings) and all tests pass. The UI uses
libadwaita: it follows the system light/dark scheme (overridable in Preferences),
the plot canvas is theme-aware, and settings live in a separate `AdwPreferencesWindow`.

**Verified** end-to-end (built, tested, and run against the simulator):

- ✅ **M0** — core port (FFI removed, typed `CoreEvent`/`PlotFrame`); 22 tests pass.
- ✅ **M1** — GTK shell, bridge, discovery/connect, channel sidebar, live timeseries.
- ✅ **M2** — cursor readout + crosshair, scroll-to-zoom, pause, nice ticks, **multi-pane** (up to 5, frames routed per pane) + channel **drag-and-drop** onto panes; FFT renders **log-log** (with log-f / log-A toggles).
- ✅ **M3** — RPC sidebar (Switch/Entry+Set/Read/Action/Capture by type, searchable); read round-trip verified against the sim.
- ✅ **M4** — logging to `.tio` (Record toggle + byte/packet counter).
- ✅ **M5** — FFT mode; 10 Hz sine → spectral peak near 10 Hz.
- ✅ **M6** — CSV export + clipboard copy; verified (20k-row CSV from a recording).
- ✅ **M7** — open `.tio` for inspection + playback scrubber; verified (metadata + playback + plot).
- ✅ **M8** — **firmware popover** (available updates, per-device Flash, live flash progress) + a **stream-health popover** (per-stream rate / jitter / drift / dropped). Verified against a real serial device (COMM-USB/VMR).
- ✅ **M9** — TOML preferences via `directories`; window/sidebar geometry persisted on close.

**Partial:**

- 🟡 **M10** — packaging scaffolding (`.gitlab-ci.yml`, Flatpak manifest, `.desktop`, macOS bundle script) written; not yet validated on real Linux/Windows/macOS builders.

Headless smoke tests behind `TWINLEAF_SELFTEST_*` env hooks (see
`twinleaf-gtk/src/window.rs`) and the `twinleaf-core-headless` bin reproduce the
verifications above without a GUI session.

## License

Apache-2.0.
