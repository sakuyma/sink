# Sink

A small, native PipeWire audio control panel for Wayland.

Sink is a plain GTK4 application. It follows the system GTK theme and talks to
PipeWire directly, with no `pactl`/`wpctl` subprocesses, no daemon and no
polling. It is intended to be a fast, focused replacement for the handful of
mixer controls most people use every day.

## Features

- One window with permanent Outputs, Applications and Inputs tabs. Each tab
  keeps its list, filter and scroll state when you switch away.
- Jump between tabs with `Ctrl+1`, `Ctrl+2` and `Ctrl+3`.
- Search active playback streams with `Ctrl+F`.
- Switch the default output and input.
- Per-device volume and mute.
- Per-application volume and mute, including output streams.
- Route an application to a specific output, or leave it following the default.
- Hardware device volume is written through the active PipeWire device route, so
  WirePlumber, `wpctl` and other mixers see the same value.
- Event-driven updates: external volume, mute and route changes appear without a
  refresh loop.
- Volume is cubic like WirePlumber and capped at 100%.
- Stable alphabetical ordering, with the default device marked by a check mark.
- Bundled symbolic fallback icons, so a missing or broken icon theme does not
  turn the window into a grid of missing-image placeholders.

Press `F1` for About and `Ctrl+Q` to quit.

## Requirements

- Wayland desktop
- PipeWire
- WirePlumber (for default-device and stream-routing metadata)
- GTK 4.10 or newer
- Rust 1.80 or newer
- `glib-compile-resources` (usually provided by `glib2`)
- `clang`/`libclang` for the PipeWire bindings

## Build and run

```sh
cargo build --release --locked
./target/release/sink
```

The application is a single-instance GTK application (`dev.saku.Sink`), so a
second launch focuses the existing window.

## Install manually

The binary embeds its symbolic icons. Install the desktop entry and the
application icon alongside it:

```sh
install -Dm755 target/release/sink ~/.local/bin/sink
install -Dm644 dev.saku.Sink.desktop \
  ~/.local/share/applications/dev.saku.Sink.desktop
install -Dm644 icons/scalable/apps/sink.svg \
  ~/.local/share/icons/hicolor/scalable/apps/sink.svg
```

If `~/.local/bin` is not in your `PATH`, use `/usr/local/bin` instead.

## Arch Linux

The included `PKGBUILD` builds from the local git checkout. Commit your
changes before running:

```sh
makepkg -si
```

`makepkg` clones `HEAD` from `$startdir`; uncommitted files are not included. To
build from a tagged GitHub archive instead, replace the source line in
`PKGBUILD` with the archive URL and checksum.

## Nix

```sh
nix build
./result/bin/sink
```

or run it without installing:

```sh
nix run
```

The lock file pins nixpkgs. Refresh it with `nix flake update` after changing
inputs or when you want a newer nixpkgs.

## How it works

The PipeWire client runs on a dedicated thread and sends immutable snapshots to
the GTK main thread over an async channel. UI actions go back over a second
channel. A registry listener tracks nodes, links, devices, metadata and active
device routes.

- Default sinks and sources are read from and written to WirePlumber's
  `default.audio.sink` / `default.audio.source` metadata.
- Application routing is written to `target.node` metadata. `-1` means “follow
  the default output”; a concrete node id pins the stream to one sink. Links are
  used to discover where an already-running stream is actually playing.
- Hardware sink and source volume goes through the matching `Route` parameter on
  the owning `Device`, with node `Props` kept in sync. Streams and virtual nodes
  use node `Props` directly.

There is no configuration file and no background service. Quitting the window
ends the PipeWire client thread.

## Scope

Sink is deliberately a V1 mixer:

- outputs, inputs and application output streams
- defaults, volume, mute and routing
- no profiles, port selection, Bluetooth codec selector, echo cancellation or
  filter-chain editing
- no PulseAudio or X11 fallback

## License

MIT. See [LICENSE](LICENSE).
