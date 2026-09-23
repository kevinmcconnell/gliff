#!/usr/bin/env bash
# Benchmark a gliff-server and stream-bench pair on a shaped link, inside a
# nested Hyprland (run from a Hyprland session):
#   scripts/bench.sh <label> [seconds] [-- server args...]
#
# Link shaping, by preset or by environment:
#   BENCH_PRESET=lan         no shaping
#   BENCH_PRESET=dsl         20 Mbit/s, 10 ms one-way (TCP proxy)
#   BENCH_PRESET=satellite   2.5 Mbit/s, 550 ms one-way, 10% loss (netem)
#   BENCH_MBIT, BENCH_DELAY_MS, BENCH_QUEUE_KB    TCP proxy (rate, delay)
#   BENCH_LOSS, BENCH_NETEM_MS [, BENCH_MBIT]     netem on a private
#       loopback (scripts/netem.sh; no root needed); loss and delay apply
#       in both directions, so acks are shaped too
# Other knobs: BENCH_W/BENCH_H (default 2560x1440), BENCH_STATIC=1 (no
# damage loop, tests the still-screen path), BENCH_ARGS (extra probe args),
# SERVER_LOG (RUST_LOG for the server), BENCH_MIRROR=1 (mirror a settled
# pre-created headless output), BENCH_SETTLE_SECS, BENCH_MOTION_AFTER,
# BENCH_MOTION_INTERVAL,
# BENCH_MOTION_SOURCE=video (fullscreen moving test pattern).
set -uo pipefail
cd "$(dirname "$0")/.."
case "${BENCH_PRESET:-}" in
    lan) ;;
    dsl) BENCH_MBIT=${BENCH_MBIT:-20}; BENCH_DELAY_MS=${BENCH_DELAY_MS:-10} ;;
    satellite)
        BENCH_MBIT=${BENCH_MBIT:-2.5}
        BENCH_NETEM_MS=${BENCH_NETEM_MS:-550}
        BENCH_LOSS=${BENCH_LOSS:-10}
        ;;
    "") ;;
    *) echo "unknown BENCH_PRESET '${BENCH_PRESET}'" >&2; exit 1 ;;
esac
LABEL=${1:?usage: bench.sh <label> [seconds] [-- server args...]}
SECS=${2:-20}
shift 2 2>/dev/null || shift $#
[ "${1:-}" = "--" ] && shift
PROBE=target/release/gliff-probe
SERVER=target/release/gliff-server
[ -x "$PROBE" ] && [ -x "$SERVER" ] || { echo "build first: cargo build --release" >&2; exit 1; }
[ -n "${WAYLAND_DISPLAY:-}" ] || { echo "run inside a Wayland (Hyprland) session" >&2; exit 1; }
[ "${BENCH_MOTION_SOURCE:-notify}" != video ] || [ -n "${BENCH_MIRROR:-}" ] || { echo "video motion needs BENCH_MIRROR=1" >&2; exit 1; }
export XDG_RUNTIME_DIR=${XDG_RUNTIME_DIR:-/run/user/$(id -u)}
W=${BENCH_W:-2560}; H=${BENCH_H:-1440}

PIDS=()
NEST=""
CONF=""
# Killed with SIGKILL, the nested Hyprland leaves its instance directory
# and sockets behind, so remove them along with its config file.
cleanup() {
    for p in "${PIDS[@]:-}"; do kill "$p" 2>/dev/null || true; done
    if [ -n "$NEST" ]; then
        pkill -9 -f "Hyprland .*bench-hypr.conf" 2>/dev/null || true
        sleep 0.5
        rm -rf "$XDG_RUNTIME_DIR/hypr/$NEST"
    fi
    [ -n "$CONF" ] && rm -f "$CONF"
}
trap cleanup EXIT

CONF=$(mktemp --suffix=-bench-hypr.conf)
cat > "$CONF" <<HYPR
monitor=,1280x800,auto,1
misc { disable_hyprland_logo = true; disable_splash_rendering = true }
ecosystem { no_update_news = true; no_donation_nag = true }
HYPR
before=$(ls "$XDG_RUNTIME_DIR/hypr" 2>/dev/null)
WAYLAND_DISPLAY="$WAYLAND_DISPLAY" HYPRLAND_INSTANCE_SIGNATURE= setsid Hyprland -c "$CONF" >/tmp/gliff-bench-hypr.log 2>&1 &
sleep 6
NEST=$(comm -13 <(echo "$before" | sort) <(ls "$XDG_RUNTIME_DIR/hypr" | sort) | head -1)
[ -n "$NEST" ] || { echo "nested Hyprland did not start" >&2; exit 1; }
export HYPRLAND_INSTANCE_SIGNATURE="$NEST"
export WAYLAND_DISPLAY=$(sed -n 2p "$XDG_RUNTIME_DIR/hypr/$NEST/hyprland.lock")
BENCH_OUTPUT=""
if [ -n "${BENCH_MIRROR:-}" ]; then
    BENCH_OUTPUT=bench-test
    hyprctl output create headless "$BENCH_OUTPUT" >/dev/null
    hyprctl keyword monitor "$BENCH_OUTPUT,${W}x${H}@60,auto,1" >/dev/null
    for _ in {1..20}; do
        hyprctl -j monitors all | jq -e --arg name "$BENCH_OUTPUT" --argjson w "$W" --argjson h "$H" \
            '.[] | select(.name == $name and .width == $w and .height == $h)' >/dev/null && break
        sleep 0.1
    done
    hyprctl -j monitors all | jq -e --arg name "$BENCH_OUTPUT" --argjson w "$W" --argjson h "$H" \
        '.[] | select(.name == $name and .width == $w and .height == $h)' >/dev/null || { echo "settled output did not reach ${W}x${H}" >&2; exit 1; }
