# Hardware and driver quirks

gliff targets Vulkan Video on both ends. Driver behaviour differs, so this
file records what we have found and where more testing is needed. Findings so
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
  writing R8/R8G8 images and `vkCmdCopyImage` into the NV12 planes. ANV's
  format query echoes whatever usage is asked for, so only `vkCreateImage`
  on the machine tells; `EXTENDED_USAGE` makes the request legal per spec.

### Decode DPB and output are distinct
`VK_VIDEO_DECODE_CAPABILITY_DPB_AND_OUTPUT_DISTINCT_BIT_KHR` only. The decoder
keeps a DPB array image (`VIDEO_DECODE_DPB`) and a separate ring of output
images (`VIDEO_DECODE_DST | SAMPLED`) the recombine shader samples. Distinct is
preferred whenever it is offered.

### Encode DPB must be one array image
The encode capabilities report no `SEPARATE_REFERENCE_IMAGES`, so the two
reference slots are layers of one image. Decode allows separate images but the
same array layout is used for both. ANV reports `SEPARATE_REFERENCE_IMAGES`
and also addresses layers, but gives array video images a special interleaved
layout; if the array DPB fails there, one image per slot (as the decoder's
coincide path does) is the fallback.

### Rate control must ride on every begin
Once a rate control mode is set with `vkCmdControlVideoCodingKHR`, every later
`vkCmdBeginVideoCodingKHR` must carry the same `VkVideoEncodeRateControlInfoKHR`
(+ H.264 layer info for CBR; none for `DISABLED`) in its pNext, or validation
flags VUID 08253 and the result is undefined. The encoder rebuilds the chain
per frame in both modes.

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

## Confirmed on Intel ANV

### Vulkan Video stays hidden until `ANV_DEBUG` asks for it
ANV compiles video support in but gates it off, so every gliff binary exits
with `no suitable GPU: no Vulkan device with a compute queue, dmabuf import
and video queues` until the environment carries
`ANV_DEBUG=video-decode,video-encode`. With it set ANV advertises
`VK_KHR_video_queue`, `VK_KHR_video_decode_queue`, `VK_KHR_video_encode_queue`,
`VK_KHR_video_decode_h264` and `VK_KHR_video_encode_h264`, plus a second queue
family carrying `VIDEO_DECODE_KHR | VIDEO_ENCODE_KHR`, and `gliff-probe vulkan`
then passes on both the H.264 decode and encode queues. Nothing in the kernel
withholds this: the `vcs0`, `vcs1` and `vecs0` engines are present and GuC and
HuC are authenticated without the variable.

Every gliff binary now sets the variable itself at the top of `main`
(`gliff_vk::prepare_driver_env`), adding the two flags to whatever `ANV_DEBUG`
already holds, so users never see it. The gate is still in Mesa `main` (issue
13059 asks for it to go). `ANV_VIDEO_DECODE=1` and `ANV_VIDEO_ENCODE=1`, which
most search results still name, do nothing on Mesa 26.2 (they were the names
up to Mesa 25.0).

### Encode exists only on Gen12 and Gen12.5
`anv_physical_device.c` enables `VK_KHR_video_encode_queue` only for
`verx10 <= 125`: Tiger Lake through Raptor Lake, Arc Alchemist and Meteor
Lake (Gen12.5 came back in Mesa 26.2). Lunar Lake, Battlemage and newer have
decode only, so they can run the client but not the server. The i915 and Xe
kernel drivers make no difference to ANV's video code.

### Encode has no bitrate control: constant QP only (from Mesa source, untested)
`VkVideoEncodeCapabilitiesKHR::rateControlModes` is `DEFAULT | DISABLED`; there
is no CBR or VBR, and `vkCmdControlVideoCodingKHR` asserts on any other mode.
In `DISABLED` the driver takes the QP from
`VkVideoEncodeH264NaluSliceInfoKHR::constantQp` (`minQp` 10, `maxQp` 51); in
`DEFAULT` it uses `pic_init_qp` from the PPS and ignores bitrates. One quality
level, one slice per picture, level 5.1, `maxCodedExtent` 4096x4096, CABAC
and 8x8 transform advertised, scaling matrices and weighted prediction not.

The encoder picks its mode from the capabilities: CBR when offered, else
`DISABLED` with a per-frame QP chosen by `QpController` from the bytes each
frame produced: a frame over budget raises the QP by up to 4 steps in
proportion to the overshoot, frames under budget lower it by 1 once a 500 ms
bucket is clear, an IDR is charged one frame budget at most, and the floor is
QP 16 so a static desktop does not race to the driver minimum. The server's
bitrate adaptation is unchanged; it retargets the controller.
`gliff-probe roundtrip --constant-qp` forces this path on a driver that has
CBR too, and on RADV it decodes 60/60 frames at the same PSNR as CBR.
`gliff-probe vulkan` prints which mode a machine will take.
- **Needs testing on Intel:** the whole server path. Nobody has run
  `gliff-probe roundtrip` or `gliff-server` on ANV yet.

### Bitstream buffer size must be a multiple of 4096
ANV reports `minBitstreamBufferSizeAlignment` 4096 and offset alignment 32.
The encoder now rounds its bitstream buffer (and so `dstBufferRange`) up to
the reported alignment; the old `coded_width * coded_height * 2` was not a
multiple for most sizes (1888x1072, the Intel tester's stream, gives 4047872).
RADV never minded.

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
