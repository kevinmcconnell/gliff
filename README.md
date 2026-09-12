# haver

Remote-desktop a Hyprland session from another Hyprland machine, over SSH only.
Custom wire protocol, VA-API hardware encode and decode, full-resolution 4:4:4
colour by the RDP AVC444 technique (two 4:2:0 H.264 streams recombined on the
client).

## Status

Working and validated on AMD (Ryzen Granite Ridge, Mesa radeonsi):

- Capture of a Hyprland output via `ext-image-copy-capture-v1` into GBM dmabufs.
- Keyboard and pointer injection, with the client's xkb keymap uploaded so keys
  map identically on both ends.
- VA-API H.264 encode and decode; Dual420 4:4:4 round-trips near-lossless, plus
  a `--low-bandwidth` single 4:2:0 stream.
- The full server pipeline (capture -> 4:4:4 -> two H.264 streams -> protocol)
  and a GTK4 client that decodes, recombines, displays, forwards input, shows
  the remote cursor, inhibits system shortcuts (release with `Shift+Esc`, set by
  `--release-hotkey`), and auto-reconnects. Validated
  over localhost against a nested Hyprland: connect, stream, resize, ack pacing,
  both chroma modes.

Design and the full picture are in `docs/architecture.md`; driver-specific
behaviour and Intel test gaps are in `docs/hardware-quirks.md`.

Not done yet: image/binary clipboard (text works both ways); the zero-copy `GlSplitter` and GL client
recombine (needs `unsafe` GL); native single-stream 4:4:4 and AV1/HEVC (no
encode entrypoint on this GPU); and a verified ssh-from-cold-machine path.

## Build

```
cargo build --release
```

Needs Rust, clang (bindgen for cros-libva), and the runtime libraries in the
PKGBUILD `depends`. Verify the machine first:

```
haver-probe all          # protocols, outputs, VA-API, encode/decode round-trip
haver-probe pipeline     # capture one frame and run the whole 4:4:4 codec
```

## Run (development, localhost)

Start a nested Hyprland, then:

```
haver-server --listen 127.0.0.1:9000 --headless
haver-client --connect 127.0.0.1:9000
```

## Run (ssh, the real path)

```
haver-client user@host
```

This spawns `ssh -T user@host haver-server --stdio --headless`. The ssh session
must see the user's `WAYLAND_DISPLAY`/`XDG_RUNTIME_DIR`; if `haver-server` is not
on PATH over ssh, pass `--server-bin /path/to/haver-server`.

## Testing

```
cargo test --workspace     # pure-logic unit tests, no GPU needed
cargo fmt --all --check    # formatting (rustfmt defaults)
./scripts/e2e.sh           # full stack against a nested Hyprland (needs a GPU)
```

`scripts/e2e.sh` must run inside a Hyprland session; it boots a nested Hyprland
and asserts the probe checks, the 4:4:4 capture pipeline, and both Dual420 and
Single420 server-plus-client streams.

## Layout

- `crates/haver-proto` wire types, framing, AVC444 4:4:4 split/recombine.
- `crates/haver-transport` framed IO, ssh spawning.
- `crates/hypr-ipc`, `crates/hypr-wl` Hyprland IPC and shared Wayland plumbing.
- `crates/hypr-capture` output + cursor capture into dmabufs.
- `crates/hypr-input` keyboard and pointer injection.
- `crates/haver-codec` VA-API encode/decode, CPU colour, Dual420.
- `bins/haver-server`, `bins/haver-client`, `tools/haver-probe`.
- `vendor/` patched cros-libva and cros-codecs (see their `README.haver.md`).
