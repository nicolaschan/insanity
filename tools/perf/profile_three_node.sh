#!/usr/bin/env bash
# profile_three_node.sh: automated 3-node insanity profiling (TUI, real conditions).
# Builds --profile profiling with mold/x86-64-v3, runs dir1/dir2 peers plus a
# profiled dir3 in ghostty windows on room test, and captures perf (default)
# or samply profiles. Music mode injects recording audio via a silent
# (unlinked-from-Modi) pw-play fan-out into every insanity input, replicating
# the manual crosspipe hookup with pw-link. Quiet mode captures room tone.
# Must run inside `nix develop`.
#
# Usage:
#   profile_three_node.sh --tag TAG [--profiler samply|perf] [--mode quiet|music|both]
#     [--window S] [--room NAME] [--wav FILE] [--out-dir DIR]
#     [--binary PATH] [--allow-desktop-audio] [--skip-build]
#
# Each run uses a fresh random room (perf-xxxxxxxx) unless --room pins one.

set -euo pipefail
cd "$(dirname "$0")/../.."

TAG=""
PROFILER="perf"
MODE="both"
WINDOW=60
ROOM="test"
ROOM_GIVEN=0
WAV="$HOME/.local/share/insanity-perf/recording_amplified_12.wav"
WAV_SHA256="86597c220b6c9bd092ee21fb01bceeca270a0e14f0b0fe6998e4ae4722c895ee"
OUT_DIR="/tmp"
BINARY=""
ALLOW_DESKTOP_AUDIO=0
SKIP_BUILD=0

usage() {
    sed -n '2,/^$/p' "$0" >&2
    exit "${1:-1}"
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --tag) TAG="$2"; shift 2 ;;
        --profiler) PROFILER="$2"; shift 2 ;;
        --mode) MODE="$2"; shift 2 ;;
        --window) WINDOW="$2"; shift 2 ;;
        --room) ROOM="$2"; ROOM_GIVEN=1; shift 2 ;;
        --wav) WAV="$2"; shift 2 ;;
        --out-dir) OUT_DIR="$2"; shift 2 ;;
        --binary) BINARY="$2"; shift 2 ;;
        --allow-desktop-audio) ALLOW_DESKTOP_AUDIO=1; shift ;;
        --skip-build) SKIP_BUILD=1; shift ;;
        -h|--help) usage 0 ;;
        *) echo "profile_three_node.sh: unknown opt $1" >&2; usage ;;
    esac
done

[[ -n "$TAG" ]] || { echo "profile_three_node.sh: --tag is required" >&2; usage; }
[[ "$TAG" =~ ^[A-Za-z0-9_-]+$ ]] || { echo "profile_three_node.sh: --tag must match [A-Za-z0-9_-]+" >&2; exit 1; }
[[ "$PROFILER" == samply || "$PROFILER" == perf ]] || { echo "profile_three_node.sh: --profiler must be samply or perf" >&2; exit 1; }
[[ "$MODE" == quiet || "$MODE" == music || "$MODE" == both ]] || { echo "profile_three_node.sh: --mode must be quiet, music, or both" >&2; exit 1; }
[[ "$WINDOW" =~ ^[0-9]+$ ]] && [[ "$WINDOW" -ge 10 ]] || { echo "profile_three_node.sh: --window must be an integer >= 10" >&2; exit 1; }
[[ -z "$BINARY" ]] && BINARY="/tmp/insanity-$TAG"
PREFIX="$(basename "$BINARY")"

log() { echo "profile_three_node.sh: $*" >&2; }
die() { echo "profile_three_node.sh: ERROR: $*" >&2; exit 1; }
if [[ "$ROOM_GIVEN" == 0 ]]; then
    ROOM="perf-$(printf '%04x%04x' "$RANDOM" "$RANDOM")"
fi
log "room: $ROOM"

need() { command -v "$1" >/dev/null 2>&1 || die "required tool missing: $1 (run inside nix develop)"; }

port_ids() {
    local kind="$1" want="$2"
    pw-link -I "$kind" 2>/dev/null | awk -v want="$want" '{ sub(/^ +/, ""); id=$1; sub(/^[^ ]+ +/, ""); if ($0 == want) print id }'
}

