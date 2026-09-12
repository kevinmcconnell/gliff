# A Vulkan-based haver: design study

Status: **study only, not scheduled.** This document describes how haver would
look if the media pipeline were rebuilt on Vulkan instead of VA-API + EGL/GL. It
exists so the approach can be evaluated later; nothing here is built.

## Why consider Vulkan at all

Today haver uses three separate GPU-facing APIs:

- **GBM** to allocate and map capture dmabufs.
- **VA-API** (through cros-codecs / cros-libva) to encode and decode H.264.
- **EGL + GL ES** (through `haver-gl`) to import dmabufs and run the AVC444
  recombine shader on the client and the split shader on the server.

Each boundary is a place where buffers are exported and re-imported, where a
different driver code path is exercised, and where synchronisation is implicit
(a `glFinish`, a `vaSyncSurface`). The split lives in one API, the encode in
another, and passing a surface between them means `vaExportSurfaceHandle` on one
side and an EGL dmabuf import on the other.

Vulkan can host all of it in **one** API, one device, one allocator, one
synchronisation model:

- **Vulkan Video** (`VK_KHR_video_queue`, `VK_KHR_video_encode_queue`,
  `VK_KHR_video_encode_h264`, and the decode equivalents) does the H.264 encode
  and decode.
- **Compute shaders** do the BGRA→YUV 4:4:4 conversion and the AVC444 split (and
  the recombine on the client), reading and writing `VkImage`s directly.
- **External memory** (`VK_EXT_external_memory_dma_buf`,
  `VK_EXT_image_drm_format_modifier`, `VK_KHR_external_memory_fd`) imports the
  captured Wayland dmabuf as a `VkImage` and exports encoder input images back
  as dmabufs if needed.
- **Timeline semaphores** (`VK_KHR_timeline_semaphore`) sequence capture →
  compute → encode with explicit, cross-queue synchronisation and no CPU stalls.

The result is a genuinely zero-copy path where a captured frame is imported once
and never leaves GPU memory until the bitstream is read back to the CPU for the
socket.

## What stays the same

Vulkan changes only the media pipeline. These parts are unaffected and would be
reused as-is:

- `haver-proto`, `haver-transport` (wire format, framing, ssh spawn).
- `hypr-ipc`, `hypr-wl` (Hyprland control, Wayland plumbing).
- `hypr-capture`'s Wayland side (`ext-image-copy-capture-v1`, the dmabuf
  negotiation). Only the buffer *allocation* would move from GBM to Vulkan
  external memory, and even GBM allocation can stay if the dmabuf is then
  imported into Vulkan.
- `hypr-input` (virtual keyboard/pointer, clipboard).
- The AVC444 layout itself (`haver-proto::chroma`) — the same split/recombine
  math, expressed as compute shaders instead of GL fragment shaders and CPU
  loops. The existing bit-exact CPU reference stays as the test oracle.

## The Vulkan pipeline, end to end

### Server

1. **Capture** hands out a Wayland dmabuf (fd + `DrmFourcc` + modifier + planes),
   exactly as now.
2. **Import** it as a `VkImage` with `VkImportMemoryFdInfoKHR` +
   `VkImageDrmFormatModifierExplicitCreateInfoEXT`, matching the modifier the
   compositor used. No copy.
3. **Compute split.** One dispatch (or a few) of a compute shader reads the
   imported BGRA image and writes two NV12 images — main (Y + even chroma) and
   aux (the dropped chroma), the AVC444 layout. NV12 as a Vulkan image is a
   two-plane `VK_FORMAT_G8_B8R8_2PLANE_420_UNORM` (or two single-plane R8/RG8
   images), written through storage-image views. This replaces both the CPU
   `bgra_to_yuv444` and the GL split.
4. **Encode.** The two NV12 images are the input pictures for two
   `VK_KHR_video_encode_h264` sessions. Vulkan video encodes directly from the
   `VkImage`; there is no upload and no export/import, because the split output
   and the encode input are the same Vulkan images on the same device.
5. **Read back** the coded bitstream from the encode output buffer and frame it
   onto the socket, as now.

Synchronisation between steps 3, 4 and 5 is a timeline semaphore, so the encode
queue waits on the compute queue by value with no CPU round-trip.

### Client

1. **Decode.** Two `VK_KHR_video_decode_h264` sessions decode the main and aux
   streams into NV12 `VkImage`s.
2. **Compute recombine.** A compute shader reads both NV12 images and writes a
   full 4:4:4 (or directly an RGBA) image — the inverse of the split, plus the
   YUV→RGB convert folded in.
3. **Present.** Blit or sample the RGBA image into the GTK4 surface. If the
   client stays on GTK4, the Vulkan image is exported as a dmabuf and handed to
   GTK through a `GdkDmabufTexture` (GTK 4.14+), or GTK's own Vulkan renderer is
   targeted. A pure-Vulkan client (winit + a swapchain) is the cleaner long-term
   shape but drops libadwaita.

## Crate and binding choices

Vulkan Video is new enough that the ergonomic wrappers do not cover it well:

- **`ash`** — thin, generated Vulkan bindings. Exposes the video extensions as
  they are added to the registry. This is the realistic choice; it is entirely
  `unsafe`, so the whole media layer would be `unsafe` (the opposite of today's
  "one crate" containment). Mitigate by wrapping `ash` in a safe facade module,
  the way `haver-gl` wraps GL today, but the surface is far larger.
- **`vulkano`** — safe wrapper, but its Vulkan Video support has lagged the spec;
  would likely need forking or waiting.
- **`wgpu`** — no Vulkan Video at all (no video encode/decode in the WebGPU
  model). Usable only for the compute split, not the codec. Not sufficient.

