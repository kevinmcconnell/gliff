# hypr-wl

Shared Wayland helpers: connect to a specific socket (from `WAYLAND_DISPLAY`
or the discovered Hyprland instance), list globals, and keep `wl_output` and
`wl_seat` state. The `Outputs`/`Seat` structs are plain state holders; the
owning crate embeds them in its `Dispatch` state and forwards events.
