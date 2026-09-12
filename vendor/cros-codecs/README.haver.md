Vendored copy of cros-codecs 0.0.6 (crates.io) with low-latency patches:

1. `codec/h264/parser.rs`: `SpsBuilder::bitstream_restriction()` so the VUI can
   declare `max_num_reorder_frames`.
2. `encoder/stateless/h264/predictor.rs`: the encoder's SPS declares
   `max_num_reorder_frames = 0`, `max_dec_frame_buffering = 1`.
3. `codec/h264/dpb.rs` + `decoder/stateless/h264.rs`: after a picture is
   stored, pictures are output while more than `max_num_reorder_frames` wait
   for output. The unpatched DPB only outputs when it is full, which adds
   several frames of latency to a low-delay stream.
4. `decoder/stateless/h264.rs`: an access-unit delimiter NAL finishes the
   picture in progress, so the decoder does not hold the last frame until the
   next one arrives.
5. `backend/vaapi/decoder.rs`: the placeholder VA context is 320x240 instead
   of 16x16, which radeonsi rejects (RESOLUTION_NOT_SUPPORTED).
6. `encoder/stateless/predictor.rs`: a forced keyframe is now a true IDR
   (resets the sequence), not just an intra frame. Random access needs it.
7. `encoder/stateless/h264.rs`: `new_h264` made `pub` so the encoder can be
   built for a `Surface`/`PooledVaSurface` input handle (image-upload path)
   instead of only the dmabuf `VideoFrame` path, which AMD/radeonsi will not
   read as encode input.