So a Vulkan haver is an `ash`-based media crate with a hand-written safe facade,
plus GLSL/SPIR-V compute shaders compiled at build time (`glslang` or `naga`).

## Driver reality on the target hardware

The current test machine is AMD (RADV / VCN). Status to confirm before building:

- **Encode.** RADV has `VK_KHR_video_encode_h264` and `_h265` on VCN; AV1 encode
  has been landing. This is the load-bearing dependency — verify H.264 encode is
  present and stable on this VCN generation with `vulkaninfo`.
- **Decode.** RADV has `VK_KHR_video_decode_h264` / `_h265` on VCN.
- **dmabuf modifier import.** `VK_EXT_image_drm_format_modifier` is supported on
  RADV; the captured tiled modifier must be in the format's supported modifier
  list for the chosen usage (sampled + storage), which constrains which
  compositor buffers can be imported without a copy.
- **NV12 storage images.** Writing planar YUV from a compute shader needs the
  multi-plane disjoint image to support `STORAGE` usage per plane, or a
  workaround writing two R8/RG8 images and aliasing them into the encode input.
  This is the fiddliest correctness detail and must be prototyped first.

Intel (ANV) and NVIDIA (proprietary and NVK) have their own maturity levels;
Vulkan Video is the same API across all three, which is a portability argument
in Vulkan's favour over VA-API's per-driver quirks (the CBR, NAL-header, and
upload workarounds documented in `hardware-quirks.md`).

## Trade-offs versus the current stack

Advantages:

- **One API** for import, convert, split, encode, decode, recombine, present.
  No GBM/VA/EGL boundaries, no export/import between them.
- **True zero-copy** server path: captured dmabuf → compute → encode, all the
  same device memory. The GL split we built still exports/imports the encoder
  surface; Vulkan removes even that.
- **Explicit synchronisation** with timeline semaphores instead of `glFinish` /
  `vaSyncSurface` blocking calls.
- **Portable codec path**: the same extensions on AMD, Intel, NVIDIA, rather
  than VA-API driver-specific behaviour.
- **Cross-vendor 4:4:4 and 10-bit** become easier to reach if a driver exposes a
  4:4:4 or 10-bit encode profile, without the AVC444 dual-stream trick.

Costs:

- **`unsafe` everywhere in the media layer.** Today all `unsafe` is in
  `haver-gl` (a few hundred lines). An `ash` pipeline is thousands of lines of
  `unsafe`, only partly hidden behind a facade. This directly conflicts with the
  project's current "unsafe confined to one small crate" property.
- **Verbosity.** Vulkan Video setup (video profiles, session parameters, DPB
  management, rate-control structs, per-frame reference management) is large and
  error-prone compared with cros-codecs' encoder API.
- **Driver maturity.** Vulkan Video is younger than VA-API. Bugs are more likely
  and less documented; the H.264 bitstream headers (SPS/PPS/slice) are partly the
  application's responsibility, similar to the header work already done here.
- **Client rewrite.** Best done as a Vulkan-native window (winit + swapchain),
  which means leaving GTK4/libadwaita, or an awkward GTK dmabuf-texture bridge.
- **A large rewrite** for a benefit that, on this hardware, mostly reproduces
  what the VA-API + GL-split path already achieves. The measured server-split win
  is ~2x wall-clock; Vulkan would remove one more export/import but not change
  the order of magnitude.

## A phased plan, if pursued

Each phase is independently verifiable, mirroring how the VA-API path was built.

- **Phase V0 — feasibility probe.** Extend `haver-probe` with a `vk` subcommand:
  enumerate the device, print which video encode/decode profiles, dmabuf modifier
  imports, and NV12 storage-image usages RADV exposes. This decides whether the
  rest is possible on the target hardware before any pipeline work. (Analogue of
  the `gl-test` and `vaapi` probes.)
- **Phase V1 — compute split only.** Import a captured dmabuf as a `VkImage`, run
  the BGRA→2×NV12 compute split, read the two NV12 images back, and check them
  against the existing bit-exact CPU reference (PSNR, as `pipeline --gl` does).
  Keep VA-API for the actual encode at this stage (export the Vulkan NV12 images
  as dmabufs into VA). This isolates the compute-shader correctness.
- **Phase V2 — Vulkan encode.** Replace the VA encode with two
  `VK_KHR_video_encode_h264` sessions fed by the compute output images. Verify
  the emitted bitstream decodes on the existing VA-API client. Handle SPS/PPS and
  rate control (CBR, matching the current default).
- **Phase V3 — Vulkan decode + recombine (client).** Two decode sessions plus a
  recombine compute shader producing RGBA, presented through the existing GTK
  path via an exported dmabuf texture. Now both ends are Vulkan; VA-API can be
  dropped from the media crate.
- **Phase V4 — consolidation.** One `haver-vk` crate with the safe facade,
  timeline-semaphore scheduling across capture/compute/encode, and a
  feature-flagged choice between the Vulkan and VA-API backends so the two can be
  compared on the same hardware. Optionally a Vulkan-native client window.

## Recommendation

Do not start a Vulkan rewrite for performance on the current hardware: the
VA-API path plus the built GPU split already covers the measured need, and the
`unsafe` cost is high. The reasons to revisit Vulkan are strategic rather than
performance: **cross-vendor codec portability** (one API instead of per-driver
VA-API workarounds) and **native 4:4:4 / 10-bit encode** if a driver exposes it,
which would retire the AVC444 dual-stream trick entirely. If those become goals,
start at Phase V0 to confirm the hardware, then V1 to prove the compute split,
before committing to the encode/decode rewrite.
