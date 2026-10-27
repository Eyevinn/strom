#!/usr/bin/env bash
# TEMPORARY: issue #1136 diag. Never merge.
set -u
EXE=$1
N=${N:-30}
mkdir -p diag-out
run_round() {
  local name=$1; shift
  local fails=0
  for i in $(seq 1 "$N"); do
    out=diag-out/$name-$i.txt
    log=diag-out/$name-$i.gst.log
    GST_DEBUG_NO_COLOR=1 GST_DEBUG_FILE=$log timeout 60 "$EXE" --nocapture --test-threads=2 "$@" >"$out" 2>&1
    rc=$?
    if [ $rc -ne 0 ]; then fails=$((fails+1)); echo "$name #$i FAILED rc=$rc"; else rm -f "$log"; fi
    grep -h '^DIAG' "$out" | sed "s/^/$name #$i /"
  done
  echo "== $name: $fails/$N failed" | tee -a diag-out/summary.txt
}
# Each test alone, then both together as CI runs them, then both with light logging.
run_round manager --exact a_mixer_latency_failure_is_reported_once_with_its_inputs
run_round direct --exact finds_both_inputs_of_a_mixer_that_cannot_configure_latency
run_round both
export GST_DEBUG=2,aggregator:4
run_round both-debug
cat diag-out/summary.txt
