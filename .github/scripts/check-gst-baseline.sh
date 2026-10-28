#!/usr/bin/env bash
# Compare the runtime GStreamer with the tested baseline (#964), given as
# MAJOR.MINOR in $GSTREAMER_BASELINE. Below the baseline minor fails the job.
# Above it only warns: Homebrew cannot be pinned, and a newer GStreamer must
# not block the macOS release build. The annotation is how the drift shows.
set -euo pipefail

: "${GSTREAMER_BASELINE:?set GSTREAMER_BASELINE to MAJOR.MINOR}"
gst-inspect-1.0 --version
version=$(gst-inspect-1.0 --version | sed -n 's/^GStreamer \([0-9][0-9.]*\).*/\1/p')
if [[ -z "$version" ]]; then
  echo "::error::could not read the GStreamer version from gst-inspect-1.0"
  exit 1
fi
IFS=. read -r have_major have_minor _ <<<"$version"
IFS=. read -r want_major want_minor _ <<<"$GSTREAMER_BASELINE"
have=$((have_major * 1000 + have_minor))
want=$((want_major * 1000 + want_minor))
if ((have < want)); then
  echo "::error::GStreamer is $version, below the tested baseline $GSTREAMER_BASELINE.x (#964)"
  exit 1
elif ((have > want)); then
  echo "::warning::GStreamer is $version, newer than the tested baseline $GSTREAMER_BASELINE.x (#964): raise the baseline or pin the runner"
fi
