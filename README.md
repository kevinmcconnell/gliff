# haver

Remote-desktop a Hyprland session from another Hyprland machine, over SSH only.
Custom wire protocol, VA-API hardware encode and decode, full-resolution 4:4:4
colour by the RDP AVC444 technique (two 4:2:0 H.264 streams recombined on the
client).

## Status

Working and validated on AMD (Ryzen Granite Ridge, Mesa radeonsi):

- Capture of a Hyprland output via `ext-image-copy-capture-v1` into GBM dmabufs.
- Keyboard and pointer injection (virtual-keyboard, wlr-virtual-pointer).
- VA-API H.264 encode and decode; Dual420 4:4:4 round-trips near-lossless.
- The full server pipeline (capture -> 4:4:4 -> two H.264 streams -> protocol)
  and a GTK4 client that decodes, recombines, and displays, validated over
  localhost against a nested Hyprland: connect, stream, resize, frame-ack pacing.

Not done yet: clipboard bridge, keymap upload from the client, cursor drawing
in the client, ssh env documentation tested end to end, the zero-copy
`GlSplitter`, native single-stream 4:4:4, AV1/HEVC, and reconnect polish. See
`docs/hardware-quirks.md` for driver-specific behaviour and what still needs
testing on Intel.

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

## Layout

- `crates/haver-proto` wire types, framing, AVC444 4:4:4 split/recombine.
- `crates/haver-transport` framed IO, ssh spawning.
- `crates/hypr-ipc`, `crates/hypr-wl` Hyprland IPC and shared Wayland plumbing.
- `crates/hypr-capture` output + cursor capture into dmabufs.
- `crates/hypr-input` keyboard and pointer injection.
- `crates/haver-codec` VA-API encode/decode, CPU colour, Dual420.
- `bins/haver-server`, `bins/haver-client`, `tools/haver-probe`.
- `vendor/` patched cros-libva and cros-codecs (see their `README.haver.md`).