count_ports() {
    local kind="$1" want="$2"
    pw-link -I "$kind" 2>/dev/null | awk -v want="$want" '{ sub(/^ +/, ""); sub(/^[^ ]+ +/, ""); if ($0 == want) n++ } END { print n+0 }'
}

pw_pairs() {
    pw-link -I -l 2>/dev/null | awk '
        {
            if (index($0, "|->") || index($0, "|<-")) {
                dir = (index($0, "|->") ? "->" : "<-")
                rest = substr($0, index($0, "|") + 3)
                sub(/^ +/, "", rest); sub(/^[0-9]+ +/, "", rest)
                if (dir == "->") print header "\t" rest
                else print rest "\t" header
            } else {
                header = $0; sub(/^ +/, "", header); sub(/^[0-9]+ +/, "", header)
            }
        }' | sort -u
}

count_links_from_to() {
    local src="$1" dst="$2"
    pw-link -I -l 2>/dev/null | awk -v src="$src" -v dst="$dst" '
        {
            if (index($0, "|->")) {
                rest = substr($0, index($0, "|") + 3)
                sub(/^ +/, "", rest); sub(/^[0-9]+ +/, "", rest)
                if (header == src && rest == dst) n++
            } else if (!index($0, "|<-")) {
                header = $0; sub(/^ +/, "", header); sub(/^[0-9]+ +/, "", header)
            }
        } END { print n+0 }'
}

count_links_into() {
    local dst="$1"
    pw-link -I -l 2>/dev/null | awk -v dst="$dst" '
        {
            if (index($0, "|->")) {
                rest = substr($0, index($0, "|") + 3)
                sub(/^ +/, "", rest); sub(/^[0-9]+ +/, "", rest)
                if (rest == dst) n++
            } else if (!index($0, "|<-")) {
                header = $0; sub(/^ +/, "", header); sub(/^[0-9]+ +/, "", header)
            }
        } END { print n+0 }'
}

input_source_ids() {
    local in_id="$1"
    pw-link -I -l 2>/dev/null | awk -v h="$in_id" '
        $0 !~ /\|->/ && $0 !~ /\|<-/ { cur=$0; sub(/^ +/, "", cur); sub(/ +.*/, "", cur); next }
        {
            t=$0; sub(/^ +/, "", t); n=split(t, f)
            if (cur == h && n >= 3 && f[2] == "|<-") print f[3]
        }'
}

port_linked_to() {
    local in_id="$1" out_id="$2"
    pw-link -I -l 2>/dev/null | awk -v h="$in_id" -v o="$out_id" '
        $0 !~ /\|->/ && $0 !~ /\|<-/ { cur=$0; sub(/^ +/, "", cur); sub(/ +.*/, "", cur); next }
        { t=$0; sub(/^ +/, "", t); n=split(t, f); if (cur == h && n >= 3 && f[3] == o) ok=1 }
        END { exit !ok }'
}

profiler_gone() {
    local pid="$1" st
    [[ -d "/proc/$pid" ]] || return 0
    st="$(awk '{print $3}' "/proc/$pid/stat" 2>/dev/null)" || return 0
    [[ "$st" == "Z" ]]
}

GHOST_PIDS=""
PLAYER_PID=""
INSANITY_PATTERN="^$BINARY --room"
DIR3_PATTERN="^$BINARY --room $ROOM --dir dir3"

unlink_all_inputs() {
    local chan in_id src_id
    for chan in FL FR; do
        while read -r in_id; do
            [[ -z "$in_id" ]] && continue
            while read -r src_id; do
                [[ -z "$src_id" ]] && continue
                pw-link -d "$src_id" "$in_id" || die "pw-link -d $src_id -> $in_id failed"
            done < <(input_source_ids "$in_id")
        done < <(port_ids -i "$PREFIX:input_$chan")
    done
    log "unlinked all $PREFIX inputs"
}

