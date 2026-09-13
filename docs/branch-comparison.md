# Branch comparison: `main` (VA-API) vs `vulkan` (Vulkan Video)

This document compares the two candidate directions for haver's media pipeline
so the project owner can choose one. Evidence comes from the code and the
`docs/` files on each branch. All measurements come from the **same** AMD
Ryzen 9955HX iGPU; neither branch has run on any other GPU.

- **`main`** — VA-API media pipeline. It vendors patched `cros-codecs` and
  `cros-libva`, converts colour and splits AVC444 on the CPU, and uploads NV12
  to two VA-API H.264 encoders. The client decodes to dmabufs and recombines in
  GL (crate `haver-gl`), or on the CPU when GL import is not available. An
  optional server-side GL split into VA surfaces is built but not wired in.
- **`vulkan`** — Vulkan Video media pipeline in crate `haver-vk` (uses `ash`).
  It imports the captured dmabuf once, runs the colour convert and AVC444 split
  in a compute shader, encodes with `VK_KHR_video_encode_h264`, decodes with
  Vulkan Video, and recombines in a compute shader. GTK shows the result as a
  dmabuf texture. The vendored crates, VA-API and GL are removed.

The short version: `vulkan` is the simpler and cleaner system to own, and it
removes all CPU pixel work, but it stands on a newer, less-proven driver
feature and it makes haver own an H.264 header parser. `main` is the
conservative, broadly-supported path, but it carries ~56k lines of vendored
third-party code with eight local patches and touches three GPU APIs.

---

## 1. Simplicity

### First-party code size

| | Files (`.rs`) | Lines |
|---|---|---|
| `main` first-party (excludes `vendor/`) | 33 | 9,485 |
| `vulkan` first-party | 33 | 10,854 |
| `main` vendored (`cros-codecs` + `cros-libva`) | 138 | 56,299 |

The `vulkan` branch has **1,369 more lines of first-party code** than `main`.
That is the cost of owning the codec logic directly. But `main` also carries
**56,299 lines of vendored third-party Rust** that `vulkan` deletes entirely.
The vendored code is third-party, so it is not "yours" to write — but it is
yours to build, audit, and re-base whenever you take an upstream update,
because you carry local patches inside it.

The media crate maps as follows. On `main`, `haver-codec` (1,741 lines) plus
`haver-gl` (877 lines) = 2,618 lines, and they lean on the vendored
`cros-codecs` for H.264 parsing, the DPB, and the encode/decode state machines.
On `vulkan`, `haver-vk` (4,203 lines) does all of that itself:

| `haver-vk` file | Lines | Role |
|---|---|---|
| `encoder.rs` | 860 | `VK_KHR_video_encode_h264` session, rate control |
| `decoder.rs` | 731 | Vulkan Video decode, DPB management |
| `image.rs` | 669 | dmabuf import/export, NV12 plane views |
| `h264/parser.rs` | 587 | SPS/PPS/slice-header parser (our own) |
| `device.rs` | 464 | instance, device, queues |
| `pipeline.rs` | 362 | the server/client pipeline glue |
| `compute.rs` | 289 | split/recombine compute dispatch |
| `h264/{bits,annexb,mod}.rs` | 196 | bit reader, Annex-B framing |

So `haver-vk` is ~1,585 lines larger than the two crates it replaces, and
the growth is exactly the codec ownership: the H.264 parser and the DPB logic
that used to live in vendored `cros-codecs`.

### Crates and dependencies

- Workspace members: `main` has **11**, `vulkan` has **10** (drops
  `haver-codec` and `haver-gl`, adds `haver-vk`).
- Resolved crate graph (`Cargo.lock`): `main` **256**, `vulkan` **227** —
  **29 fewer** on `vulkan`.
