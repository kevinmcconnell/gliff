# gliff architecture and design

gliff remote-desktops a Hyprland session to another Hyprland machine over SSH.
This document covers what is built, the design choices behind it, and what is
still pending.

## Data flow

```
        server (remote host)                         client (local)
  ┌─────────────────────────────┐              ┌──────────────────────────────┐
  Hyprland output                │              │  GTK4 window (gtk::Picture)
    │ ext-image-copy-capture     │              │        ▲ GdkDmabufTexture
    ▼                            │              │        │
  GBM dmabuf ─ Vulkan import ─►  │              │   BGRX dmabuf (Vulkan export)
    │ VkImage (sampled)          │              │        ▲
    ▼ split.comp (BT.709 + AVC444)│             │   recombine.comp
  main NV12 + aux NV12           │   protocol   │        ▲
    │ (encoder input images)     │  over TCP    │   two NV12 ◄ two Vulkan decoders
    ▼ two VK_KHR_video_encode_h264│   or ssh     │        ▲
  VideoFrame{main,aux} ──────────┼──────────────┼────────┘ payloads
                                 │              │
  hypr-input ◄ Key/Pointer ◄─────┼──────────────┼──◄ GTK event controllers
```

Nothing touches pixels on the CPU. On the server the captured dmabuf is
imported once per ring buffer, the split shader writes the encoders' input
images, and the encoders read them in place; only the coded bytes come back to
the CPU. On the client the two decoders write NV12 images, the recombine shader
writes a linear BGRX image, and GTK imports that image as a dmabuf texture.

Keyboard, pointer, resize, frame acks and keyframe requests flow client→server;
video, cursor and pongs flow server→client.

## Crates

| Crate | Role | `unsafe` |
|---|---|---|
| `gliff-proto` | wire messages, framing header, CPU reference for colour and the AVC444 split/recombine | none |
| `gliff-transport` | `Framed` length-prefixed IO with out-of-band payloads (`write_vectored`), the clipboard transfer engine and file spool, ssh spawn | none |
| `hypr-ipc` | Hyprland control socket (instance discovery, outputs, options) | none |
| `hypr-wl` | shared Wayland plumbing (connect, globals, output/seat tracking, calloop runner) | none |
| `hypr-capture` | output + cursor capture into GBM dmabufs on a calloop thread | none |
| `hypr-input` | virtual keyboard (xkb state) + virtual pointer + clipboard bridge (mime types and pipes) on calloop threads | none |
| `gliff-vk` | Vulkan device, dmabuf import/export, split/recombine compute, H.264 encode/decode, header parser | **yes, isolated here** |
| `gliff-server` | ties capture+input+encoder to the protocol; `--stdio`/`--listen` | none |
| `gliff` | GTK4/libadwaita UI, decode worker | one block: hands GTK a dmabuf fd |
| `gliff-probe` | environment checks and the headless test client | none |

`gliff-vk` wraps `ash`, whose every call is `unsafe` because Vulkan is a C API
with no lifetime or synchronisation checks. The crate exposes plain Rust types
(`Gpu`, `Encoder`, `Decoder`, `DisplayFrame`); every `unsafe` block carries a
`SAFETY` comment, and the Khronos validation layer runs clean on the probe
round-trip.

## Key design choices

- **SSH is the only transport.** The server opens no port in production; the
  client spawns `ssh -T host gliff-server --stdio` and the protocol runs over
  that pipe. `--listen`/`--connect` exist for local development only and have no
  auth. TCP framing is length-prefixed postcard messages; large video and cursor
  payloads ride outside the postcard body so the encoder output is sent with
  `write_vectored` and read straight into the decoder.

- **Why not UDP.** Measured on 2026-09-14 between two Wi-Fi machines over
  Tailscale's direct path (MTU 1280, 330-380 Mbit/s of SSH throughput, RTT
  3-12 ms with contention spikes): a 1856x1238 Dual420 mirror ran at 58 fps
  with a worst frame-arrival gap of 71 ms while TCP retransmitted 0.7-6.5% of
  segments. A frame is ~115 packets, so raw UDP would damage most frames at
  those loss rates, and a working UDP path needs NACK or FEC, a jitter buffer
  and codec resilience, whose recovery also costs one RTT. TCP in SSH stays
  until a high-RTT lossy path becomes a primary use; then QUIC (one stream
  per frame, keys handed over SSH) is the candidate, not raw UDP. To measure
  again: client `RUST_LOG=info,gliff_vk=debug`, server
  `--server-bin 'env RUST_LOG=info,gliff_server=debug gliff-server'`, compare
  the `sent frame` and `decode + recombine` timestamps, and sample
  `ss -tin` on the server for retransmits.