assert_inputs_clean() {
    local mode="$1" want="$2" chan total pp other
    for chan in FL FR; do
        total="$(count_links_into "$PREFIX:input_$chan")"
        pp="$(count_links_from_to "pw-play:output_$chan" "$PREFIX:input_$chan")"
        other=$((total - pp))
        if [[ "$mode" == quiet ]]; then
            [[ "$total" == 0 ]] || die "mic still linked in quiet mode ($PREFIX:input_$chan has $total link(s))"
        else
            [[ "$pp" == "$want" ]] || die "music fan-out incomplete ($PREFIX:input_$chan)"
            [[ "$other" == 0 ]] || die "non-player source linked in music mode ($PREFIX:input_$chan)"
        fi
    done
}

teardown() {
    if [[ -n "$PLAYER_PID" ]] && kill -0 "$PLAYER_PID" 2>/dev/null; then
        kill "$PLAYER_PID" 2>/dev/null || true
        for _ in $(seq 1 10); do kill -0 "$PLAYER_PID" 2>/dev/null || break; sleep 1; done
        kill -9 "$PLAYER_PID" 2>/dev/null || true
    fi
    pkill -f "$INSANITY_PATTERN" 2>/dev/null || true
    sleep 2
    for pid in $GHOST_PIDS; do kill "$pid" 2>/dev/null || true; done
    if pw-link -o 2>/dev/null | grep -q '^pw-play:'; then
        log "WARN: pw-play ports still present after teardown"
    fi
    if pw-link -i -o 2>/dev/null | grep -q "$PREFIX:"; then
        log "WARN: $PREFIX ports still present after teardown"
    fi
    log "teardown complete"
}

need ghostty
need pw-play
need pw-link
need python3
if [[ "$PROFILER" == samply ]]; then need samply; else need perf; fi

GOVERNOR="$(cat /sys/devices/system/cpu/cpu0/cpufreq/scaling_governor 2>/dev/null || echo unknown)"
[[ "$GOVERNOR" == "performance" ]] || log "WARN: governor=$GOVERNOR (want performance)"

[[ -f "$WAV" ]] || die "wav file missing: $WAV"
python3 tools/perf/validate_audio.py "$WAV" --expect-sha256 "$WAV_SHA256" || die "reference audio failed validation"

if pw-link -o 2>/dev/null | grep -q '^pw-play:'; then
    die "stale pw-play node in graph; stop other players first"
fi
if pw-link -i -o 2>/dev/null | grep -q "$PREFIX:"; then
    die "stale $PREFIX ports in graph; stop leftover instances first"
fi
if pgrep -f "(samply|perf) record.*$TAG" >/dev/null 2>&1; then
    die "stale profiler for tag $TAG running; kill leftovers first"
fi

if [[ "$ALLOW_DESKTOP_AUDIO" == 0 ]]; then
    if pw_pairs | PREFIX="$PREFIX" awk -F'\t' '
        {
            rb = (index($1, "Rhythmbox:") == 1 || index($2, "Rhythmbox:") == 1)
            ins = (index($1, ENVIRON["PREFIX"] ":") == 1 || index($2, ENVIRON["PREFIX"] ":") == 1)
            if (rb && ins) linked=1
        }
        END { exit !linked }'; then
        die "Rhythmbox is linked to $PREFIX; disconnect it or pass --allow-desktop-audio"
    fi
    if pw-link -o 2>/dev/null | grep -q '^Rhythmbox:'; then
        log "WARN: Rhythmbox running but not linked to $PREFIX; continuing"
    fi
fi

if [[ "$SKIP_BUILD" == 0 ]]; then
    need mold
    need cargo
    log "building profile=profiling with x86-64-v3 + mold"
    RUSTFLAGS="-C target-cpu=x86-64-v3 -C link-arg=-fuse-ld=mold" \
        cargo build --profile profiling --bin insanity
    cp target/profiling/insanity "$BINARY"
    chmod +x "$BINARY"
fi
[[ -x "$BINARY" ]] || die "binary not executable: $BINARY"
if ! file "$BINARY" | grep -q 'with debug_info'; then
    die "$BINARY has no debug symbols; profiling output would be useless"
fi

WORK="$(mktemp -d /tmp/insanity_profile_XXXXXX)"

trap teardown EXIT

log "wiping dir1 dir2 dir3"
rm -rf dir1 dir2 dir3

