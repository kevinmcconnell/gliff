# Hardware and driver quirks

gliff targets Vulkan Video on both ends, with a CPU fallback (OpenH264 in
`gliff-sw`) for machines without it: in the default `--video gpu` mode a
failed device open, a missing encode/decode queue, or a failure to create
the encoder drops to the CPU tier with a log line, and `--video cpu` /
`GLIFF_VIDEO=cpu` forces it. Driver behaviour differs, so this file
records what we have found and where more testing is needed. Findings so
far come from **two** machines:

- GPU: AMD Granite Ridge iGPU (Ryzen 9 9955HX), VCN 4 class.
  Driver: Mesa RADV 26.2 (Vulkan 1.4), kernel 7.2.
  Compositor: Hyprland 0.56.2.
- GPU: Intel Gen12 iGPU.
  Driver: Mesa ANV 26.2 on the i915 KMD, kernel 7.2.
  Compositor: Hyprland 0.56.2.

`gliff-probe vulkan` prints the device and its video queues.

## Confirmed on AMD RADV

### Encode input images may carry STORAGE usage (used)
The `VK_KHR_video_encode_h264` input format query accepts
`VIDEO_ENCODE_SRC | STORAGE` on `G8_B8R8_2PLANE_420_UNORM` with
`MUTABLE_FORMAT | EXTENDED_USAGE`, so the split shader writes straight into
the encoder's input planes through R8 / R8G8 plane views. Each view must be
limited with `VkImageViewUsageCreateInfo`: the plane formats have no video
usage and NV12 has no storage usage, so a view that inherits the image's full
usage is invalid.
- **Needs testing on Intel/NVIDIA:** if STORAGE is refused, fall back to
  writing R8/R8G8 images and `vkCmdCopyImage` into the NV12 planes.

### H.264 encode maximum is 4096x4096
`maxCodedExtent` for H.264 encode is 4096x4096 on VCN 4.0 (RX 7600). The
value is the kernel's static codec table for the VCN generation
(`drivers/gpu/drm/amd/amdgpu/soc21.c`), read by Mesa through
`AMDGPU_INFO_VIDEO_CAPS_ENCODE` and passed on by RADV. No AMD generation
encodes H.264 above 4096 in either dimension; HEVC and AV1 on the same
engine reach 8192x4352. The server and `gliff-probe pipeline` scale a
larger output down to fit (5120x2880 becomes 4096x2304), and
`gliff-probe vulkan` prints the limit.

### Decode DPB and output are distinct
`VK_VIDEO_DECODE_CAPABILITY_DPB_AND_OUTPUT_DISTINCT_BIT_KHR` only. The decoder
keeps a DPB array image (`VIDEO_DECODE_DPB`) and a separate ring of output
images (`VIDEO_DECODE_DST | SAMPLED`) the recombine shader samples. Distinct is
preferred whenever it is offered.

### Encode DPB must be one array image
The encode capabilities report no `SEPARATE_REFERENCE_IMAGES`, so the two
reference slots are layers of one image. Decode allows separate images but the
same array layout is used for both.

### Rate control must ride on every begin
Once CBR is set with `vkCmdControlVideoCodingKHR`, every later
`vkCmdBeginVideoCodingKHR` must carry the same `VkVideoEncodeRateControlInfoKHR`
(+ H.264 layer info) in its pNext, or validation flags VUID 08253 and the
result is undefined. The encoder rebuilds the chain per frame.

### Encoded parameter sets and slices carry no start codes
`vkGetEncodedVideoSessionParametersKHR` and the slice output are raw NAL units;
gliff prepends `00 00 00 01` when a start code is absent, and asks for the SPS
and PPS in two calls so each can be framed.

### Quality level and virtual buffer size barely matter
On synthetic stress content the three quality levels and buffer sizes from
500 ms to 2 s change PSNR by less than 1 dB; bitrate is what matters. The
encoder uses quality level 0 and a 500 ms buffer.

### Host memory for readback should be cached
Reading 8 MB of BGRA back through write-combined host memory took ~25 ms;
through `HOST_CACHED` memory it takes ~1 ms. `HostBuffer` prefers cached
memory and falls back to write-combined. The encoder's bitstream buffer uses
the same path.

## OpenH264 (the CPU tier)

- **The decoder holds one picture for non-Baseline streams.** OpenH264
  skips its reorder buffer only for Baseline-profile streams, so the CPU
  encoder emits Baseline and its streams display with no delay. A
  GPU-encoded (High-profile) stream comes out one access unit late; the
  client drains the held picture after 150 ms of stream silence
  (`Decoder::flush`), which does not disturb later decoding.
- **Per-decode flushing breaks the reference chain.** The `openh264`
  crate's default `Flush::Flush` ejects the reference picture of a
  low-delay stream (dsOutOfMemory on the fourth frame of a RADV-encoded
  stream); the decoder runs with `Flush::NoFlush`.
- **CBR is soft without frame skipping.** The encoder disables
  `skip_frames` so every capture yields a frame (the AVC444 pair must stay
  in step), which OpenH264 says weakens its bitrate cap. The server's own
  `BitrateController` adapts the target from ack timing on top.

## Hyprland cursor capture (0.56.2, and upstream main as of 2026-09-02)

The server captures the remote cursor with an `ext-image-copy-capture-v1`
cursor session. Three Hyprland behaviours shape how `hypr-capture` drives it:

- **Every shared cursor image is fully transparent.**
  `CCursorshareSession::render()` draws the cursor texture only when the
  pointer image has both a buffer and a surface set, and the pointer manager
  never sets both. The buffer is cleared to `{0,0,0,0}` instead, or to opaque
  black when the pointer is on another output. The client therefore treats an
  image with no visible shape as "no remote image" and shows the default
  pointer. Upstream fix: in `render()`, take
  `Pointer::mgr()->getCurrentCursorTexture()` and draw it when non-null.
