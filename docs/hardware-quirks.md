# Hardware and driver quirks

gliff targets Vulkan Video on both ends. Driver behaviour differs, so this
file records what we have found and where more testing is needed. Findings so
far come from **one** machine:

- GPU: AMD Granite Ridge iGPU (Ryzen 9 9955HX), VCN 4 class.
- Driver: Mesa RADV 26.2 (Vulkan 1.4), kernel 7.2.
- Compositor: Hyprland 0.56.2.

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

### Decode DPB and output are distinct
`VK_VIDEO_DECODE_CAPABILITY_DPB_AND_OUTPUT_DISTINCT_BIT_KHR` only. The decoder
keeps a DPB array image (`VIDEO_DECODE_DPB`) and a separate ring of output
images (`VIDEO_DECODE_DST | SAMPLED`) the recombine shader samples. The
coincide mode is not implemented.
- **Needs testing:** a driver that only offers coincide mode.

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
  twice on 0.56.2; Hyprland restarts in safe mode). The server ends the
  capture thread, flushes its Wayland connection, and only then removes the
  headless output.

## Hyprland with the Lua config (Omarchy)

`hyprctl keyword` is rejected (`keyword can't work with non-legacy parsers`).
`hypr-ipc` falls back to `eval hl.monitor({ output = ..., mode = ...,
position = "auto", scale = ... })`, which applies at runtime. `output create
headless` and `output remove` are hyprctl commands and work with both config
types. The server reads back the mode Hyprland applied instead of assuming
the request took effect.

## Not yet tested anywhere
- Intel ANV and NVIDIA (proprietary and NVK) for every item above.
- Native 4:4:4 encode (HEVC 4:4:4 / AV1) to retire the dual-stream split.
- `VK_VALVE_video_encode_rgb_conversion` (exposed by RADV here): the encoder
  converts RGB itself, which would remove the split pass for `Single420`.
- Tiled capture buffers. The capture ring prefers linear modifiers and the
  dmabuf import passes the modifier through, but only linear has been run.
- Multiple GPUs / non-renderD128 nodes: `--render-node` matches the DRM
  device number to the Vulkan physical device, untested with two GPUs.

## Known limitations recorded from code review (not yet fixed)

- **Instance discovery tie-break.** `hypr-ipc` picks the newest instance by
  directory mtime; two Hyprland instances started within the same coarse
  filesystem timestamp are tie-broken by name, which could pick the older one.
- **`wl_output` bound at version 4.** A compositor offering an older `wl_output`
  would fail to bind. Hyprland always offers v4.
- **Capture dmabuf uses one buffer-object fd for all planes.** Correct for the
  single-plane XRGB/ARGB formats we select; wrong if a multi-fd planar format is
  ever chosen.
- **ssh environment.** The `--stdio` path is built but not yet tested from a cold
  machine; the ssh session must expose `WAYLAND_DISPLAY` and `XDG_RUNTIME_DIR`.
