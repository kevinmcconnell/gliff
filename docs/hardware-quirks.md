# Hardware and driver quirks

haver targets VA-API on both ends. VA-API behaviour differs between drivers, so
this file records what we have found and where more testing is needed. Findings
so far come from **one** machine:

- GPU: AMD Granite Ridge iGPU (Ryzen 9 9955HX), VCN 4/5 class.
- Driver: Mesa `radeonsi` VA-API 26.2, libva 2.24, kernel 7.2.
- Compositor: Hyprland 0.56.2.

## Confirmed on AMD radeonsi

### Encoder needs CBR, not CQP (blocker, fixed)
cros-codecs' constant-QP path sends the rate-control buffer with
`bits_per_second = 0`. On radeonsi this makes the H.264 encoder compress the
luma range: a decoded pixel comes back as `input / 2 + 64` (a constant 64 in,
say, 96 out). It is a clean, deterministic transform, not quantisation.
ffmpeg's own `h264_vaapi` encodes the same input correctly, so the hardware is
fine; the fault is the CQP setup. **Fix:** haver drives the encoder in CBR
(`RateControl::ConstantBitrate`). With CBR a synthetic ramp round-trips at
about 52 dB PSNR. `haver-codec` therefore has no CQP path.
- **Needs testing on Intel:** whether Intel media-driver also needs CBR, or
  whether CQP works there. If CQP works on Intel, make the mode driver-gated.

### Encoder zeroes the slice NAL header byte (fixed)
The radeonsi encoder returns each coded slice with its NAL unit header byte set
to `0x00`, because cros-codecs does not supply a packed slice header. haver
rewrites it: IDR slices to `0x65`, referenced P slices to `0x61`
(`nal_ref_idc = 3`). See `haver-codec/src/h264.rs`.
- **Needs testing on Intel:** Intel drivers usually return a complete NAL. The
  rewrite only fires on a `0x00` header, so it should be a no-op there, but
  confirm.

### Decoder rejects a tiny placeholder context (fixed, in vendored cros-codecs)
cros-codecs creates its first decode context at 16x16; radeonsi rejects that
with `RESOLUTION_NOT_SUPPORTED`. The vendored copy uses 320x240. Intel accepts
16x16, so this is harmless elsewhere.

### NV12 allocation must be a single linear buffer (fixed)
GBM on radeonsi refuses a multi-planar NV12 `create_buffer_object`. haver
allocates one linear `R8` buffer of `height * 3/2` rows and places the UV plane
at `stride * height`. Every importer we use takes explicit plane offsets, so
this is portable, but the exact modifier handling wants checking on Intel.

### Encoder input must be uploaded, not imported (fixed)
The radeonsi encoder does not read a linear external dmabuf as its input
surface. haver uploads NV12 into a driver-owned surface with `vaPutImage`
(the path cros-codecs' own tests use). The zero-copy `GlSplitter` path (phase 5)
will need a driver that accepts dmabuf encode input, or a blit into an
encoder-owned surface.

## Vendored cros-codecs changes

All of the above that live inside cros-codecs are in
`vendor/cros-codecs/README.haver.md`, plus a low-latency decode change
(reorder-window output and access-unit-delimiter handling) and a forced-IDR
change so a requested keyframe is a real random-access point.

## Not yet tested anywhere
- Intel media-driver and Intel Mesa (`iHD`, `i965`) for every item above.
- Native 4:4:4 encode: no AMD profile advertises it (`haver-probe vaapi` shows
  none), so `Dual420` is the only 4:4:4 path on this GPU. Needs an Intel/again
  GPU that advertises HEVC 4:4:4 or AV1 to exercise `Native444`.
- Multiple GPUs / non-renderD128 nodes: `--render-node` exists but is untested.
