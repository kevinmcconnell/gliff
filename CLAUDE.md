# gliff

Run `scripts/check.sh` before every push or PR. It runs the CI checks
(rustfmt, clippy with warnings as errors, the unit tests); a clippy warning
is a CI failure. `scripts/e2e.sh` covers the hardware paths and needs a
Hyprland session; the GPU cases need Vulkan compute and a VA-API H.264
codec. Run it before merging changes to the video pipeline. See
`docs/architecture.md` for the design and the testing tools.
