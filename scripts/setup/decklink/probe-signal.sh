#!/bin/bash
# Probe DeckLink inputs for signal from inside a strom container.
#
# Iterates device-number x connection combinations and reports SIGNAL / no
# signal per input, plus the detected video mode (resolution / framerate /
# interlace) when present. Useful for figuring out which SDI/HDMI lines are
# live before configuring DeckLink blocks in the strom UI.
#
# Run inside the strom container:
#   docker cp probe-signal.sh strom:/tmp/probe-signal.sh
#   docker exec strom bash /tmp/probe-signal.sh
#
# Or pipe via stdin (no copy needed):
#   docker exec -i strom bash < probe-signal.sh
#
# Override defaults via env:
#   DECKLINK_DEVICES="0 1"           DECKLINK_CONNECTIONS="sdi"
#   DECKLINK_PROBE_TIMEOUT=5         VERBOSE=1

set -u

DEVICES="${DECKLINK_DEVICES:-0 1 2 3 4 5 6 7}"
CONNECTIONS="${DECKLINK_CONNECTIONS:-sdi hdmi optical-sdi component composite svideo}"
PROBE_TIMEOUT="${DECKLINK_PROBE_TIMEOUT:-3}"
VERBOSE="${VERBOSE:-0}"

if ! command -v gst-launch-1.0 >/dev/null 2>&1; then
    echo "error: gst-launch-1.0 not found — run this script inside the strom container" >&2
    exit 1
fi

if ! gst-inspect-1.0 decklinkvideosrc >/dev/null 2>&1; then
    echo "error: GStreamer decklink plugin not loaded" >&2
    echo "       check that /lib/libDeckLinkAPI.so and /lib/blackmagic are mounted" >&2
    exit 1
fi

# gst-device-monitor often does not enumerate DeckLink cards (the plugin
# does not register them as GstDevice instances). Show whatever it knows
# but never use it to decide whether to probe.
echo "=== DeckLink devices visible to gst-device-monitor ==="
echo "(this list can be empty even when the cards work — probing happens below)"
echo
gst-device-monitor-1.0 Video/Source 2>&1 | grep -B1 -A 12 -i decklink || echo "  (none listed)"

echo
echo "=== Probing inputs for signal (timeout ${PROBE_TIMEOUT}s per probe) ==="
echo
printf " %-6s | %-13s | %s\n" "device" "connection" "result"
printf " %s-+-%s-+-%s\n" "------" "-------------" "----------------------------------"

parse_mode() {
    # Read caps from gst-launch -v output and produce a compact mode string.
    local out=$1
    local caps width height fps interlace
    caps=$(echo "$out" | grep -m1 -oE 'video/x-raw[^,]*(,[^,]+)+' | head -1)
    width=$(echo "$caps" | grep -oP 'width=\(int\)\K[0-9]+' | head -1)
    height=$(echo "$caps" | grep -oP 'height=\(int\)\K[0-9]+' | head -1)
    fps=$(echo "$caps" | grep -oP 'framerate=\(fraction\)\K[0-9]+/[0-9]+' | head -1)
    interlace=$(echo "$caps" | grep -oP 'interlace-mode=\(string\)\K\S+' | head -1)

    if [ -n "$width" ] && [ -n "$height" ] && [ -n "$fps" ]; then
        local i_short=""
        case "$interlace" in
            progressive) i_short="p" ;;
            interleaved|mixed) i_short="i" ;;
        esac
        echo "${width}x${height}${i_short} @ ${fps}"
    fi
}

for dev in $DEVICES; do
    for conn in $CONNECTIONS; do
        out=$(timeout "$PROBE_TIMEOUT" gst-launch-1.0 -v decklinkvideosrc \
                device-number="$dev" connection="$conn" mode=auto \
                ! fakesink num-buffers=1 2>&1)
        rc=$?

        if [ "$VERBOSE" = "1" ]; then
            echo "--- dev=$dev conn=$conn rc=$rc ---" >&2
            echo "$out" >&2
        fi

        result=""
        # Order matters: "Signal lost" / "No input source" can co-occur with
        # rc=0 because decklinkvideosrc still produces an EOS even without
        # signal. Check those markers BEFORE trusting the exit code.
        if echo "$out" | grep -qiE "signal lost|no input source|no video signal|no signal"; then
            result="no signal"
        elif echo "$out" | grep -qiE "no such device|no devices found|invalid argument.*device-number|device-number.*out of range"; then
            # Device-number doesn't exist on this host — stop probing higher numbers.
            break
        elif echo "$out" | grep -qiE "not supported|invalid.*connection|connection.*not"; then
            # Connection not valid for this card model — skip silently.
            continue
        elif [ $rc -eq 124 ]; then
            result="no signal (timeout)"
        elif [ $rc -eq 0 ]; then
            mode=$(parse_mode "$out")
            result="SIGNAL${mode:+  $mode}"
        else
            result="error (run with VERBOSE=1)"
        fi

        printf " %-6s | %-13s | %s\n" "$dev" "$conn" "$result"
    done
done

echo
echo "=== Done ==="
