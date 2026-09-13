#!/usr/bin/env bash
# Capture a full BeaterCore host<->client session at the syscall level.
#
# Starts two Xvfb displays, launches a host and a client each under the
# udpsniff LD_PRELOAD, drives both through the UI with xdotool, and leaves
# two newline-delimited capture files behind.
#
# Readiness is judged by observable socket state, not by pixels or log lines:
# the host is "up" once it listens on the server port, the join is "done" once
# an established socket to that port exists. Every UI step is retried, because
# the game's animated menu and one-shot dialogs make single-shot clicking
# unreliable.
#
# Usage: capture_session.sh <outdir>
set -uo pipefail

OUT=${1:-/tmp/bccap}
GAME=${BC_GAME:-/mnt/data-drive/SteamLibrary/steamapps/common/BeaterCore}
HERE=$(cd "$(dirname "$0")" && pwd)
SNIFF=$HERE/udpsniff.so
SEED=$HERE/seed

HOST_DISPLAY=:99
CLIENT_DISPLAY=:98
HOST_PORT=${BC_PORT:-6237}

mkdir -p "$OUT"

log() { echo "== $*"; }

start_xvfb() {
    local d=$1
    pgrep -f "Xvfb $d " >/dev/null && return 0
    Xvfb "$d" -screen 0 1280x720x24 -nolisten tcp -ac >/dev/null 2>&1 &
    sleep 1
}

start_game() {
    local display=$1 data=$2 log=$3
    mkdir -p "$data/beaterCore"
    # A profile with pbs.ron + saves/ + mods/local/ boots straight to the main
    # menu. Without mods/local the game opens first-run language/difficulty
    # tutorial dialogs that swallow every scripted click.
    cp -n "$SEED"/*.json5 "$SEED"/pbs.ron "$data/beaterCore/" 2>/dev/null
    cp -rn "$SEED/saves" "$data/beaterCore/" 2>/dev/null
    mkdir -p "$data/beaterCore/mods/local"
    (
        cd "$GAME" || exit 1
        # Capture every UDP datagram; port filtering happens offline. A filter
        # here would drop the far side, whose peer port is the client's
        # ephemeral socket rather than the server port.
        env -u WAYLAND_DISPLAY -u WAYLAND_SOCKET \
            DISPLAY="$display" SDL_VIDEODRIVER=x11 \
            XDG_DATA_HOME="$data" LD_LIBRARY_PATH=. \
            SteamAppId=3711050 \
            BC_SNIFF_LOG="$log" LD_PRELOAD="$SNIFF" \
            ./beaterCore >"$OUT/$(basename "$data").stdout" 2>&1 &
        echo $! >"$OUT/$(basename "$data").pid"
    )
}

wid_on() {
    DISPLAY=$1 xdotool search --name 'beaterCore' 2>/dev/null | head -1
}

click_at() {
    local d=$1 w=$2 x=$3 y=$4
    # SDL ignores synthetic events on an unfocused window; focus first.
    DISPLAY=$d xdotool windowfocus --sync "$w" 2>/dev/null
    DISPLAY=$d xdotool mousemove --window "$w" "$x" "$y"
    sleep 0.4
    DISPLAY=$d xdotool click 1
}

# Wait for the window to exist and for asset loading to finish. The log line
# used earlier ("Steam Input: Available gamepads") only appears when Steam is
# absent, so readiness is judged by the log going quiet instead.
wait_loaded() {
    local display=$1 data=$2 tries=${3:-150}
    local log="$data/beaterCore/beaterCore_log.txt" last=-1 quiet=0
    for _ in $(seq 1 "$tries"); do
        local n
        n=$(wc -l <"$log" 2>/dev/null || echo 0)
        if [ -n "$(wid_on "$display")" ] && [ "$n" -gt 50 ] && [ "$n" = "$last" ]; then
            quiet=$((quiet + 1))
            [ "$quiet" -ge 4 ] && return 0
        else
            quiet=0
        fi
        last=$n
        sleep 1
    done
    return 1
}

listening() { ss -aun 2>/dev/null | grep -q ":$HOST_PORT "; }
joined()    { ss -aun 2>/dev/null | grep -q "$1:$HOST_PORT "; }

log "outdir $OUT"
start_xvfb "$HOST_DISPLAY"
start_xvfb "$CLIENT_DISPLAY"

log "starting host"
start_game "$HOST_DISPLAY" "$OUT/hostdata" "$OUT/cap_host.txt"
wait_loaded "$HOST_DISPLAY" "$OUT/hostdata" || { echo "host never loaded"; exit 1; }

HW=$(wid_on "$HOST_DISPLAY")
log "host window $HW"

# Host Multiplayer -> LAN checkbox -> Create server. Retry the whole sequence;
# a click that lands while the menu is still settling is silently dropped.
for attempt in 1 2 3 4; do
    click_at "$HOST_DISPLAY" "$HW" 806 232; sleep 4
    click_at "$HOST_DISPLAY" "$HW" 260 332; sleep 1
    click_at "$HOST_DISPLAY" "$HW" 480 375
    for _ in $(seq 1 20); do
        listening && break
        sleep 1
    done
    listening && break
    log "host attempt $attempt did not bind; retrying"
done

if ! listening; then
    echo "!! host never listened on $HOST_PORT"
    exit 1
fi
log "host listening on $HOST_PORT"

log "starting client"
start_game "$CLIENT_DISPLAY" "$OUT/clientdata" "$OUT/cap_client.txt"
wait_loaded "$CLIENT_DISPLAY" "$OUT/clientdata" || { echo "client never loaded"; exit 1; }

CW=$(wid_on "$CLIENT_DISPLAY")
log "client window $CW"

# Connect by IP -> type address -> Connect button. Same retry discipline; the
# address field must be cleared or the typed text concatenates.
for attempt in 1 2 3 4; do
    click_at "$CLIENT_DISPLAY" "$CW" 818 272; sleep 3
    click_at "$CLIENT_DISPLAY" "$CW" 480 275; sleep 1
    for _ in $(seq 1 40); do
        DISPLAY=$CLIENT_DISPLAY xdotool key --clearmodifiers BackSpace
    done
    DISPLAY=$CLIENT_DISPLAY xdotool type --clearmodifiers --delay 80 "127.0.0.1:$HOST_PORT"
    sleep 1
    click_at "$CLIENT_DISPLAY" "$CW" 480 317
    for _ in $(seq 1 25); do
        joined 127.0.0.1 && break
        sleep 1
    done
    joined 127.0.0.1 && break
    log "client attempt $attempt did not connect; retrying"
done

if joined 127.0.0.1; then
    log "client joined"
    # Let the session stream a while so steady-state traffic lands in the capture.
    sleep 30
else
    echo "!! client never established a session"
fi

log "sockets"
ss -aunp 2>/dev/null | grep ":$HOST_PORT" || echo "(none)"

log "capture sizes"
wc -l "$OUT"/cap_host.txt "$OUT"/cap_client.txt 2>/dev/null

# Leave the games running for interactive inspection; caller decides when to
# stop them. Print the PIDs so they can be targeted specifically.
log "host pid $(cat "$OUT/hostdata.pid" 2>/dev/null) client pid $(cat "$OUT/clientdata.pid" 2>/dev/null)"
