# hypr-ipc

Minimal client for Hyprland's IPC socket. Discovers the running instance from
`$XDG_RUNTIME_DIR/hypr` (newest wins unless `HYPRLAND_INSTANCE_SIGNATURE` or an
explicit signature is given), reads the Wayland socket name from
`hyprland.lock`, and wraps the few commands gliff needs: monitors, options,
headless output create/remove/resize.
