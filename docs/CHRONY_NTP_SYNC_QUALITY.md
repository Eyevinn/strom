# Chrony NTP Sync Quality over Home LAN + Fiber + Public Internet

Reference notes from a one-off test of an Ateliere Live-style chrony
configuration on a consumer-grade setup. Not directly related to the
strom codebase, but useful as a baseline for what clock sync quality is
achievable without local stratum-1 infrastructure.

## Motivation

Ateliere Live recommends using Ubuntu's NTP pool for broadcast/video
production because none of those servers apply **leap smearing**. A
smeared second stretched across many hours is worse for a pipeline that
reads `CLOCK_TAI` than a clean 1 s step would be — the TAI/UTC offset
becomes non-integer and userspace reading TAI drifts away from reality
for the duration of the smear.

Reference: <https://help.ateliere.com/live/docs/installation/base-platform/ntp/>

Ateliere's documented config is minimal: pool lines + `leapsectz
right/UTC`. This test adds tighter polling and outlier-visibility
logging on top of that baseline.

## Configuration

`/etc/chrony/chrony.conf`:

```conf
# Ateliere Live-style chrony.conf for broadcast/video production.
# Reference: https://help.ateliere.com/live/docs/installation/base-platform/ntp/
#
# Ubuntu's NTP pool does NOT apply leap smearing, which matters for
# pipelines that read CLOCK_TAI — a smear across many hours is worse
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
```

Key choices beyond Ateliere's baseline:

- `minpoll 6 / maxpoll 8` on every pool — pins the steady-state poll to
  64 s..256 s. Default `maxpoll 10` (1024 s) is unnecessarily loose for
  broadcast. Public NTP pool guidance asks for `minpoll 6` or slower,
  which is honored here.
- `minsources 3` — do not update the clock unless at least three
  sources agree. Blocks single-source false updates.
- `log measurements statistics tracking selection` — per-sample and
  per-selection entries in `/var/log/chrony/` so a falseticker is
  attributable after the fact.
- `leapsectz right/UTC` — installs the TAI/UTC offset table so
  userspace reading `CLOCK_TAI` gets the correct offset. Required when
  mixing with PTP/TAI-aware pipelines.

## Apply

```bash
sudo apt-get install -y chrony          # auto-masks systemd-timesyncd
sudo cp chrony.conf /etc/chrony/chrony.conf
sudo systemctl restart chrony
```

Verify:

```bash
chronyc tracking
chronyc sources -v
chronyc sourcestats
```

## Test environment

- Debian 12 (bookworm), kernel 6.1, consumer x86_64 workstation
- Home LAN behind a consumer fiber line, over public internet to the
  Ubuntu NTP pool — no local stratum-1, no PTP
- Chrony 4.3 from Debian bookworm

## Results

Three measurements were taken after a clean `systemctl restart chrony`:

| Metric                    | T ≈ 5 min | T ≈ 20 min | T ≈ 37 min |
| ------------------------- | ---------:| ----------:| ----------:|
| RMS offset                |    427 μs |     228 μs | **167 μs** |
| Last offset               |     68 μs |     235 ns |    30.6 μs |
| System time vs. reference |      56 ns|      1.5 μs|     13.9 μs|
| Skew                      |  0.94 ppm |  0.165 ppm |**0.092 ppm**|
| Residual freq             | 0.028 ppm |  0.000 ppm | -0.004 ppm |
| Update interval           |     65 s  |     130 s  |     261 s  |
| Root dispersion           |    229 μs |     305 μs |     459 μs |

Sources (`chronyc sources -v`) at T ≈ 37 min:

- 8/8 sources reachable (`reach 377` octal = last 8 polls all successful)
- 1 source selected as best (`^*`), a Nordic stratum-2 server with
  ~65 μs per-sample std dev
- 7 sources marked not-combined (`^-`) — all measured within 0.5–3 ms
  of the selected source, but with std dev ranging from ~70 μs up to
  ~1.5 ms
- No falsetickers (`x`), no too-variable (`~`)
- Polls on stable sources reached `log2 = 8` (256 s) = the configured
  `maxpoll`. The `log2 = 6` (64 s) `minpoll` was only active during the
  first few minutes of convergence

## Conclusions

1. **Practical sync ceiling over home LAN + fiber + public internet is
   ≈100–200 μs RMS offset.** The floor is set by WAN jitter to the best
   single source (~65 μs std dev). No amount of chrony tuning will beat
   that floor without local time infrastructure.

2. **Frequency stability is effectively at the limit of a consumer PC
   crystal**: skew 0.092 ppm after ~37 min. Kernel clock drift between
   polls is negligible; chrony is not the bottleneck.

3. **Chrony does not combine sources (`^+`) in this setup** — the best
   source is ~15× better than the next best, so combining would
   degrade, not improve, accuracy. This is correct behavior. Seeing
   `^+` requires multiple sources of comparable quality, which over
   public internet typically means multiple LAN-reachable servers.

4. **Ateliere's non-smearing recommendation holds in practice.** With
   the four Ubuntu pools configured, chrony has more than enough
   sources to vote out anomalies (`minsources 3` was never a blocker
   during this test), and the leap status stays `Normal`.

5. **Outlier visibility is cheap.** Adding `log selection` to
   `/var/log/chrony/selection.log` costs essentially nothing and turns
   a "why did the clock jump" post-mortem from guesswork into a lookup.

## Going lower than 100 μs

The only path to sub-100 μs over this kind of network is **local time
infrastructure**:

- **GPS-disciplined stratum-1 on the LAN**: a dedicated box with a
  GPS/GNSS receiver acting as NTP stratum 1. Puts the jitter floor
  around 10–50 μs depending on switch quality.
- **PTP (IEEE 1588) with hardware timestamping** across PTP-aware
  switches: sub-microsecond is realistic. Requires NIC and switch
  support end-to-end.
- For a video production context, PTP is the standard (SMPTE 2059-2
  profile). NTP is the baseline; PTP is the target for frame-accurate
  lock.

## Rollback

```bash
sudo apt-get purge -y chrony
sudo systemctl enable --now systemd-timesyncd
```

The pre-Ateliere chrony.conf (if any existed) is preserved at
`/etc/chrony/chrony.conf.pre-ateliere` by the install script.
