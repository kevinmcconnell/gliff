Vendored copy of cros-libva 0.0.12 (crates.io) with one fix from upstream main:
`VAEncPictureParameterBufferVP9` gained `seg_id_block_size`/`va_reserved8` in
libva 1.23, and the published crate does not compile against it. Only
`build.rs` (new cfg) and `src/buffer/vp9.rs` (initializer) differ.
Version stays 0.0.12 so that cros-codecs' `^0.0.12` requirement is satisfied.
