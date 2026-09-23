#!/usr/bin/env bash
# Run a command inside a private network namespace whose loopback drops,
# delays and rate-limits packets, to stand in for a lossy link. Both ends
# (server and client) must run inside, so wrap the whole benchmark.
#   scripts/netem.sh DELAY_MS LOSS_PCT [RATE_MBIT] -- command args...
# The delay applies to each direction, so the RTT is twice DELAY_MS; the
# loss applies to every packet in both directions, TCP and UDP alike. The
# loopback MTU is set to NETEM_MTU (default 1280, a tunnel's) so a frame is
# as many packets as on a real link, not one 64 KiB segment.
# Needs only an unprivileged user namespace, not root.
set -euo pipefail
DELAY_MS=$1; LOSS=$2; shift 2
RATE=""
if [ "${1:-}" != "--" ]; then RATE=$1; shift; fi
[ "${1:-}" = "--" ] && shift
if [ -n "${NETEM_INSIDE:-}" ]; then
    ip link set lo up mtu "${NETEM_MTU:-1280}"
    args=(delay "${DELAY_MS}ms" loss "${LOSS}%")
    [ -n "$RATE" ] && args+=(rate "${RATE}mbit")
    tc qdisc add dev lo root netem "${args[@]}"
    exec "$@"
fi
export NETEM_INSIDE=1
exec unshare -rn "$0" "$DELAY_MS" "$LOSS" ${RATE:+"$RATE"} -- "$@"
