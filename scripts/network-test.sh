#!/usr/bin/env bash
set -euo pipefail

usage() {
    cat <<'EOF'
Usage: scripts/network-test.sh <slow|lossy|bad>

  slow   5 Mbit/s, 100 ms added RTT with jitter, no injected loss
  lossy  8% random packet loss, 250 ms added RTT
  bad    5 Mbit/s, 250 ms added RTT with jitter, 8% random loss

Builds release binaries and opens nested Hyprland plus a gliff client.
Requires sudo, iproute2, ethtool, util-linux, dbus, and a terminal
(foot, kitty, or alacritty). Run from a Wayland session as your normal user.
Close gliff or press Ctrl-C to stop. Logs are retained in /tmp/gliff-network.*.
Uses direct TCP, not SSH. Test applications have no external network access.
EOF
}

fail() { printf 'network-test: %s\n' "$*" >&2; exit 1; }

if [[ ${1:-} == --help || ${1:-} == -h ]]; then
    usage
    exit 0
fi

session=false
if [[ ${1:-} == --session && $# == 3 ]]; then
    session=true
    log_dir=$3
    shift
else
    [[ $# == 1 ]] || { usage >&2; exit 2; }
fi
quality=$1
case "$quality" in
    slow) impairment=(delay 50ms 10ms distribution normal rate 5mbit) ;;
    lossy) impairment=(delay 125ms loss random 8%) ;;
    bad) impairment=(delay 125ms 10ms distribution normal loss random 8% rate 5mbit) ;;
    *) usage >&2; exit 2 ;;
esac

[[ $EUID != 0 ]] || fail 'Run as your normal user; the script requests sudo for network setup.'
[[ -n ${WAYLAND_DISPLAY:-} && -n ${XDG_RUNTIME_DIR:-} ]] || fail 'Run inside a Wayland session.'
cd "$(dirname "$(realpath "$0")")/.."
repo=$PWD

if ! "$session"; then
    for command in cargo Hyprland hyprctl sudo ip tc ethtool unshare setpriv setsid dbus-run-session ss; do
        command -v "$command" >/dev/null || fail "Required command not found: $command"
    done
    terminal=''
    for candidate in foot kitty alacritty; do
        if command -v "$candidate" >/dev/null; then
            terminal=$candidate
            break
        fi
    done
    [[ -n $terminal ]] || fail 'Install foot, kitty, or alacritty for the nested desktop.'

    cargo build --release -p gliff -p gliff-server
    sudo -v
    log_dir=$(mktemp -d /tmp/gliff-network.XXXXXX)
    environment=("GLIFF_TEST_TERMINAL=$terminal")
    for name in HOME USER LOGNAME PATH XDG_RUNTIME_DIR WAYLAND_DISPLAY DISPLAY \
        XDG_CONFIG_HOME XDG_DATA_HOME XDG_CACHE_HOME XDG_DATA_DIRS XDG_SESSION_TYPE \
        LANG LC_ALL ANV_DEBUG VK_ICD_FILENAMES VK_DRIVER_FILES DRI_PRIME \
        MESA_VK_DEVICE_SELECT RUST_LOG; do
        if [[ -v $name ]]; then
            environment+=("$name=${!name}")
        fi
    done
    printf 'Profile: %s\nLogs: %s\n' "$quality" "$log_dir"
    exec sudo unshare --net -- bash -euc '
        uid=$1; gid=$2; script=$3; quality=$4; logs=$5
        shift 5
        netem=()
        while [[ $1 != -- ]]; do netem+=("$1"); shift; done
        shift
        ip link set lo mtu 1500 up
        ethtool -K lo tso off gso off gro off
        tc qdisc add dev lo root netem limit 1000 "${netem[@]}"
        tc -s qdisc show dev lo > "$logs/network.log"
        exec setpriv --reuid "$uid" --regid "$gid" --init-groups \
            env -i "$@" bash "$script" --session "$quality" "$logs"
    ' bash "$(id -u)" "$(id -g)" "$repo/scripts/network-test.sh" \
        "$quality" "$log_dir" "${impairment[@]}" -- "${environment[@]}"