- `main` pulls `bindgen` and `clang-sys` (for `cros-libva`'s `build.rs`),
  `glow`, and `khronos-egl`. `vulkan` drops all four and adds one crate: `ash`
  0.38 (thin, generated Vulkan bindings, no C build step).

### Build requirements

- `main` needs `clang` at build time (bindgen generates the libva bindings).
  Its PKGBUILD `makedepends` lists `clang`; `depends` lists `libva`.
- `vulkan` needs no C toolchain beyond `pkgconf`. It drops `clang`. It commits
  the compute shaders as SPIR-V (`split.spv`, `recombine.spv`), so no shader
  compiler is needed to build either. Runtime dependency shifts from `libva` +
  a VA driver to `vulkan-icd-loader` + a Vulkan ICD.

### Distinct GPU APIs touched

- `main`: **three** — GBM (capture buffers), VA-API (encode/decode), and
  EGL/GL (client recombine, and the optional server split). Each hand-off
  between them needs surface export/import glue.
- `vulkan`: **one and a fraction** — GBM for capture buffers, then Vulkan for
  everything else (import, colour, split, encode, decode, recombine, export).
  GTK does the final dmabuf import itself, so the client owns no display GPU
  code.

### `unsafe` surface

| | `unsafe` blocks | Where |
|---|---|---|
| `main` first-party | 32 | `haver-gl` (30), `haver-client` (1), `hypr-capture` (1) |
| `main` vendored | 135 | inside `cros-codecs` / `cros-libva` |
| `vulkan` first-party | 47 | `haver-vk` (44), `haver-client` (2), `hypr-capture` (1) |

Read this carefully. If you count only first-party code, `main` (32) looks
safer than `vulkan` (47). But `main`'s real trust boundary includes the 135
`unsafe` blocks in the vendored codec libraries, because those libraries parse
untrusted input (see §4). `vulkan` concentrates its 44 codec `unsafe` blocks in
one crate you wrote, each with a `SAFETY` comment (43 present), checked under
the Khronos validation layer. So `vulkan` has more `unsafe` that you own and
can audit, and far less `unsafe` overall inside the trust boundary.

### Patch burden (vendored, `main` only)

`main` carries **7 functional patches to `cros-codecs`** (low-latency DPB
output, access-unit-delimiter handling, a 320x240 placeholder context for
radeonsi, forced true-IDR keyframes, a public constructor for the image-upload
path, and two SPS/reorder changes) plus **1 compile fix to `cros-libva`** (a
libva 1.23 struct field). Every upstream update must be re-based over these
eight patches. `vulkan` has zero vendored code and zero patches.

---

## 2. Architecture

### Data flow and CPU copies per frame

`main` (from `docs/architecture.md`): the server maps the capture dmabuf to
CPU BGRA, converts colour (BT.709) and splits AVC444 on the CPU with `rayon`,
then uploads two NV12 buffers to the encoders with `vaPutImage`. The client
decodes to dmabufs and recombines either in GL or on the CPU. So the default
path touches every pixel on the CPU at least twice per frame (colour+split on
the server, recombine+colour on the client, unless the GL client path is
active).

`vulkan`: "Nothing touches pixels on the CPU." The captured dmabuf is imported
once per ring slot as a `VkImage`, the split shader writes the encoders' input
images in place, and only the coded bytes return to the CPU. On the client the
decoders write NV12 images, the recombine shader writes a linear BGRX image,
and GTK imports that as a dmabuf texture. This is the central architectural
difference and it is real: the CPU pixel stages that `main` measures simply do
not exist on `vulkan`.

`main` has a built, measured server GL split that would also remove the
server CPU work, but it is **not wired into the live session** — it needs the
captured dmabuf held on the encoder thread until `glFinish` returns, a
capture-lifecycle change the author deliberately deferred. The client GPU path
*is* live on `main` (auto-selected, with CPU fallback).

### Threading and synchronisation

Both branches use the same shape: current-thread tokio runtimes own the
`!Send` codec objects, capture and input run on their own calloop threads and
talk over channels, and reads and writes are split into independent tasks so an
input burst cannot starve video. `vulkan` orders all GPU work with a single
timeline semaphore on one device — a simpler synchronisation story than
`main`'s GBM→VA-API→EGL hand-offs, which each need their own export/import and
fence discipline. On `vulkan` the capture thread hands the whole
`CapturedFrame` to the encoder, which releases the ring slot when the encode
finishes — the same lifecycle change `main` still has to make for its server
split.

### Per-driver workarounds

`main`'s `docs/hardware-quirks.md` lists five radeonsi-specific fixes it needs
to work at all: CBR-only (the CQP path miscompresses luma), rewriting zeroed
slice NAL header bytes, a 320x240 placeholder context, a single-linear-buffer
NV12 allocation, and upload-not-import for encoder input. These are workarounds
for VA-API driver behaviour, and the doc repeatedly flags "needs testing on
Intel" because the behaviour is per-driver.

`vulkan`'s `docs/hardware-quirks.md` lists a comparable set, but they read as
capability negotiation rather than bug workarounds: STORAGE usage on encode
input images, DPB-and-output-distinct decode, single-array-image encode DPB,
rate control re-sent on every begin, start-code insertion, and cached host
memory for readback. These are "this is how the Vulkan Video spec works on this
driver" facts, most of which the spec lets you *query*. Neither branch escapes
driver-specific behaviour, but `vulkan`'s is spec-defined and queryable, while
`main`'s CQP/NAL-header issues are genuine driver bugs it must code around.

### Portability

This cuts both ways and honesty matters here:

- VA-API is a mature, widely-deployed API. But its **per-driver behaviour is
  exactly why `main` needs the quirks file**, and `main` has only ever run on
  radeonsi. Intel is expected to work but is untested; several fixes are gated
  "confirm on Intel."
- Vulkan Video is a **newer** API. `VK_KHR_video_encode_h264` was finalised in
  late 2023 and is still maturing in drivers. `vulkan` has run on **one AMD
  RADV machine only**. NVIDIA and Intel ANV/NVK are completely untested, and
  encode support in those drivers is less mature than their decode support.

So both are single-GPU-tested today. `vulkan`'s API promises "the same code on
any driver with Vulkan Video," but that promise is unverified and the feature
is young. `main`'s API is older and more deployed, but its own design proves
that "VA-API" does not mean "portable without per-driver work."

### Maintainability

`vulkan` trades a large vendored dependency (56k lines, 8 patches, periodic
re-basing against a ChromeOS-focused upstream) for owning ~1,400 lines of
H.264 parser and DPB logic yourself. Owning a codec header parser is a real,
ongoing liability (correctness and security — see §4). But so is carrying a
patched fork of someone else's codec stack. `vulkan`'s dependency, `ash`, is a
thin mechanically-generated binding that tracks the Vulkan spec and changes
little.

---

## 3. Performance

The two branches record **different kinds of numbers**, so compare them with
care.

`main` (`docs/architecture.md`) records **per-stage CPU costs**, not end-to-end
frame time, on the 9955HX iGPU with `rayon`:

| Stage | 720p | 1080p | 4K |
|---|---|---|---|
| BGRA→YUV444 (server) | 0.7 ms | 1.0 ms | 6.2 ms |
| split →2×NV12 (server) | 0.3 ms | 0.6 ms | 3.0 ms |
| recombine (client) | 0.3 ms | 0.6 ms | 3.0 ms |
| YUV444→BGRA (client) | 0.7 ms | 1.5 ms | 5.5 ms |

So `main`'s CPU pixel work is ~1.6 ms server + ~2.1 ms client at 1080p, rising
to ~9 ms + ~8.5 ms at 4K. Its built-but-unwired GL server split does 1080p in
1.16 ms (GPU) vs 2.75 ms (CPU), ~2x and growing with resolution. **These
numbers exclude the VA-API encode and decode time**, which run on the VCN
hardware block.

`vulkan` (`docs/architecture.md`, "Measurements") records **GPU pipeline
stages including encode and decode**, 1080p Dual420, from `haver-probe
roundtrip`:

| Stage | Time/frame |
|---|---|
| split + two encodes (serial on one encode queue) | ~9 ms |
| two decodes + recombine (one fence wait) | ~5 ms |
| CPU readback of BGRX (probe only) | ~1.5 ms |

What is and is not comparable:

- **Not comparable directly.** `main`'s table is CPU-stage cost with the
  encoder/decoder time excluded; `vulkan`'s table is GPU pipeline time with the
  encodes and decodes *included*. You cannot subtract one from the other.
- **The encode hardware is the same either way** — both branches drive the same
  VCN H.264 block on this iGPU. Neither branch's design changes raw encode
  throughput; they change how much *other* work surrounds it.
- `vulkan`'s headline win is that **the CPU pixel stages from `main`'s table
  (~1.6 + ~2.1 ms at 1080p, ~9 + ~8.5 ms at 4K) are gone entirely** — moved
  onto the GPU. This matters most at 4K, where `main`'s CPU cost is a real
  bottleneck.

Known performance work left:

- `main`: wire the server GL split into the live session (needs the
  capture-lifecycle change); the client GPU path already ships.
- `vulkan`: pipeline the two encodes — they are currently submitted and waited
  serially on one encode queue, so the main stream's readback does not overlap
  the aux encode. The ~9 ms split+2-encodes figure should drop once pipelined.
  Also fence the display ring rather than reusing an image after three frames.

---

## 4. Quality

### Test coverage

- First-party test functions: `main` **18**, `vulkan` **23**.
- Both cover the same pure logic: AVC444 split/recombine losslessness,
  single-stream subsample/upsample, BGRA↔YUV444 colour round-trip, framing with
  partial writes, ack-window bounds, keymap building, Hyprland discovery.
  `vulkan` adds a test of its **own H.264 header parser against a real x264
  stream** (`h264/testdata/x264-64x64.264`) — a test `main` does not need
  because it does not parse headers itself.
- Both ship `haver-probe` with the same subcommand shape (`protocols`,
  `outputs`, the GPU probe, `roundtrip` PSNR, `capture`, `input`, `pipeline`,
  `serve-test`) and both ship `scripts/e2e.sh` (nested Hyprland, asserts PASS
  across Dual420 and Single420). `vulkan`'s `pipeline` probe checks its compute
  shaders against the CPU reference in `haver-proto::chroma`; `main`'s checks
  the GL split against the CPU split at 65–74 dB.

### Validation tooling

- `vulkan` was checked under the **Khronos validation layer**
  (`VK_LAYER_KHRONOS_validation`), which catches API misuse, synchronisation
  hazards, and lifetime errors that `unsafe` `ash` calls would otherwise hide.
  The docs instruct running the probe under it after any `haver-vk` change.
- `main` relies on `cros-codecs`' safe Rust API to contain the VA-API `unsafe`,
  and on the vendored libraries' own test suites. There is no equivalent
  runtime validation layer for the GL path.

### Robustness to hostile input

This is the most important quality difference. The decoder consumes an H.264
bitstream, which on the wire is **untrusted input** (even over SSH, a
compromised or buggy server is the adversary).

- `main` hands untrusted headers to **`cros-codecs`' parser**, which is
  battle-tested in ChromeOS across many streams and fuzzers. That is a genuine
  strength.
- `vulkan` **parses untrusted SPS/PPS/slice headers itself** in
  `haver-vk/src/h264/parser.rs` (587 lines). This is new attack surface. It was
  put through an **adversarial review**, and the findings were fixed in commit
  `a764e42` ("Harden the decoder against hostile streams"): it now rejects an
  SPS whose coded size exceeds the device limit *before* allocating any image,
  bounds every syntax field (`log2_max_* ≤ 12`, ref frames ≤ 16, POC cycle ≤
  255, picture size ≤ 1023 MBs), uses saturating/`i64` arithmetic so even a
  debug build cannot panic, errors on `bits(n)` for `n > 32`, checks the encode
  feedback status as `VkQueryResultStatusKHR`, and frees the image on a failed
  `import_dmabuf` bind. The review found **no memory-safety defects**. An
  earlier review (`1de7a2c`) fixed logic issues shared by both branches (NAL
  fixup, frame pairing by timestamp, read/write task separation).

So `main` inherits a mature parser for free; `vulkan` owns a small, hardened,
reviewed one. `vulkan`'s is the smaller and more auditable body of code, but it
has far less exposure to real-world streams than `cros-codecs` has.

### Known open limitations

Shared by both (recorded in each `hardware-quirks.md`): the Hyprland instance
tie-break by mtime, `wl_output` bound at v4, the single-fd assumption for
capture dmabuf planes, and the untested `--stdio` ssh environment.

- `main` only: the server GPU split is not wired in; native single-stream
  4:4:4 and AV1/HEVC are probe-gated and unreachable on this AMD GPU; VPP is
  Single420-only and not reachable through the vendored `cros-libva` 0.0.12.
- `vulkan` only: the two encodes and two decodes are waited serially, not
  pipelined; the display ring reuses an image after three frames without a GTK
  fence (possible tearing on a very slow compositor).

---

## 5. Risk over the next year

### `main` (VA-API)

- **Upstream churn.** `cros-codecs` is pinned at 0.0.6 and `cros-libva` at
  0.0.12, both pre-1.0 and ChromeOS-driven. Taking any update means re-basing
  8 local patches; skipping updates means drifting from upstream security and
  correctness fixes. This is a standing tax.
- **Driver bugs are real and per-driver.** `main` already codes around a
  radeonsi CQP luma-compression bug and a zeroed-NAL-header bug. Intel or a
  Mesa update could surface new ones; the design assumes VA-API behaviour that
  is not portable.
- **Three GPU APIs.** GBM + VA-API + EGL/GL is more surface to break across
  driver and library updates, and more glue to maintain.
- **Bus factor.** Understanding `main` means understanding VA-API, EGL/GL, and
  a large patched vendored codec stack. Lower on first-party lines, higher on
  total trust-boundary code.

### `vulkan` (Vulkan Video)

- **Driver maturity is the top risk.** `VK_KHR_video_encode_h264` is young.
  It works on RADV here, but NVIDIA and Intel encode paths are untested and
  historically lag decode. A driver that only offers decode "coincide" mode, or
  refuses STORAGE on encode input, needs a fallback that is noted but not built.
- **Single-machine validation.** Everything is proven on one AMD iGPU. The
  "same code on any driver" claim is unverified.
- **You own the codec.** The H.264 parser and DPB are yours to keep correct and
  secure. It is hardened and reviewed, but real-world stream exposure is thin
  compared with `cros-codecs`.
- **Lower churn, smaller surface.** Against those risks: `ash` is a stable thin
  binding, there is one GPU API, no vendored patches, and no C build step. The
  bus factor is "understand Vulkan + our one 4.2k-line crate" — more
  self-contained than `main`'s stack.

---

## Recommendation

**Continue on `vulkan`, provided you can confirm it on at least one non-AMD
GPU within the next milestone.**

The reasoning:

1. It is the structurally simpler system to own long-term: one GPU API instead
   of three, 29 fewer dependencies, no C toolchain, and — decisively — it
   deletes 56k lines and 8 patches of vendored third-party code.
2. It achieves the performance goal `main` only partly reached: **zero CPU
   pixel work**. `main`'s server GPU path is built but still not wired in, and
   even wired it would leave `main` juggling three APIs to get there.
3. Its added risks are concentrated and already partly retired: the H.264
   parser is small, hardened, adversarially reviewed with no memory-safety
   defects, and validation-layer clean. Its `unsafe` is larger in first-party
   count but far smaller inside the real trust boundary than `main`'s
   vendored 135 blocks.

**Choose `main` instead if any of these hold:**

- **You must ship to NVIDIA or Intel users soon.** VA-API is deployed and
  proven on those vendors today; Vulkan Video encode is not, and `vulkan` is
  untested there. If broad hardware support in the next year is the priority,
  `main`'s maturity outweighs its complexity — but budget the Intel testing
  `main`'s own quirks file keeps flagging.
- **You are not prepared to own a codec.** If maintaining an H.264 parser and
  DPB is a burden the team cannot carry, `cros-codecs` doing that for you is a
  real advantage, and `main`'s vendoring tax is the price of it.
- **Vulkan Video encode proves unstable on your target drivers.** If a survey
  of RADV/ANV/NVIDIA shows the encode extension is not dependable yet, defer:
  keep `vulkan` as the intended direction but ship on `main` until the drivers
  catch up.

The single most decision-relevant unknown is the same for both branches:
**neither has run on more than one GPU.** Resolve that for `vulkan` first,
because a second working GPU converts its biggest risk (unproven driver
support) into its biggest advantage (one portable API, no per-driver
workarounds).

## Post-draft note

Two `vulkan` items listed above as work left were fixed after this comparison
was drafted (commit `add5bda`): the two encodes are now submitted together and
waited once, and the display ring is fenced by GTK's texture release callback.
The remaining open item on `vulkan` is the second-GPU test.
