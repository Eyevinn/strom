#!/usr/bin/env bash
# TEMPORARY: issue #1277 diag. Never merge.
set -u
mkdir -p diag-out
run() {
  local name=$1; shift
  echo "=== $name ==="
  env GST_DEBUG_NO_COLOR=1 GST_DEBUG_FILE="$PWD/diag-out/$name.gst.log" "$@" \
    cargo test --package strom --features efp --test it mpegtssrt_late_caller_stall \
    -- --ignored --nocapture 2>&1 | grep -v "^\s*Compiling\|^\s*Finished\|^\s*Running" | tee "diag-out/$name.txt"
  # Keep the GStreamer log only where the run dropped.
  if grep -q "dropped 0 (0.0 %)" "diag-out/$name.txt"; then rm -f "diag-out/$name.gst.log"; else gzip -f "diag-out/$name.gst.log" 2>/dev/null; fi
}
DBG="GST_DEBUG=2,vtdec*:5,decodebin:4,multiqueue:4,tsdemux:4,videodecoder:4,basesink:4"
for i in 1 2 3 4 5 6; do run vtdec-$i STALL_CALLER=leaky "$DBG"; done
for i in 1 2 3 4; do run avdec-$i STALL_CALLER=leaky "$DBG" GST_PLUGIN_FEATURE_RANK=vtdec_hw:NONE,vtdec:NONE; done
for i in 1 2 3; do run nodecode-$i STALL_CALLER=leaky STALL_DECODE=false "$DBG"; done
echo "=== summary ==="
for f in diag-out/*.txt; do echo "$f: $(grep -h RESULT $f)"; done
