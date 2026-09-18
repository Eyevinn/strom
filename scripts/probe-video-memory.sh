#!/usr/bin/env bash
# Report how this host bridges GPU video memory into consumers that cannot take it.
#
# Phase 1 of issue #682 needs one table per platform and GPU vendor: which
# memory types exist here, what `autovideoconvert` selects for each, and whether
# a GPU-to-GPU path stays on the GPU. Run this on every machine we support and
# paste the output into the issue. It needs GStreamer on PATH and nothing else —
# not a build of Strom, not a GPU for the parts that do not use one.
#
# Every probe is a real pipeline. A row that says OK produced buffers.

set -u

GST_LAUNCH=${GST_LAUNCH:-gst-launch-1.0}
GST_INSPECT=${GST_INSPECT:-gst-inspect-1.0}
BUFFERS=${BUFFERS:-10}
W=${W:-320}
H=${H:-240}

have() { "$GST_INSPECT" "$1" >/dev/null 2>&1; }

# Run a pipeline and report whether it ran, plus what autovideoconvert chose.
#
# The children of an autovideoconvert are the answer this script exists for:
# GStreamer names the sub-bin after the elements inside it, so
# `autovideoconvert-glcolorconvertgldownload` means it downloaded, and
# `autovideoconvert-glupload...` means it stayed on the GPU.
probe() {
    local label="$1"; shift
    local out status kids
    out=$("$GST_LAUNCH" -v "$@" 2>&1)
    status=$?
    # Two shapes to catch. A template of several elements becomes a sub-bin
    # named after them. A template of exactly one element does not: the base
    # class parses with GST_PARSE_FLAG_NO_SINGLE_ELEMENT_BINS, so a lone
    # d3d11convert appears as a plain child and a grep for the bin misses it —
    # which reads as "nothing was selected" and is wrong.
    kids=$(printf '%s' "$out" \
        | grep -oE "GstBin:auto(video)?convert-[a-z0-9]+|GstAutoVideoConvert:[a-z0-9_-]+/Gst[A-Za-z0-9]+:[a-z0-9_-]+" \
        | sed -e 's|.*GstBin:autovideoconvert-||' \
              -e 's|.*/Gst[A-Za-z0-9]*:||' \
              -e 's/[0-9]*$//' \
        | sort -u | tr '\n' ' ')
    if [ $status -eq 0 ] && ! printf '%s' "$out" | grep -qE "ERROR|not-negotiated|erroneous pipeline"; then
        printf '  %-34s OK    %s\n' "$label" "${kids:--}"
    else
        # The chosen sub-bin is the answer this script exists for, so print it
        # on a failure too: a chain that fails still says what was selected,
        # and a selection that reaches for GL on a headless host is the finding.
        printf '  %-34s FAIL  %s\n' "$label" "${kids:--}"
        # A refused link is a WARNING from gst-launch, not an ERROR. Print the
        # whole line: the reason is the point, and truncating it loses the run.
        printf '%s' "$out" | grep -E "(ERROR|WARNING)" | grep -v "^ERROR: pipeline doesn" \
            | head -1 | sed 's/^/        /'
    fi
}

# A producer of `feature` memory, as a gst-launch fragment, or empty when this
# host cannot produce it.
producer_for() {
    case "$1" in
        memory:GLMemory)
            have gltestsrc && printf 'gltestsrc num-buffers=%s ! video/x-raw(memory:GLMemory),width=%s,height=%s' "$BUFFERS" "$W" "$H" ;;
        memory:CUDAMemory)
            have cudaupload && printf 'videotestsrc num-buffers=%s ! video/x-raw,width=%s,height=%s ! cudaupload ! video/x-raw(memory:CUDAMemory)' "$BUFFERS" "$W" "$H" ;;
        memory:D3D11Memory)
            have d3d11upload && printf 'videotestsrc num-buffers=%s ! video/x-raw,width=%s,height=%s ! d3d11upload ! video/x-raw(memory:D3D11Memory)' "$BUFFERS" "$W" "$H" ;;
        memory:VAMemory)
            have vapostproc && printf 'videotestsrc num-buffers=%s ! video/x-raw,width=%s,height=%s ! vapostproc ! video/x-raw(memory:VAMemory)' "$BUFFERS" "$W" "$H" ;;
    esac
}

echo "=== host"
printf '  os               %s %s\n' "$(uname -s)" "$(uname -r)"
printf '  gstreamer        %s\n' "$("$GST_INSPECT" --version | sed -n 2p)"

echo
echo "=== elements present"
for group in "gltestsrc glupload gldownload glcolorconvert" \
             "cudaupload cudadownload cudaconvert" \
             "d3d11upload d3d11download d3d11convert" \
             "vapostproc" \
             "autovideoconvert videoconvertscale"; do
    line=""
    for e in $group; do
        have "$e" && line="$line $e" || line="$line -$e"
    done
    printf '  %s\n' "${line# }"
done

echo
echo "=== encoders present"
line=""
for e in x264enc nvh264enc nvh264device0enc vtenc_h264 vtenc_h264_hw mfh264enc d3d11h264enc vah264enc; do
    have "$e" && line="$line $e"
done
printf '  %s\n' "${line:- none}"

# Strom picks its process-wide converter from these two facts (backend/src/gpu.rs).
# Reporting them here means the table says what Strom would decide on this host.
echo
echo "=== what Strom's probe would see"
have nvh264enc && echo "  nvh264enc present    -> VideoConvertMode::GpuAccelerated (autovideoconvert)" \
               || echo "  nvh264enc absent     -> VideoConvertMode::Software (plain videoconvert)"

for feature in memory:GLMemory memory:CUDAMemory memory:D3D11Memory memory:VAMemory; do
    src=$(producer_for "$feature")
    echo
    if [ -z "$src" ]; then
        echo "=== $feature — no producer on this host, skipped"
        continue
    fi
    echo "=== $feature"
    # shellcheck disable=SC2086
    probe "autovideoconvert -> system memory" $src ! autovideoconvert ! capsfilter caps="video/x-raw" ! fakesink
    # shellcheck disable=SC2086
    probe "autovideoconvert -> same memory" $src ! autovideoconvert ! capsfilter caps="video/x-raw($feature)" ! fakesink
    # shellcheck disable=SC2086
    probe "autovideoconvert -> any sink" $src ! autovideoconvert ! fakesink
    # The control: a plain videoconvert has video/x-raw(ANY) templates, so it
    # passes GPU memory through undownloaded and the dead end appears later.
    # shellcheck disable=SC2086
    probe "videoconvert -> system memory" $src ! videoconvert ! capsfilter caps="video/x-raw" ! fakesink

    for enc in nvh264enc vah264enc d3d11h264enc mfh264enc vtenc_h264_hw x264enc; do
        if have "$enc"; then
            # shellcheck disable=SC2086
            probe "autovideoconvert -> $enc" $src ! autovideoconvert ! "$enc" ! fakesink
            break
        fi
    done
done

echo
echo "Paste this whole output into issue #682. Do not add host names or addresses."