log "launching peers dir1 dir2 (room $ROOM)"
ghostty -e "$BINARY" --room "$ROOM" --dir dir1 &
GHOST_PIDS="$GHOST_PIDS $!"
ghostty -e "$BINARY" --room "$ROOM" --dir dir2 &
GHOST_PIDS="$GHOST_PIDS $!"

JOINED=0
for _ in $(seq 1 60); do
    N=$(grep -c -h "Connected to " dir1/insanity.log dir2/insanity.log 2>/dev/null | awk '{s+=$1} END {print s+0}' || true)
    if [[ "$N" -ge 2 ]]; then JOINED=1; break; fi
    sleep 2
    pgrep -f "$INSANITY_PATTERN" >/dev/null || die "peer died during join"
done
[[ "$JOINED" == 1 ]] || log "WARN: join gate timed out; proceeding"
sleep 5

for _ in $(seq 1 30); do
    [[ "$(count_ports -i "$PREFIX:input_FL")" -ge 2 ]] && break
    sleep 2
done
[[ "$(count_ports -i "$PREFIX:input_FL")" -ge 2 ]] || die "peer input ports never appeared (want $PREFIX:input_FL)"

log "unlinking mics from peer inputs"
unlink_all_inputs
assert_inputs_clean quiet 0

start_player() {
    log "starting silent player: pw-play --target 0 $WAV"
    pw-play --target 0 "$WAV" >"$WORK/player.log" 2>&1 &
    PLAYER_PID=$!
    sleep 2
    kill -0 "$PLAYER_PID" 2>/dev/null || { tail -n 20 "$WORK/player.log" >&2; die "player died immediately"; }
    for _ in $(seq 1 15); do
        [[ -n "$(port_ids -o "pw-play:output_FL")" ]] && break
        sleep 1
    done
    [[ -n "$(port_ids -o "pw-play:output_FL")" ]] || die "player ports never appeared"
    if pw_pairs | awk -F'\t' '$1 ~ /^pw-play:/ && ($2 ~ /Modi/ || $2 ~ /modi/) { found=1 } END { exit !found }'; then
        die "player auto-linked to Modi despite --target 0"
    fi
}

link_channel() {
    local chan="$1"
    local out_id in_id in_ids
    out_id="$(port_ids -o "pw-play:output_$chan")" || die "port query failed for pw-play:output_$chan"
    [[ -n "$out_id" ]] || die "no pw-play:output_$chan port found"
    [[ "$(printf '%s\n' "$out_id" | wc -l)" == 1 ]] || die "want exactly one pw-play:output_$chan, found: $out_id"
    in_ids="$(port_ids -i "$PREFIX:input_$chan")" || die "port query failed for $PREFIX:input_$chan"
    while read -r in_id; do
        [[ -z "$in_id" ]] && continue
        if ! port_linked_to "$in_id" "$out_id"; then
            pw-link "$out_id" "$in_id" || die "pw-link $out_id -> $in_id failed"
        fi
    done < <(printf '%s\n' "$in_ids")
}

link_player_to_all() {
    local want="$1"
    link_channel FL
    link_channel FR
    [[ "$(count_links_from_to "pw-play:output_FL" "$PREFIX:input_FL")" == "$want" ]] || die "FL fan-out incomplete"
    [[ "$(count_links_from_to "pw-play:output_FR" "$PREFIX:input_FR")" == "$want" ]] || die "FR fan-out incomplete"
    log "player fanned out to $want instance(s)"
}

stop_player() {
    if [[ -n "$PLAYER_PID" ]] && kill -0 "$PLAYER_PID" 2>/dev/null; then
        kill "$PLAYER_PID" 2>/dev/null || true
        for _ in $(seq 1 10); do kill -0 "$PLAYER_PID" 2>/dev/null || break; sleep 1; done
        kill -9 "$PLAYER_PID" 2>/dev/null || true
    fi
    PLAYER_PID=""
    for _ in $(seq 1 10); do
        pw-link -o 2>/dev/null | grep -q '^pw-play:' || break
        sleep 1
    done
    if pw-link -o 2>/dev/null | grep -q '^pw-play:'; then
        die "player ports linger after stop"
    fi
}

