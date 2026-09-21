#!/usr/bin/env bash
# Compare the TCP and UDP video paths on a lossy link. Starts a nested
# Hyprland (run inside a Hyprland session), serves a headless output from
# it, keeps its screen changing, and runs stream-bench over a private
# loopback shaped by netem (scripts/netem.sh; no root needed):
#   scripts/lossy-bench.sh [DELAY_MS] [LOSS_PCT] [SECONDS] [-- server args...]
# Defaults: 20 ms one-way, 2% loss, 10 s. Prints one result line per path.
set -uo pipefail
cd "$(dirname "$0")/.."
DELAY=${1:-20}; LOSS=${2:-2}; SECS=${3:-10}; shift 3 2>/dev/null || shift $#
[ "${1:-}" = "--" ] && shift
PROBE=target/release/gliff-probe
SERVER=target/release/gliff-server
[ -x "$PROBE" ] && [ -x "$SERVER" ] || { echo "build first: cargo build --release" >&2; exit 1; }
[ -n "${WAYLAND_DISPLAY:-}" ] || { echo "run inside a Wayland (Hyprland) session" >&2; exit 1; }
export XDG_RUNTIME_DIR=${XDG_RUNTIME_DIR:-/run/user/$(id -u)}

PIDS=()
NEST=""
cleanup() {
    for p in "${PIDS[@]:-}"; do kill "$p" 2>/dev/null || true; done
    [ -n "$NEST" ] && pkill -9 -f "Hyprland .*lossy-hypr.conf" 2>/dev/null || true
}
trap cleanup EXIT

CONF=$(mktemp --suffix=-lossy-hypr.conf)
cat > "$CONF" <<HYPR
monitor=,1280x800,auto,1
misc { disable_hyprland_logo = true; disable_splash_rendering = true }
ecosystem { no_update_news = true; no_donation_nag = true }
HYPR
before=$(ls "$XDG_RUNTIME_DIR/hypr" 2>/dev/null)
WAYLAND_DISPLAY="$WAYLAND_DISPLAY" HYPRLAND_INSTANCE_SIGNATURE= setsid Hyprland -c "$CONF" >/tmp/gliff-lossy-hypr.log 2>&1 &
sleep 6
NEST=$(comm -13 <(echo "$before" | sort) <(ls "$XDG_RUNTIME_DIR/hypr" | sort) | head -1)
[ -n "$NEST" ] || { echo "nested Hyprland did not start" >&2; exit 1; }
export HYPRLAND_INSTANCE_SIGNATURE="$NEST"
export WAYLAND_DISPLAY=$(sed -n 2p "$XDG_RUNTIME_DIR/hypr/$NEST/hyprland.lock")
echo "nested Hyprland $NEST on $WAYLAND_DISPLAY; link ${DELAY} ms one-way, ${LOSS}% loss"

# Keep the screen changing so frames flow.
(while true; do hyprctl notify 1 300 0 "lossy $RANDOM" >/dev/null 2>&1; sleep 0.05; done) &
PIDS+=($!)

run_one() {
    local label=$1; shift
    local port=$((9200 + RANDOM % 500))
    local out
    out=$(scripts/netem.sh "$DELAY" "$LOSS" -- bash -c "
        RUST_LOG=${SERVER_LOG:-info} $SERVER --listen 127.0.0.1:$port --headless --instance $NEST $* >/tmp/gliff-lossy-server-$label.log 2>&1 &
        sp=\$!
        sleep 2
        $PROBE stream-bench --connect 127.0.0.1:$port --seconds $SECS $LOSSY_BENCH_ARGS 2>&1
        kill \$sp 2>/dev/null; wait \$sp 2>/dev/null")
    echo "=== $label"
    echo "$out" | grep -v '^RESULT'
    echo "$out" | grep '^RESULT' | sed "s/^RESULT/RESULT $label/"
}

[[ "${LOSSY_PATHS:-tcp udp}" == *tcp* ]] && LOSSY_BENCH_ARGS="--tcp" run_one tcp "$@"
[[ "${LOSSY_PATHS:-tcp udp}" == *udp* ]] && LOSSY_BENCH_ARGS="" run_one udp "$@"
