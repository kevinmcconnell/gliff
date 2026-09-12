# haver architecture and design

haver remote-desktops a Hyprland session to another Hyprland machine over SSH.
This document covers what is built, the design choices behind it, and what is
still pending.

## Data flow

```
        server (remote host)                         client (local)
  ┌─────────────────────────────┐              ┌──────────────────────────────┐
  Hyprland output                │              │  GTK4 window (gtk::Picture)
    │ ext-image-copy-capture     │              │        ▲ MemoryTexture
    ▼                            │              │        │
  GBM dmabuf ─ CPU map ─► BGRA   │              │   BGRA ◄─ yuv444_to_bgra
    │                            │              │        ▲
    ▼ bgra_to_yuv444 (BT.709)    │              │   YUV 4:4:4 ◄ recombine
  YUV 4:4:4                      │   protocol   │        ▲
    │ split (AVC444)             │  over TCP    │   two NV12 ◄ two H.264 decoders
    ▼                            │   or ssh     │        ▲
  main NV12 + aux NV12           │  ═════════►  │        │ payloads
    │ two VA-API H.264 encoders  │              │   VideoFrame{main,aux}
    ▼ VideoFrame{main,aux} ──────┼──────────────┼────────┘
                                 │              │
  hypr-input ◄ Key/Pointer ◄─────┼──────────────┼──◄ GTK event controllers
```

Keyboard, pointer, resize, frame acks and keyframe requests flow client→server;
video, cursor and pongs flow server→client.

## Crates

| Crate | Role | `unsafe` |
|---|---|---|
| `haver-proto` | wire messages, framing header, AVC444 split/recombine, chroma up/subsample | none |
| `haver-transport` | `Framed` length-prefixed IO with out-of-band payloads (`write_vectored`), ssh spawn | none |
| `hypr-ipc` | Hyprland control socket (instance discovery, outputs, options) | none |
| `hypr-wl` | shared Wayland plumbing (connect, globals, output/seat tracking) | none |
| `hypr-capture` | output + cursor capture into GBM dmabufs on a calloop thread | none |
| `hypr-input` | virtual keyboard (xkb state) + virtual pointer on a calloop thread | none |
| `haver-codec` | VA-API H.264 encode/decode, CPU colour, Dual420 and Single420 | none |
| `haver-server` | ties capture+input+codec to the protocol; `--stdio`/`--listen` | none |
| `haver-client` | GTK4/libadwaita UI, decode worker | none |
| `haver-probe` | environment checks and the headless test client | none |
| `vendor/cros-*` | patched cros-libva and cros-codecs | in the libraries only |

There is no `unsafe` in haver's own code. The GBM, VA-API and xkb crates wrap
the C libraries; the vendored codec libraries contain the only `unsafe`, and our
patches to them are documented in their `README.haver.md`.

## Key design choices

- **SSH is the only transport.** The server opens no port in production; the
  client spawns `ssh -T host haver-server --stdio` and the protocol runs over
  that pipe. `--listen`/`--connect` exist for local development only and have no
  auth. TCP framing is length-prefixed postcard messages; large video and cursor
  payloads ride outside the postcard body so the encoder output is sent with
  `write_vectored` and read straight into the decoder.

- **4:4:4 by two 4:2:0 streams (AVC444).** Hardware H.264 encoders only do
  4:2:0, which blurs coloured text. haver splits full 4:4:4 into a main stream
  (luma + even-position chroma) and an auxiliary stream (the dropped chroma), and
  recombines them bit-exactly on the client. Unlike RDP AVC444 v1 we do not
  average or filter the main chroma, because we own both ends, so the split and
  recombine are a lossless inverse pair (unit-tested through 1080p).
  `--low-bandwidth` drops to a single 4:2:0 stream with chroma upsampled on
  decode.

- **CBR, not constant-QP.** The AMD VA-API encoder mis-handles cros-codecs'
  constant-QP path (see `docs/hardware-quirks.md`); haver drives CBR.

- **Latest-wins, ack-paced.** The server keeps only the most recent captured
  frame and encodes it when the client has ack capacity. The number of unacked
  frames allowed is derived from a smoothed ack RTT and clamped to 2..8, so the
  frame rate is not capped by latency and a slow client cannot build a backlog.
  Frames are captured on demand, so a static screen costs nothing.

- **Threading.** cros-codecs' encoder and decoder use `Rc` and are `!Send`, so
  each runs on a single thread: the server loop and the client decode worker are
  current-thread tokio runtimes. Capture and input each own a Wayland connection
  on their own calloop thread and talk to the async side through channels; only
  plain buffers cross threads. On both peers, reads and writes run on separate
  tasks so an input burst cannot starve video and a blocked write cannot block
  reads.

- **Client rendering is CPU, for now.** Decoded NV12 is recombined and colour-
  converted on the CPU into BGRA and shown as a `gdk::MemoryTexture`. This keeps
  the client free of `unsafe` GL. The zero-copy GL path is a documented future
  optimisation (see below).

- **Cursor.** The remote cursor is shown as the video widget's own cursor, so
  the local compositor draws it at the real pointer with no added latency; it is
  never baked into the video.

## Testing

- **Unit tests** cover the pure logic: AVC444 split/recombine losslessness,
  single-stream subsample/upsample, BGRA↔YUV444 colour round-trip, NAL-header
  fixup safety, framing with payloads and partial writes, the ack-window bounds,
  keymap building, and Hyprland instance discovery.
- **`haver-probe`** is the hardware integration harness: `protocols`, `outputs`,
  `vaapi`, `roundtrip` (encode→decode PSNR), `capture`, `input`, `pipeline`
  (capture→4:4:4→decode), and `serve-test` (a headless protocol client).
- **`scripts/e2e.sh`** boots a nested Hyprland and asserts PASS across the probe
  checks, the 4:4:4 pipeline, and both Dual420 and Single420 server-plus-client
  streams. It needs a Hyprland session and a VA-API GPU, so it is not a CI unit
  test; run it on a target machine.

## Pending and recommended improvements

Built and validated on AMD: capture, input, H.264 encode/decode, Dual420 and
Single420, the server loop, the GTK client, keymap upload, remote cursor,
shortcut inhibit, and reconnect.

Not yet built, roughly in priority order:

1. **Clipboard bridge** (`ext-data-control` on the server, `gdk::Clipboard` on
   the client). Useful and on the roadmap; deferred because it is a sizeable
   bidirectional subsystem with limited automated testability.
2. **`GlSplitter` / GL client recombine.** Zero-copy chroma split on the server
   and a recombine shader on the client, replacing the CPU colour passes. This
   is the main throughput optimisation. It requires `unsafe` GL/EGL (a C API),
   so it is deferred per the project's no-unsafe preference and needs a display
   to validate.
3. **Native single-stream 4:4:4, and AV1/HEVC.** Probe-gated; this GPU exposes
   no such VA-API encode entrypoint, so they cannot be validated here. Needs an
   Intel or newer GPU.
4. **Verified ssh path** from a cold machine, including the `WAYLAND_DISPLAY` /
   `XDG_RUNTIME_DIR` environment setup, and a systemd user unit if wanted.
5. **Polish**: multi-output selection UI, a configurable escape key for shortcut
   inhibit, and `tc netem` tuning of the adaptive ack window.

See `docs/hardware-quirks.md` for driver-specific behaviour and the low-severity
items surfaced by code review.
