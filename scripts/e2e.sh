#!/usr/bin/env bash
# End-to-end test for gliff. Must run inside a Hyprland session (it starts a
# nested Hyprland as the device under test). Requires a GPU with Vulkan Video.
#
# It exercises, and asserts PASS on:
#   1. gliff-probe protocols / vulkan / GPU encode-decode round-trip
#   2. gliff-probe pipeline: capture one frame and run the whole GPU 4:4:4 path
#   3. server --listen --headless  + serve-test client  (Dual420 4:4:4)
#   4. server --listen --headless --low-bandwidth + serve-test (Single420)
#
# Exits non-zero on the first failure.
set -uo pipefail
cd "$(dirname "$0")/.."

fail() { echo "E2E FAIL: $*" >&2; cleanup; exit 1; }
PIDS=()
NEST_SIG=""
cleanup() {
    for p in "${PIDS[@]:-}"; do kill "$p" 2>/dev/null || true; done
    [ -n "$NEST_SIG" ] && pkill -9 -f "Hyprland .*e2e-hypr.conf" 2>/dev/null || true
}
trap cleanup EXIT

command -v Hyprland >/dev/null || fail "Hyprland not found"
[ -n "${WAYLAND_DISPLAY:-}" ] || fail "run inside a Wayland (Hyprland) session"

echo "== building (release) =="
cargo build --release --workspace >/dev/null 2>&1 || fail "build failed"
PROBE=target/release/gliff-probe
SERVER=target/release/gliff-server

echo "== starting nested Hyprland =="
CONF=$(mktemp --suffix=-e2e-hypr.conf)
cat > "$CONF" <<HYPR
monitor=,1280x800,auto,1
misc { disable_hyprland_logo = true; disable_splash_rendering = true }
ecosystem { no_update_news = true; no_donation_nag = true }
HYPR
before=$(ls "$XDG_RUNTIME_DIR/hypr" 2>/dev/null)
WAYLAND_DISPLAY="$WAYLAND_DISPLAY" HYPRLAND_INSTANCE_SIGNATURE= setsid Hyprland -c "$CONF" >/tmp/gliff-e2e-hypr.log 2>&1 &
sleep 6
# The nested instance is the directory that was not there before we started it.
NEST_SIG=$(comm -13 <(echo "$before" | sort) <(ls "$XDG_RUNTIME_DIR/hypr" | sort) | head -1)
[ -n "$NEST_SIG" ] || fail "nested Hyprland did not start"
export HYPRLAND_INSTANCE_SIGNATURE="$NEST_SIG"
export WAYLAND_DISPLAY=$(sed -n 2p "$XDG_RUNTIME_DIR/hypr/$NEST_SIG/hyprland.lock")
echo "   nested sig $NEST_SIG on $WAYLAND_DISPLAY"

damage() { for i in $(seq 1 80); do hyprctl notify 1 200 0 "e2e $i" >/dev/null 2>&1; sleep 0.1; done; }

echo "== 1. probe checks =="
$PROBE --instance "$NEST_SIG" protocols 2>/dev/null | grep -q "^PASS" || fail "protocols"
$PROBE vulkan 2>/dev/null | grep -q "^PASS Vulkan H.264 encode" || fail "vulkan encode"
$PROBE roundtrip 2>/dev/null | grep -q "^PASS min RGB PSNR" || fail "codec round-trip"
echo "   probe checks PASS"

echo "== 2. capture->4:4:4 pipeline =="
hyprctl output create headless e2ecap >/dev/null 2>&1
sleep 1
HN=$(hyprctl monitors -j | python3 -c "import sys,json;print(next((m['name'] for m in json.load(sys.stdin) if 'e2ecap' in m['name'] or m['name']=='e2ecap'),''))")
pipe=$($PROBE --instance "$NEST_SIG" pipeline --output "${HN:-e2ecap}" 2>/dev/null)
echo "$pipe" | grep -q "^PASS" || fail "4:4:4 pipeline"
echo "$pipe" | grep -q "^FAIL" && fail "4:4:4 pipeline ($(echo "$pipe" | grep '^FAIL' | head -1))"
hyprctl output remove "${HN:-e2ecap}" >/dev/null 2>&1
echo "   pipeline PASS"

