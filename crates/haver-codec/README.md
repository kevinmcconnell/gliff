# haver-codec

VA-API H.264 encode/decode built on the vendored `cros-codecs` (see
`vendor/cros-codecs/README.haver.md` for the low-latency patches).

- `frame`: `Nv12Frame`, a pooled linear NV12 dmabuf allocated with GBM. It
  implements cros-codecs' `VideoFrame`, so the same buffer type is the encoder
  input and the decoder output, and it exposes the fd/offsets/strides that EGL
  needs. Frames return to their `FramePool` on drop.
- `h264`: `H264Encoder` (CQP, low delay, forced keyframes, SPS/PPS capture,
  an AUD appended to every access unit) and `H264Decoder` (one access unit in,
  ready frames out, pool re-created on format change).
- `vaapi`: display opening and a capability probe (profiles, entrypoints).
- `annexb`: NAL unit iteration and parameter-set extraction.

No `unsafe` in this crate.
