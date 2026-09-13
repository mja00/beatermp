#!/usr/bin/env bash
# Launch one real BeaterCore client under Xvfb and make it join a server.
#
# This is the verification harness for beatermp: it drives the shipped game
# through "Connect by IP" against whatever is listening on the target port and
# leaves a syscall-level capture of everything the client sent and received.
# Readiness is judged by socket state, not pixels: the join is "done" once an
# established UDP socket to the target exists.
#
# Usage: join_client.sh <outdir> [host:port]
set -uo pipefail

OUT=${1:-/tmp/bcjoin}
TARGET=${2:-127.0.0.1:6237}
GAME=${BC_GAME:-/mnt/data-drive/SteamLibrary/steamapps/common/BeaterCore}
HERE=$(cd "$(dirname "$0")" && pwd)
SNIFF=$HERE/udpsniff.so
SEED=$HERE/seed
DISPLAY_NO=${BC_DISPLAY:-:98}
PORT=${TARGET##*:}

mkdir -p "$OUT"
log() { echo "== $*"; }

pgrep -f "Xvfb $DISPLAY_NO " >/dev/null || {
    Xvfb "$DISPLAY_NO" -screen 0 1280x720x24 -nolisten tcp -ac >/dev/null 2>&1 &
    sleep 1
}

DATA=$OUT/clientdata
mkdir -p "$DATA/beaterCore/mods/local"
# pbs.ron + saves/ + mods/local/ skip the first-run tutorial dialogs that
# otherwise swallow every scripted click.
cp -n "$SEED"/*.json5 "$SEED"/pbs.ron "$DATA/beaterCore/" 2>/dev/null
cp -rn "$SEED/saves" "$DATA/beaterCore/" 2>/dev/null

(
    cd "$GAME" || exit 1
    env -u WAYLAND_DISPLAY -u WAYLAND_SOCKET \
        DISPLAY="$DISPLAY_NO" SDL_VIDEODRIVER=x11 \
        XDG_DATA_HOME="$DATA" LD_LIBRARY_PATH=. SteamAppId=3711050 \
        BC_SNIFF_LOG="$OUT/cap_client.txt" LD_PRELOAD="$SNIFF" \
        ./beaterCore >"$OUT/client.stdout" 2>&1 &
    echo $! >"$OUT/client.pid"
)
log "client pid $(cat "$OUT/client.pid")"

wid() { DISPLAY=$DISPLAY_NO xdotool search --name 'beaterCore' 2>/dev/null | head -1; }
click_at() {
    # SDL ignores synthetic events on an unfocused window; focus first.
    DISPLAY=$DISPLAY_NO xdotool windowfocus --sync "$1" 2>/dev/null
    DISPLAY=$DISPLAY_NO xdotool mousemove --window "$1" "$2" "$3"
    sleep 0.4
    DISPLAY=$DISPLAY_NO xdotool click 1
}
joined() { ss -aun 2>/dev/null | grep -q ":$PORT "; }

# Ready once the window exists and the asset-loading log has gone quiet; the
# "Steam Input" marker only appears when Steam is not running.
LOG=$DATA/beaterCore/beaterCore_log.txt; last=-1; quiet=0
for _ in $(seq 1 150); do
    n=$(wc -l <"$LOG" 2>/dev/null || echo 0)
    if [ -n "$(wid)" ] && [ "$n" -gt 50 ] && [ "$n" = "$last" ]; then
        quiet=$((quiet + 1))
        [ "$quiet" -ge 4 ] && break
    else
        quiet=0
    fi
    last=$n
    sleep 1
done
W=$(wid)
[ -n "$W" ] || { echo "!! client never opened a window"; exit 1; }
log "client window $W"

# Connect by IP -> clear field -> type address -> Connect. Retried because a
# click that lands while the animated menu is settling is silently dropped.
for attempt in 1 2 3 4; do
    click_at "$W" 818 272; sleep 3
    click_at "$W" 480 275; sleep 1
    for _ in $(seq 1 40); do DISPLAY=$DISPLAY_NO xdotool key --clearmodifiers BackSpace; done
    DISPLAY=$DISPLAY_NO xdotool type --clearmodifiers --delay 80 "$TARGET"
    sleep 1
    click_at "$W" 480 317
    for _ in $(seq 1 25); do joined && break; sleep 1; done
    joined && break
    log "attempt $attempt did not connect; retrying"
done

if joined; then
    log "client joined $TARGET"
else
    echo "!! client never established a session"
    exit 1
fi
