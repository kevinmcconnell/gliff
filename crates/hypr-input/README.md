# hypr-input

Injects keyboard and pointer input into Hyprland from a dedicated thread.

- Keyboard: `zwp_virtual_keyboard_v1`. The client's keymap is uploaded
  verbatim; only evdev keycodes are sent. An `xkb::State` built from the same
  keymap derives the `modifiers` request, so modifier state is never applied
  twice. All keys and buttons are released on `ReleaseAll` and on drop.
- Pointer: `zwlr_virtual_pointer_v1` bound to the captured output, absolute
  motion in that output's logical coordinates, buttons, and axes.