run_server_test() {
    local port=$1; shift
    local label=$1; shift
    "$SERVER" --listen "127.0.0.1:$port" --headless --instance "$NEST_SIG" "$@" >/tmp/gliff-e2e-server.log 2>&1 &
    local sp=$!; PIDS+=("$sp")
    sleep 2
    damage & local dp=$!; PIDS+=("$dp")
    local out
    out=$(timeout 30 $PROBE serve-test --connect "127.0.0.1:$port" --frames 8 2>&1)
    kill "$dp" 2>/dev/null; kill "$sp" 2>/dev/null; sleep 1
    echo "$out" | grep -q "^PASS" || fail "$label ($(echo "$out" | tail -1))"
    echo "$out" | grep -q "^FAIL" && fail "$label ($(echo "$out" | grep '^FAIL' | head -1))"
    echo "   $label PASS"
}

echo "== 3. server + client, Dual420 =="
run_server_test 9040 "Dual420 stream"

echo "== 4. server + client, Single420 (--low-bandwidth) =="
run_server_test 9041 "Single420 stream" --low-bandwidth

echo "== 5. clipboard both directions =="
command -v wl-copy >/dev/null && command -v wl-paste >/dev/null || fail "wl-clipboard not installed"
wl-copy "e2e-clip-in" 2>/dev/null
"$SERVER" --listen 127.0.0.1:9042 --headless --instance "$NEST_SIG" >/tmp/gliff-e2e-server.log 2>&1 &
csp=$!; PIDS+=("$csp"); sleep 2
damage & cdp=$!; PIDS+=("$cdp")
clipf=$(mktemp)
GLIFF_SEND_CLIP="e2e-clip-out" timeout 20 $PROBE serve-test --connect 127.0.0.1:9042 --frames 200 >"$clipf" 2>&1 &
clipc=$!; PIDS+=("$clipc")
sleep 5
pasted=$(wl-paste -n 2>/dev/null)
kill "$clipc" "$cdp" "$csp" 2>/dev/null
grep -q "CLIP-RECV: e2e-clip-in" "$clipf" || fail "compositor->client clipboard (got: $(grep CLIP-RECV "$clipf"))"
[ "$pasted" = "e2e-clip-out" ] || fail "client->compositor clipboard (got: $pasted)"
rm -f "$clipf"
echo "   clipboard both directions PASS"

echo "== 6. mirrored output resize =="
hyprctl output create headless e2emirror >/dev/null 2>&1
sleep 1
mirror=$(hyprctl monitors -j | python3 -c "import sys,json; print(next(m['name'] for m in json.load(sys.stdin) if 'e2emirror' in m['name']))")
hyprctl keyword monitor "$mirror,1280x800@60,auto,1" >/dev/null 2>&1
sleep 1
"$SERVER" --listen 127.0.0.1:9043 --output "$mirror" --instance "$NEST_SIG" >/tmp/gliff-e2e-mirror-server.log 2>&1 &
msp=$!; PIDS+=("$msp"); sleep 1
damage & mdp=$!; PIDS+=("$mdp")
mirror_log=$(mktemp)
timeout 30 "$PROBE" serve-test --connect 127.0.0.1:9043 --frames 30 >"$mirror_log" 2>&1 &
mcp=$!; PIDS+=("$mcp")
wait_for_mirror() {
    local pattern=$1
    for ((attempt = 0; attempt < 100; attempt++)); do
        grep -q "$pattern" "$mirror_log" && return 0
        kill -0 "$mcp" 2>/dev/null || break
        sleep 0.1
    done
    fail "mirror resize: missing '$pattern' (log: $mirror_log)"
}
wait_for_mirror 'first decoded frame ok'
hyprctl keyword monitor "$mirror,640x480@60,auto,1" >/dev/null 2>&1
wait_for_mirror 'reconfig to 640x480'
hyprctl keyword monitor "$mirror,1024x768@60,auto,1" >/dev/null 2>&1
wait_for_mirror 'reconfig to 800x600'
wait "$mcp" || fail "mirror resize: probe failed (log: $mirror_log)"
grep -q '^PASS decoded 30 frames' "$mirror_log" || fail "mirror resize: frames stopped (log: $mirror_log)"
grep -q '^FAIL' "$mirror_log" && fail "mirror resize: failed assertion (log: $mirror_log)"
kill "$mdp" "$msp" 2>/dev/null
hyprctl output remove "$mirror" >/dev/null 2>&1
echo "   mirrored output resize PASS"

echo "E2E PASS: all checks passed"