- **Surface cursors have no constraints.** `calculateConstraints` records the
  format and size only for shm buffer cursors (hyprcursor shapes). If the
  pointer shows a client-provided cursor surface (a terminal's I-beam, say)
  when the capture session is created, the format is invalid and Hyprland
  drops the capture session without sending `stopped`. The capture thread
  detects this with a `wl_display.sync` after creation and recreates the
  cursor session on the next cursor change. If the surface cursor appears
  later, the size stays at the previous value and `capture` fails with
  `stopped`, which is not a real stop: constraints arrive again on the next
  cursor change, so the thread waits for `done`.
- **Constraints arrive before the in-flight frame completes.** On a cursor
  change Hyprland sends `buffer_size` + `done`, then `ready` for the pending
  frame; the next frame only completes on the following change. The capture
  thread must not destroy the in-flight frame on `done`, or every shape after
  the first is lost.

Hyprland also sends the hotspot in logical units while the image is in
physical pixels, so on a scaled output the hotspot will be off once real
images arrive.

- **Removing a monitor under a live cursor session aborts Hyprland.**
  `output remove` on a headless output unmaps its layer surfaces, which
  refocuses and re-renders the cursor; `CCursorshareSession::copy()` then
  renders into the vanishing monitor and `beginRender` aborts (SIGABRT, seen
  three times on 0.56.2; Hyprland restarts in safe mode). The server ends the
  capture thread and only then removes the headless output. Ending the
  thread with a plain flush is not enough: the destroy requests sit in
  Hyprland's queue while `output remove` arrives on the hyprctl socket, and
  Hyprland can handle the remove first. The capture thread therefore ends
  with a roundtrip, which blocks until Hyprland has processed the destroys.

## Hyprland with the Lua config (Omarchy)

`hyprctl keyword` is rejected (`keyword can't work with non-legacy parsers`).
`hypr-ipc` falls back to `eval hl.monitor({ output = ..., mode = ...,
position = "auto", scale = ... })`, which applies at runtime. `output create
headless` and `output remove` are hyprctl commands and work with both config
types. The server reads back the mode Hyprland applied instead of assuming
the request took effect.

## Confirmed on Intel ANV

### Vulkan Video stays hidden until `ANV_DEBUG` asks for it
ANV compiles video support in but gates it off, so every gliff binary
reports `no suitable GPU: no Vulkan device with a compute queue, dmabuf
import and video queues` and falls back to the CPU tier until the
environment carries `ANV_DEBUG=video-decode,video-encode`. With it set ANV advertises
`VK_KHR_video_queue`, `VK_KHR_video_decode_queue`, `VK_KHR_video_encode_queue`,
`VK_KHR_video_decode_h264` and `VK_KHR_video_encode_h264`, plus a second queue
family carrying `VIDEO_DECODE_KHR | VIDEO_ENCODE_KHR`, and `gliff-probe vulkan`
then passes on both the H.264 decode and encode queues. Nothing in the kernel
withholds this: the `vcs0`, `vcs1` and `vecs0` engines are present and GuC and
HuC are authenticated without the variable.

`ANV_VIDEO_DECODE=1` and `ANV_VIDEO_ENCODE=1`, which most search results still
name, do nothing on Mesa 26.2.

### Decode DPB and output coincide
`VK_VIDEO_DECODE_CAPABILITY_DPB_AND_OUTPUT_COINCIDE_BIT_KHR` only, so the
decoder gives each DPB slot its own image with `VIDEO_DECODE_DPB |
VIDEO_DECODE_DST | SAMPLED` usage, decodes into the slot being set up, and
hands that slot to the recombine pass. A spare image past the last slot takes
pictures that are not references.
- **Needs testing:** the Khronos validation layer, which is not installed on
  the Intel machine, so the coincide-mode VUIDs are unchecked.

## Not yet tested anywhere
- NVIDIA (proprietary and NVK) for every item above, and Intel ANV for every
  item not listed under "Confirmed on Intel ANV".
- Baseline-profile streams (the CPU tier's output) through Vulkan decode
  sessions created with the fixed High decode profile: RADV accepts them
  (the e2e cpu-server -> gpu-client case), other drivers are unverified.
- Native 4:4:4 encode (HEVC 4:4:4 / AV1) to retire the dual-stream split.
- `VK_VALVE_video_encode_rgb_conversion` (exposed by RADV here): the encoder
  converts RGB itself, which would remove the split pass for `Single420`.
- Tiled capture buffers. The capture ring prefers linear modifiers and the
  dmabuf import passes the modifier through, but only linear has been run.
- Multiple GPUs / non-renderD128 nodes: `--render-node` matches the DRM
  device number to the Vulkan physical device, untested with two GPUs.

## Known limitations recorded from code review (not yet fixed)

- **Instance discovery tie-break.** `hypr-ipc` picks the newest instance
  whose socket answers, so dead leftovers (a killed nested Hyprland) are
  skipped; two live instances started within the same coarse filesystem
  timestamp are still tie-broken by name, which could pick the older one.
- **`wl_output` bound at version 4.** A compositor offering an older `wl_output`
  would fail to bind. Hyprland always offers v4.
- **Capture dmabuf uses one buffer-object fd for all planes.** Correct for the
  single-plane XRGB/ARGB formats we select; wrong if a multi-fd planar format is
  ever chosen.
- **ssh environment.** The `--stdio` path is built but not yet tested from a cold
  machine; the ssh session must expose `WAYLAND_DISPLAY` and `XDG_RUNTIME_DIR`.