- **4:4:4 by two 4:2:0 streams (AVC444).** Hardware H.264 encoders only do
  4:2:0, which blurs coloured text. gliff splits full 4:4:4 into a main stream
  (luma + even-position chroma) and an auxiliary stream (the dropped chroma), and
  recombines them bit-exactly on the client. Unlike RDP AVC444 v1 we do not
  average or filter the main chroma, because we own both ends, so the split and
  recombine are a lossless inverse pair. The CPU implementation in
  `gliff-proto::chroma` is unit-tested through 1080p and is the oracle the
  compute shaders are checked against (`gliff-probe pipeline`).
  `--low-bandwidth` drops to a single 4:2:0 stream with chroma upsampled on
  decode.

- **One GPU API.** Import, colour conversion, split, encode, decode, recombine
  and export all happen in Vulkan on one device, ordered by a timeline
  semaphore. Buffers never cross between GPU APIs, and driver differences are
  read from the Vulkan capability queries rather than special-cased; the same
  code runs on any driver with Vulkan Video.

- **Low-delay H.264.** The encoder emits IDR then P frames with one reference
  and no reordering (POC type 0), High profile, CABAC, CBR at the configured
  bitrate, with the SPS and PPS prepended to every IDR so any keyframe is a
  random-access point. The decoder parses only what the hardware does not
  (SPS, PPS, slice header up to the reference marking) and manages a two-slot
  DPB with sliding-window marking.

- **Latest-wins, ack-paced.** The server keeps only the most recent captured
  frame and encodes it when the client has ack capacity. The number of unacked
  frames allowed is derived from a smoothed ack RTT and clamped to 2..8, so the
  frame rate is not capped by latency and a slow client cannot build a backlog.
  Frames are captured on demand, so a static screen costs nothing.

- **Threading.** Each pipeline lives on one thread: the server loop and the
  client decode worker are current-thread tokio runtimes that own their
  `gliff-vk` objects. Capture and input each own a Wayland connection on their
  own calloop thread and talk to the async side through channels; the capture
  thread hands the whole `CapturedFrame` (its dmabuf) to the encoder, which
  releases the ring slot when the encode has finished. On both peers, reads and
  writes run on separate tasks so an input burst cannot starve video and a
  blocked write cannot block reads.

- **Client display.** The decode worker exports each finished BGRX image as a
  linear dmabuf and the UI wraps it in a `GdkDmabufTexture` on a
  `gtk::Picture`. GTK imports the dmabuf itself (through its GL/Vulkan renderer
  or, failing that, a CPU map), so the client needs no GL code and no fallback
  path of its own.

- **Cursor.** The remote cursor is shown as the video widget's own cursor, so
  the local compositor draws it at the real pointer with no added latency; it is
  never baked into the video. An image with no visible shape falls back to the
  default pointer (see `hardware-quirks.md` for why Hyprland sends those).

- **Clipboard.** Lazy, typed and chunked; `gliff_proto::clipboard` has the
  rules and `gliff_transport::clipboard` the engine both peers run. A new
  selection is announced as an `Offer` of mime types (plus a file list when it
  holds a `text/uri-list`); the peer advertises the same on its own clipboard
  and sends a `Request` only when an application there pastes. The item then
  streams as `Data` chunks of 256 KiB with four chunks in flight per `Ack`, so
  a large payload cannot stall video or buffer without bound; an in-memory
  item is capped at 32 MiB and either side may `Abort`. The server bridges
  `ext-data-control-v1` on its own thread, moving only mime lists and pipe fds;
  the client bridges `gdk::Clipboard` with a lazy `ContentProvider` subclass,
  chunks crossing to the network thread through bounded channels. Files are
  never sent as URIs: the pasting side streams each one into a disk-backed
  spool directory (removed when the offer is replaced or the session ends)
  and hands its applications a URI list pointing there. A loop guard on each
  side (mime-set match on the server, `is_local` on the client) stops a proxy
  we set from being offered back.

## Testing

