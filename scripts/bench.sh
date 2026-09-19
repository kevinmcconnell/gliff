#!/usr/bin/env bash
# Stream from a gliff-server built in $1 against the nested Hyprland set up
# for benchmarking, and report the stream-bench numbers.
#   scripts/bench.sh <server-bin-dir> <label> [seconds] [-- server args...]
# Expects /tmp/gliff-bench-nest (instance signature) and /tmp/gliff-bench-mon
# (output name) from the bench setup, and content running on that output.
set -uo pipefail
cd "$(dirname "$0")/.."
BIN=$1; LABEL=$2; SECS=${3:-12}; shift 3 2>/dev/null || shift $#
[ "${1:-}" = "--" ] && shift
export XDG_RUNTIME_DIR=${XDG_RUNTIME_DIR:-/run/user/$(id -u)}
NEST=$(cat /tmp/gliff-bench-nest)
MON=$(cat /tmp/gliff-bench-mon)
export WAYLAND_DISPLAY=$(sed -n 2p "$XDG_RUNTIME_DIR/hypr/$NEST/hyprland.lock")
export HYPRLAND_INSTANCE_SIGNATURE=$NEST
PORT=$((9100 + RANDOM % 800))
W=${BENCH_W:-3840}; H=${BENCH_H:-2160}
echo "=== $LABEL ($BIN, ${W}x${H}, $* )"
RUST_LOG=${SERVER_LOG:-info} "$BIN/gliff-server" --listen "127.0.0.1:$PORT" --instance "$NEST" --output "$MON" "$@" >"/tmp/gliff-bench-server-$LABEL.log" 2>&1 &
SP=$!
sleep 2
CPORT=$PORT
if [ -n "${BENCH_MBIT:-}" ]; then
    CPORT=$((PORT + 1))
    python3 scripts/throttle-proxy.py "$CPORT" "$PORT" "$BENCH_MBIT" "${BENCH_DELAY_MS:-0}" "${BENCH_QUEUE_KB:-64}" &
    PP=$!
    sleep 0.5
    echo "  link: $BENCH_MBIT Mbit/s, ${BENCH_DELAY_MS:-0} ms one-way, ${BENCH_QUEUE_KB:-64} KiB queue"
fi
target/release/gliff-probe stream-bench --connect "127.0.0.1:$CPORT" --seconds "$SECS" --width "$W" --height "$H" ${BENCH_ARGS:-} 2>&1 | grep -v "^RESULT"
kill $SP ${PP:-} 2>/dev/null; wait $SP 2>/dev/null
grep -c "adapting" "/tmp/gliff-bench-server-$LABEL.log" | sed "s/^/  bitrate adaptations: /"; true