run_mode() {
    local mode="$1" out="$2"
    log "[$mode] wiping dir3 and launching profiled instance"
    rm -rf dir3
    rm -f "$out"
    if [[ "$PROFILER" == samply ]]; then
        ghostty -e samply record -o "$out" -- "$BINARY" --room "$ROOM" --dir dir3 &
    else
        ghostty -e perf record -F 997 -g -o "$out" -- "$BINARY" --room "$ROOM" --dir dir3 &
    fi
    GHOST_PIDS="$GHOST_PIDS $!"
    PROFILER_PID=""
    for _ in $(seq 1 60); do
        PROFILER_PID="$(pgrep -f "/$PROFILER record.*$out" || true)"
        if [[ -n "$PROFILER_PID" && "$(printf '%s\n' "$PROFILER_PID" | wc -l)" == 1 ]]; then break; fi
        sleep 1
    done
    [[ -n "$PROFILER_PID" ]] || die "[$mode] profiler process never appeared"
    [[ "$(printf '%s\n' "$PROFILER_PID" | wc -l)" == 1 ]] || die "[$mode] multiple profiler processes match; kill leftovers first"
    log "[$mode] profiler pid $PROFILER_PID"
    for _ in $(seq 1 30); do
        [[ "$(count_ports -i "$PREFIX:input_FL")" -ge 3 ]] && break
        sleep 2
    done
    [[ "$(count_ports -i "$PREFIX:input_FL")" -ge 3 ]] || die "[$mode] dir3 input ports never appeared"
    log "[$mode] unlinking mics from all inputs"
    unlink_all_inputs
    if [[ "$mode" == music ]]; then
        start_player
        link_player_to_all 3
        assert_inputs_clean music 3
    else
        assert_inputs_clean quiet 0
    fi
    log "[$mode] capturing for ${WINDOW}s -> $out"
    sleep "$WINDOW"
    if [[ "$mode" == music ]]; then
        assert_inputs_clean music 3
    else
        assert_inputs_clean quiet 0
    fi
    log "[$mode] stopping profiled instance"
    pkill -f "$DIR3_PATTERN" 2>/dev/null || true
    for _ in $(seq 1 30); do
        pgrep -f "$DIR3_PATTERN" >/dev/null || break
        sleep 2
    done
    if pgrep -f "$DIR3_PATTERN" >/dev/null; then
        pkill -9 -f "$DIR3_PATTERN" 2>/dev/null || true
        sleep 2
    fi
    pgrep -f "$DIR3_PATTERN" >/dev/null && die "[$mode] profiled instance would not die"
    for i in $(seq 1 150); do
        if profiler_gone "$PROFILER_PID"; then
            [[ -f "$out" ]] || die "[$mode] profiler (pid $PROFILER_PID) exited without writing $out"
            break
        fi
        if (( i % 15 == 0 )); then log "[$mode] still finalizing (${i}x2s elapsed)"; fi
        sleep 2
    done
    if ! profiler_gone "$PROFILER_PID"; then
        die "[$mode] profiler still finalizing after grace period (pid $PROFILER_PID)"
    fi
    [[ -f "$out" ]] || die "[$mode] profiler output missing: $out"
    [[ "$(stat -c%s "$out")" -gt 10240 ]] || die "[$mode] profiler output suspiciously small: $out"
    log "[$mode] captured $(stat -c%s "$out") bytes -> $out"
    if grep -ai "stream error\|xrun" dir3/insanity.log 2>/dev/null | head -n 5; then
        log "WARN: [$mode] possible audio stream errors in dir3 log (see above)"
    fi
    if [[ "$mode" == music ]]; then
        stop_player
    fi
}

if [[ "$MODE" == quiet || "$MODE" == both ]]; then
    if [[ "$PROFILER" == samply ]]; then
        run_mode quiet "$OUT_DIR/${TAG}_quiet.json"
    else
        run_mode quiet "$OUT_DIR/${TAG}_quiet.data"
    fi
fi
if [[ "$MODE" == music || "$MODE" == both ]]; then
    if [[ "$PROFILER" == samply ]]; then
        run_mode music "$OUT_DIR/${TAG}_music.json"
    else
        run_mode music "$OUT_DIR/${TAG}_music.data"
    fi
fi

log "done; outputs in $OUT_DIR/${TAG}_{quiet,music}.*"