fi

pids=()
cleanup() {
    local status=$?
    trap - EXIT INT TERM
    tc -s qdisc show dev lo > "$log_dir/network-final.log" 2>/dev/null || true
    for pid in "${pids[@]}"; do
        kill -TERM -- "-$pid" 2>/dev/null || true
    done
    sleep 0.5
    for pid in "${pids[@]}"; do
        kill -KILL -- "-$pid" 2>/dev/null || true
        wait "$pid" 2>/dev/null || true
    done
    printf '\nTest stopped. Logs: %s\n' "$log_dir"
    exit "$status"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

cat > "$log_dir/hyprland.conf" <<EOF
monitor=,1280x800,auto,1
misc {
    disable_hyprland_logo = true
    disable_splash_rendering = true
}
ecosystem {
    no_update_news = true
    no_donation_nag = true
}
bind = SUPER, Return, exec, $GLIFF_TEST_TERMINAL
bind = SUPER, Q, killactive,
EOF

setsid env -u HYPRLAND_INSTANCE_SIGNATURE Hyprland -c "$log_dir/hyprland.conf" \
    > "$log_dir/hyprland.log" 2>&1 &
nested_pid=$!
pids+=("$nested_pid")
nested_signature=''
ready=false
for ((attempt = 0; attempt < 150; attempt++)); do
    kill -0 "$nested_pid" 2>/dev/null || fail "Nested Hyprland exited; see $log_dir/hyprland.log"
    for lock in "$XDG_RUNTIME_DIR"/hypr/*/hyprland.lock; do
        [[ -f $lock ]] || continue
        mapfile -t fields < "$lock"
        if [[ ${fields[0]:-} == "$nested_pid" && -n ${fields[1]:-} ]]; then
            nested_signature=${lock%/hyprland.lock}
            nested_signature=${nested_signature##*/}
            nested_display=${fields[1]}
            break
        fi
    done
    if [[ -n $nested_signature ]] && hyprctl -i "$nested_signature" monitors >/dev/null 2>&1; then
        ready=true
        break
    fi
    sleep 0.1
done
"$ready" || fail "Nested Hyprland did not become ready; see $log_dir/hyprland.log"
hyprctl -i "$nested_signature" configerrors > "$log_dir/config-errors.log"
[[ -z $(cat "$log_dir/config-errors.log") ]] || fail "Nested configuration errors; see $log_dir/config-errors.log"

setsid env WAYLAND_DISPLAY="$nested_display" HYPRLAND_INSTANCE_SIGNATURE="$nested_signature" \
    "$GLIFF_TEST_TERMINAL" > "$log_dir/terminal.log" 2>&1 &
pids+=("$!")

setsid env RUST_LOG="${RUST_LOG:-info,gliff_server=debug}" \
    "$repo/target/release/gliff-server" --listen 127.0.0.1:9040 \
    --instance "$nested_signature" --output auto > "$log_dir/server.log" 2>&1 &
server_pid=$!
pids+=("$server_pid")
ready=false
for ((attempt = 0; attempt < 100; attempt++)); do
    kill -0 "$server_pid" 2>/dev/null || fail "Server exited; see $log_dir/server.log"
    if [[ -n $(ss -H -ltn 'sport = :9040') ]]; then
        ready=true
        break
    fi
    sleep 0.1
done
"$ready" || fail 'Server did not start listening.'

printf 'Opening gliff (%s). Super+Return opens a nested terminal; Shift+Esc releases input.\n' "$quality"
setsid env RUST_LOG="${RUST_LOG:-info}" dbus-run-session -- \
    "$repo/target/release/gliff" --connect 127.0.0.1:9040 > "$log_dir/client.log" 2>&1 &
client_pid=$!
pids+=("$client_pid")
wait -n "$client_pid" "$server_pid" "$nested_pid"