- **Unit tests** cover the pure logic: AVC444 split/recombine losslessness,
  single-stream subsample/upsample, BGRA↔YUV444 colour round-trip, the H.264
  header parser against an x264 stream, framing with payloads and partial
  writes, the ack-window bounds, keymap building, Hyprland instance
  discovery, and the clipboard rules and engine (mime filtering, URI lists,
  safe paths, the send window and assembler, chunked transfers between two
  engines, the size cap, and a spooled directory tree).
- **`gliff-probe`** is the hardware integration harness: `protocols`,
  `outputs`, `vulkan`, `roundtrip` (synthetic BGRA → encode → decode → PSNR
  against the CPU reference), `capture`, `input`, `pipeline` (a captured
  dmabuf through the exact server and client pipelines), and `serve-test` (a
  headless protocol client). Run it under `VK_LAYER_KHRONOS_validation` after
  touching `gliff-vk`.
- **`scripts/e2e.sh`** boots a nested Hyprland and asserts PASS across the probe
  checks, the GPU pipeline, both Dual420 and Single420 server-plus-client
  streams, and the clipboard in both directions, as text and as a 1 MiB
  binary item. It needs a Hyprland session and a GPU with Vulkan Video, so it
  is not a CI unit test; run it on a target machine.

## Measurements

On the AMD Ryzen 9955HX iGPU (RADV, Mesa 26.2), release build, 1920x1080
Dual420, from `gliff-probe roundtrip`:

| Stage | Time per frame |
|---|---|
| split + two encodes (both submitted, then waited; one encode queue) | ~8.5 ms |
| two decodes + recombine (GPU, one fence wait) | ~5 ms |
| CPU readback of the BGRX frame (probe only, cached host memory) | ~1.5 ms |

No pixel work happens on the CPU at any resolution; the remaining cost is the
encode hardware itself, which serialises the two streams, so a 1080p Dual420
frame costs about two encodes' worth of time. The server adapts the CBR
target to the link (see the session's `BitrateController`), so a slow link
lowers quality rather than frame rate.

## Pending and recommended improvements

Built and validated on AMD: capture, input, Vulkan H.264 encode/decode,
Dual420 and Single420, the server loop, the GTK client, keymap upload, remote
cursor, shortcut inhibit, reconnect, and the clipboard bridge (any mime
type and files, lazily streamed).

Not yet built, roughly in priority order:

1. **Test on Intel ANV and NVIDIA.** Everything runs on one AMD RADV
   machine; a second driver decides whether the per-driver freedom is real.
2. **Optional AV1 for outputs above 4096 wide.** The VCN H.264 encoder
   stops at 4096x4096 (the kernel amdgpu codec table, reported through RADV
   as `maxCodedExtent`), so a 5K output streams at 4096x2304 today: the
   server scales the stream to the encoder maximum and the client scales it
   back up. AV1 and HEVC on the same engine reach 8192x4352. AV1 is the
   preferred second codec: it is royalty-free, and hardware supports it for
   encode on AMD VCN 4.0 (RDNA3, Ryzen 7040) and later, Intel Arc and
   Meteor Lake and later, and NVIDIA RTX 40 and later; for decode on AMD
   VCN 3.0 and later, Intel 11th generation and later, and NVIDIA RTX 30
   and later. H.264 stays as the codec every GPU has. The server picks AV1
   only when both ends list it in `ClientCaps.codecs`, and the size clamp
   stays as the guard for whichever codec is chosen. A native 4:4:4 profile
   on some driver would also retire the split.
3. **Verified ssh path** from a cold machine, including the `WAYLAND_DISPLAY` /
   `XDG_RUNTIME_DIR` environment setup, and a systemd user unit if wanted.
4. **Polish**: multi-output selection UI, `tc netem` tuning of the adaptive
   ack window, and clipboard progress feedback (a large file paste blocks the
   pasting application until the spool is complete, with no UI).

See `docs/hardware-quirks.md` for driver-specific behaviour and the low-severity
items surfaced by code review.

## Dependencies and binaries

The binaries are dynamically linked: `gliff-server`/`gliff-probe` need
libvulkan, libgbm, libdrm, libwayland-client, libxkbcommon and libc, and the
Vulkan loader `dlopen`s the GPU's ICD; `gliff` additionally pulls the
full GTK4 runtime. A normal Hyprland desktop already has all of these (they are
the PKGBUILD `depends`). The Rust side is `ash` (thin generated bindings, no C
build step) plus the Wayland, GTK and async crates. The compute shaders are
committed as SPIR-V, so no shader compiler is needed to build; rerun
`crates/gliff-vk/shaders/build.sh` (needs `glslc`) after editing a shader.