fi
sleep "${BENCH_SETTLE_SECS:-0}"

PORT=$((9100 + RANDOM % 800))
SLOG=/tmp/gliff-bench-server-$LABEL.log
echo "=== $LABEL (${W}x${H}, ${SECS}s, static=${BENCH_STATIC:-0}, server args: $*)"

run_pair() {
    local server_args=(--listen "127.0.0.1:$PORT" --instance "$NEST")
    if [ -n "$BENCH_OUTPUT" ]; then server_args+=(--output "$BENCH_OUTPUT"); else server_args+=(--headless); fi
    RUST_LOG=${SERVER_LOG:-info} "$SERVER" "${server_args[@]}" "$@" >"$SLOG" 2>&1 &
    local sp=$!
    sleep 2
    local cport=$PORT
    if [ -z "${NETEM_ACTIVE:-}" ] && [ -n "${BENCH_MBIT:-}" ]; then
        cport=$((PORT + 1))
        python3 scripts/throttle-proxy.py "$cport" "$PORT" "$BENCH_MBIT" "${BENCH_DELAY_MS:-0}" "${BENCH_QUEUE_KB:-64}" &
        PIDS+=($!)
        sleep 0.5
        echo "  link: $BENCH_MBIT Mbit/s, ${BENCH_DELAY_MS:-0} ms one-way, ${BENCH_QUEUE_KB:-64} KiB queue (proxy)"
    fi
    local probe_w=$W probe_h=$H
    if [ -n "${BENCH_MIRROR:-}" ]; then probe_w=0; probe_h=0; fi
    echo "  probe started: $(date --iso-8601=ns)"
    if [ -z "${BENCH_STATIC:-}" ]; then
        (
            sleep "${BENCH_MOTION_AFTER:-0}"
            echo "  motion started: $(date --iso-8601=ns)"
            if [ "${BENCH_MOTION_SOURCE:-notify}" = video ]; then
                hyprctl dispatch focusmonitor "$BENCH_OUTPUT" >/dev/null
                exec env SDL_VIDEODRIVER=wayland ffplay -loglevel error -nostats -f lavfi \
                    -i "testsrc2=size=${W}x${H}:rate=60" -fs -window_title bench-motion \
                    >"/tmp/gliff-bench-motion-$LABEL.log" 2>&1
            fi
            while true; do
                hyprctl notify 1 300 0 "bench $RANDOM" >/dev/null 2>&1
                sleep "${BENCH_MOTION_INTERVAL:-0.05}"
            done
        ) &
        PIDS+=($!)
    fi
    "$PROBE" stream-bench --connect "127.0.0.1:$cport" --seconds "$SECS" \
        --width "$probe_w" --height "$probe_h" ${BENCH_ARGS:-} 2>&1
    kill $sp 2>/dev/null; wait $sp 2>/dev/null
}

if [ -n "${BENCH_LOSS:-}${BENCH_NETEM_MS:-}" ]; then
    echo "  link: netem ${BENCH_NETEM_MS:-0} ms one-way, ${BENCH_LOSS:-0}% loss${BENCH_MBIT:+, $BENCH_MBIT Mbit/s} (both directions)"
    export -f run_pair
    export NEST PORT SLOG PROBE SERVER SECS W H BENCH_OUTPUT NETEM_ACTIVE=1
    export SERVER_LOG BENCH_MBIT BENCH_ARGS BENCH_DELAY_MS BENCH_QUEUE_KB BENCH_MIRROR BENCH_MOTION_AFTER BENCH_MOTION_INTERVAL BENCH_MOTION_SOURCE BENCH_STATIC
    scripts/netem.sh "${BENCH_NETEM_MS:-0}" "${BENCH_LOSS:-0}" ${BENCH_MBIT:+"$BENCH_MBIT"} -- \
        bash -c 'run_pair "$@"' bench "$@"
else
    run_pair "$@"
fi
echo "  bitrate adaptations: $(grep -c 'adapting' "$SLOG" || true)"
echo "  server log: $SLOG"
