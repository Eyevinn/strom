#!/usr/bin/env bash
# TEMPORARY: issue #1277 diag. Never merge.
set -u
mkdir -p diag-out
run() {
  local name=$1; shift
  echo "=== $name ==="
  env "$@" cargo test --package strom --features efp --test it mpegtssrt_late_caller_stall \
    -- --ignored --nocapture 2>&1 | grep -v "^\s*Compiling\|^\s*Finished\|^\s*Running" | tee "diag-out/$name.txt"
}
run late-leaky-1 STALL_CALLER=leaky
run late-leaky-2 STALL_CALLER=leaky
run late-leaky-3 STALL_CALLER=leaky
run late-blocking-1 STALL_CALLER=blocking
run late-blocking-2 STALL_CALLER=blocking
run first-leaky STALL_CALLER=leaky STALL_RECEIVER_FIRST=true
run late-leaky-nosync STALL_CALLER=leaky STALL_SYNC=false
echo "=== summary ==="
grep -h RESULT diag-out/*.txt
