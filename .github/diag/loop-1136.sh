#!/usr/bin/env bash
# TEMPORARY: issue #1136 diag. Never merge.
set -u
EXE=$1
N=${N:-30}
mkdir -p diag-out
fails=0
for i in $(seq 1 "$N"); do
  out=diag-out/both-$i.txt
  log=diag-out/both-$i.gst.log
  GST_DEBUG=2,aggregator:4,GST_CAPS:3 GST_DEBUG_NO_COLOR=1 GST_DEBUG_FILE=$log "$EXE" --nocapture --test-threads=2 >"$out" 2>&1
  rc=$?
  if [ $rc -ne 0 ]; then fails=$((fails+1)); echo "#$i FAILED rc=$rc"; grep -h "panicked\|failed" "$out" | head -5; else rm -f "$out" "$log"; fi
done
echo "== $RUNNER_OS: $fails/$N failed" | tee diag-out/summary.txt
[ $fails -eq 0 ]
