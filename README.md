# gliff

[![CI](https://github.com/kevinmcconnell/gliff/actions/workflows/ci.yml/badge.svg)](https://github.com/kevinmcconnell/gliff/actions/workflows/ci.yml)

Remote-desktop a Hyprland session from another Hyprland machine, over SSH only.
Custom wire protocol, Vulkan Video hardware encode and decode, full-resolution
4:4:4 colour by the RDP AVC444 technique (two 4:2:0 H.264 streams recombined on
the client). The whole media path stays on the GPU: the captured dmabuf is
imported into Vulkan, split by a compute shader, encoded, and on the client
decoded, recombined by a compute shader and handed to GTK as a dmabuf.

## Status

Working and validated on AMD (Ryzen Granite Ridge, Mesa RADV):

- Capture of a Hyprland output via `ext-image-copy-capture-v1` into GBM dmabufs.
- Keyboard and pointer injection, with the client's xkb keymap uploaded so keys
  map identically on both ends.
- Vulkan Video H.264 encode and decode (`VK_KHR_video_encode_h264`,
  `VK_KHR_video_decode_h264`); Dual420 4:4:4 round-trips near-lossless, plus a
  `--low-bandwidth` single 4:2:0 stream.
- The full server pipeline (capture -> dmabuf import -> GPU split -> two H.264
  streams -> protocol) and a GTK4 client that decodes, recombines on the GPU,
  displays through a dmabuf texture, forwards input, shows the remote cursor,
  inhibits system shortcuts (release with `Shift+Esc`, set by
  `--release-hotkey`), and auto-reconnects. Validated over localhost against a
  nested Hyprland: connect, stream, resize, ack pacing, both chroma modes.

Design and the full picture are in `docs/architecture.md`; driver-specific
behaviour and test gaps are in `docs/hardware-quirks.md`.

Not done yet: image/binary clipboard (text works both ways); native
single-stream 4:4:4 and AV1/HEVC; a verified ssh-from-cold-machine path; and
testing on Intel and NVIDIA Vulkan drivers.

## Build

Prebuilt x86_64 binaries are on the [releases
page](https://github.com/kevinmcconnell/gliff/releases): every push to `main`
updates the `latest` pre-release, and `v*` tags make permanent releases.

To build from source:

```
cargo build --release
```

Needs Rust and the runtime libraries in the PKGBUILD `depends`: a Vulkan
loader and a driver with Vulkan Video (Mesa RADV 24+ on AMD). No C toolchain
is needed; the compute shaders are committed as SPIR-V
(`crates/gliff-vk/shaders/build.sh` rebuilds them with `glslc`). Verify the
machine first:

```
gliff-probe all          # protocols, outputs, Vulkan, GPU encode/decode round-trip
gliff-probe pipeline     # capture one frame and run the whole GPU 4:4:4 path
```

To run under the Khronos validation layer during development:

```
VK_INSTANCE_LAYERS=VK_LAYER_KHRONOS_validation gliff-probe roundtrip
```

## Run (ssh, the real path)

```
gliff user@host
```

This mirrors the remote's focused screen: it spawns
`ssh -T user@host gliff-server --stdio --output auto`. Other modes:

```
gliff --output DP-1 user@host   # mirror a named remote screen
gliff --headless user@host      # a private remote screen sized and
                                       # scaled to this window (resizes live)
```

A mirrored screen keeps its own size and scale and is letterboxed in the
window; a headless one follows the window. The ssh session
must see the user's `WAYLAND_DISPLAY`/`XDG_RUNTIME_DIR`; if `gliff-server` is not
on PATH over ssh, pass `--server-bin /path/to/gliff-server`.

## Run (development, localhost)

Start a nested Hyprland, then:

```
gliff-server --listen 127.0.0.1:9000 --headless
gliff --connect 127.0.0.1:9000
```

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

- `crates/gliff-proto` wire types, framing, and the CPU reference for colour
  conversion and the AVC444 4:4:4 split/recombine that the shaders must match.
- `crates/gliff-transport` framed IO, ssh spawning.
- `crates/hypr-ipc`, `crates/hypr-wl` Hyprland IPC and shared Wayland plumbing.
- `crates/hypr-capture` output + cursor capture into dmabufs.
- `crates/hypr-input` keyboard and pointer injection.
- `crates/gliff-vk` the Vulkan media pipeline: device, dmabuf import/export,
  split and recombine compute shaders, H.264 encode/decode, header parser.
  The only crate with `unsafe`.
- `bins/gliff-server`, `bins/gliff`, `tools/gliff-probe`.
