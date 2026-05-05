#!/usr/bin/env bash
# Install chrony with a broadcast/video-production-friendly config.
#
# Replaces systemd-timesyncd with chrony configured to:
#  - Use four Ubuntu NTP pools (>=3 corroborating sources)
#  - Tighten polling to 64 s..256 s for faster drift correction
#  - Require minsources 3 before updating the clock
#  - Install the TAI/UTC leap table (leapsectz right/UTC) so userspace
#    reading CLOCK_TAI gets the correct offset (~37 s vs UTC, not 0)
#
# Reference: https://help.ateliere.com/live/docs/installation/base-platform/ntp/
# See README.md in this directory for measurement results and rationale.
#
# Idempotent: safe to re-run. Existing chrony.conf is preserved at
# /etc/chrony/chrony.conf.pre-ateliere on first run.
#
# Usage:  sudo bash install-chrony.sh

set -euo pipefail

CONF=/etc/chrony/chrony.conf
BACKUP=/etc/chrony/chrony.conf.pre-ateliere

if [[ $EUID -ne 0 ]]; then
    echo "error: must run as root (use sudo)" >&2
    exit 1
fi

if ! command -v apt-get >/dev/null 2>&1; then
    echo "error: this script targets Debian/Ubuntu (apt-get not found)" >&2
    exit 1
fi

echo "=== installing chrony ==="
# tzdata-legacy is pulled in automatically on Ubuntu 24.04+ and provides
# the right/UTC zoneinfo files that leapsectz consumes.
apt-get install -y chrony

if [[ -f "$CONF" && ! -f "$BACKUP" ]]; then
    echo "=== backing up existing $CONF to $BACKUP ==="
    mv "$CONF" "$BACKUP"
fi

echo "=== writing $CONF ==="
cat > "$CONF" <<'EOF'
# Ateliere Live-style chrony.conf for broadcast/video production.
# Reference: https://help.ateliere.com/live/docs/installation/base-platform/ntp/
#
# Ubuntu's NTP pool does NOT apply leap smearing, which matters for
# pipelines that read CLOCK_TAI -- a smear across many hours is worse
# than a clean 1 s step. Using four pools gives chrony >=3 corroborating
# sources so it can vote out falsetickers.

# --- Ateliere-recommended sources ---
# Default polling is minpoll 6 / maxpoll 10 (64 s..1024 s). We tighten to
# 64 s..256 s for faster drift correction without abusing the public pool.
pool ntp.ubuntu.com        iburst maxsources 4 minpoll 6 maxpoll 8
pool 0.ubuntu.pool.ntp.org iburst maxsources 1 minpoll 6 maxpoll 8
pool 1.ubuntu.pool.ntp.org iburst maxsources 1 minpoll 6 maxpoll 8
pool 2.ubuntu.pool.ntp.org iburst maxsources 2 minpoll 6 maxpoll 8

# TAI/UTC leap table for userspace reading CLOCK_TAI.
leapsectz right/UTC

# --- Outlier handling ---
# Require at least 3 corroborating sources before the clock is updated.
minsources 3
# Log selection and per-measurement data so falsetickers show up in /var/log/chrony.
logdir /var/log/chrony
log measurements statistics tracking selection

# --- Standard hygiene ---
makestep 1.0 3
rtcsync
driftfile /var/lib/chrony/chrony.drift
EOF

echo "=== restarting chrony ==="
systemctl restart chrony

echo "=== waiting 8 s for first samples ==="
sleep 8

echo "=== chronyc tracking ==="
chronyc tracking
echo
echo "=== chronyc sources -v ==="
chronyc sources -v
