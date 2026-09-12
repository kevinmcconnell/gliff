# hypr-capture

Captures one Hyprland output with `ext-image-copy-capture-v1` into a small
ring of GBM dmabufs, on a dedicated thread. Frames are captured on demand
(`Capturer::request_frame`) and handed out as `CapturedFrame`; dropping a
frame returns its buffer to the ring. A cursor session reports the cursor
image (ARGB, via wl_shm) and position separately, so the cursor is never baked
into the video.

Hyprland reports full-frame damage, so `damage` is informational only.
