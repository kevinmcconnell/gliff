# gliff

[![CI](https://github.com/kevinmcconnell/gliff/actions/workflows/ci.yml/badge.svg)](https://github.com/kevinmcconnell/gliff/actions/workflows/ci.yml)

gliff is a remote desktop for Hyprland. It shows the Hyprland session of
another machine in a window on yours, and all it needs between the two is
ssh: no open ports, no accounts, no relay.

Text stays sharp. Most remote desktops send video with halved colour
resolution, which blurs coloured text; gliff sends full-resolution colour
(4:4:4) through the H.264 hardware every GPU has. Video runs on the GPU at
both ends, and falls back to the CPU on machines where it cannot.

![A gliff window showing a terminal on a remote machine](.github/assets/screenshot.png)

## What you get

- Mirror any screen of the remote machine, or get a private screen that
  exists only for your session and follows the size of your window.
- Your keyboard layout, not the remote's: keys map the same on both ends.
- A shared clipboard in both directions: text, images, and copied files.
- Quality that adapts to the link, giving up frame rate before sharpness.
- Reconnection when the link drops.
- A window that follows your Omarchy theme.

## Requirements

- Hyprland on both machines, with a session running on the remote one.
- ssh access from your machine to the remote one.
- For GPU video: an AMD or Intel GPU with its Vulkan and VA-API drivers
  (`vulkan-radeon` and `libva-mesa-driver`, or `vulkan-intel` and
  `intel-media-driver`, on Arch). Without them, and on NVIDIA, gliff uses the
  CPU and needs nothing extra.

## Install

Install gliff on both machines: your machine runs `gliff`, the remote one
runs `gliff-server`.

On Arch, build the package from a checkout:

```
makepkg -si
```

Or take the prebuilt x86_64 binaries from the [releases
page](https://github.com/kevinmcconnell/gliff/releases) and put `gliff`,
`gliff-server` and `gliff-probe` on your `PATH`. The `latest` release follows
`main`; `v*` tags are permanent releases. For a launcher entry, copy the
`.desktop` file to `~/.local/share/applications/` and the `.svg` icon to
`~/.local/share/icons/hicolor/scalable/apps/`.

To build from source, see [docs/development.md](docs/development.md).

## Use

```
gliff user@host
```

This mirrors the screen that has focus on the remote machine. To pick a
screen, or to get a private one:

```
gliff --output DP-1 user@host   # mirror a named remote screen
gliff --headless user@host      # a private remote screen, sized and
                                # scaled to this window
```

A mirrored screen keeps its own size and is fitted into the window. A private
screen follows the window as you resize it, and is removed when you
disconnect.

The address bar in the title bar holds the machine. Type another one and
press Enter to switch to it, or pick one of the six you used last from the
drop-down. A bare `gliff` opens the window with the address bar ready.

**Keyboard.** Click the picture to send everything to the remote machine,
window-manager shortcuts included. Press `Shift+Esc` to get your own
shortcuts back. `--release-hotkey` changes that key (`ctrl+alt+q`,
`double-escape`, or `none`).

**Clipboard.** Copy on one machine and paste on the other. Nothing is sent
until you paste. A paste that takes more than a second shows its progress
with a cancel button. Pasted files are kept in `~/.cache/gliff/clipboard`.

**Slow links.** gliff measures the link and lowers the frame rate, then the
colour detail, then the resolution, and raises them again when there is
room. Two flags change the video, and you pass them to the server through
`--server-bin`: `--low-bandwidth` sends 4:2:0 colour from the start, and
`--full-chroma` keeps 4:4:4 on a server that encodes on the CPU.

```
gliff --server-bin 'gliff-server --low-bandwidth' user@host
```

## If something does not work

- Run `gliff-probe all` on each machine. It checks the compositor, the GPU
  drivers and both video paths, and prints PASS or FAIL for each.
- If ssh cannot find `gliff-server`, give its path:
  `gliff --server-bin /path/to/gliff-server user@host`.
- The server needs the remote user's running Hyprland session. If the ssh
  session does not have `XDG_RUNTIME_DIR` or `WAYLAND_DISPLAY`, set them
  through `--server-bin 'env XDG_RUNTIME_DIR=/run/user/1000 gliff-server'`.
- To rule out a driver problem, force the CPU on your end with
  `gliff --video cpu user@host`.
- Screens wider than 4096 pixels are streamed at a reduced size and scaled
  back up.

## More

- [docs/architecture.md](docs/architecture.md): how it works, the design
  choices, measurements, and what is not built yet.
- [docs/hardware-quirks.md](docs/hardware-quirks.md): what each GPU driver
  and Hyprland do, and what is tested where.
- [docs/development.md](docs/development.md): building, running on one
  machine, and the tests.

gliff is released under the MIT license.
