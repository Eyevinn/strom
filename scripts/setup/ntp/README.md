# Chrony NTP Setup for Strom

Install and configure `chrony` on a strom host with broadcast/video-production-friendly settings: tight polling, multi-source corroboration, and the TAI/UTC leap table installed so userspace reading `CLOCK_TAI` gets the correct offset (~37 s vs UTC, not 0).

## Why chrony, not systemd-timesyncd

Ubuntu/Debian default to `systemd-timesyncd`, which has two problems for any host running broadcast-style timestamping:

1. **Single source.** `timesyncd` polls one NTP server at a time. With no corroborating peers, the kernel's `maxerror` field grows linearly between polls — on a host with the default fallback (`ntp.ubuntu.com`) and a max poll of ~34 min, `maxerror` reaches the ~1 s range routinely. Strom's `System Clock` panel reads this directly and reports Health=Degraded as a result.
2. **No TAI/UTC leap table.** Without `leapsectz right/UTC` installed, the kernel's TAI−UTC offset stays at 0. Code reading `CLOCK_TAI` (PTP-aware paths, audio/video timestamping) is silently 37 s off. UTC-only monitoring will never surface this.

`chrony` fixes both: multi-source voting (with `minsources 3`), proper outlier rejection, and `leapsectz right/UTC` to populate the TAI table.

The broader rationale follows Ateliere Live's recommendation for video production: <https://help.ateliere.com/live/docs/installation/base-platform/ntp/>. The Ubuntu NTP pool deliberately does **not** apply leap smearing, which is a feature for `CLOCK_TAI` consumers — a smeared second stretched across many hours produces a non-integer TAI/UTC offset and userspace drifts away from reality for the duration of the smear. A clean 1 s step is preferable.

## Install

```bash
sudo bash scripts/setup/ntp/install-chrony.sh
```

The script:

1. Installs the `chrony` package (which auto-masks `systemd-timesyncd` on Ubuntu/Debian, and pulls in `tzdata-legacy` on Ubuntu 24.04+ for the `right/UTC` zoneinfo).
2. Backs up any existing `/etc/chrony/chrony.conf` to `/etc/chrony/chrony.conf.pre-ateliere` (won't overwrite an existing backup on re-run).
3. Writes the broadcast/video-production config (see below) to `/etc/chrony/chrony.conf`.
4. Restarts the `chrony` service.
5. Prints `chronyc tracking` + `chronyc sources -v` after an 8 s warmup.

Idempotent — safe to re-run.

## What the config does

The full conf is embedded in the script. Key choices beyond Ateliere's baseline:

- **Four Ubuntu NTP pools** (`ntp.ubuntu.com`, `0/1/2.ubuntu.pool.ntp.org`) with `iburst`. Provides chrony with enough sources to satisfy `minsources 3` and to vote out falsetickers.
- **`minpoll 6 / maxpoll 8`** on every pool — pins the steady-state poll to 64 s..256 s. Default `maxpoll 10` (1024 s) is unnecessarily loose for broadcast. Public NTP pool guidance asks for `minpoll 6` or slower, which is honored here.
- **`minsources 3`** — do not update the clock unless at least three sources agree. Blocks single-source false updates.
- **`leapsectz right/UTC`** — installs the TAI/UTC offset table so userspace reading `CLOCK_TAI` gets the correct offset. Required for any pipeline doing TAI/PTP-aware timestamping.
- **`log measurements statistics tracking selection`** — per-sample and per-selection entries in `/var/log/chrony/` so a falseticker is attributable after the fact.

## Verify

### From the strom UI

Open the `Clocks` panel. After the switch, the `System Clock` should show:

- **Health: Healthy** (was Degraded with `timesyncd`)
- **TAI − UTC offset: 37 s** (was 0 s — the silent footgun)
- **Max error**: low ms range (was hundreds of ms with `timesyncd`)

### From the host

```bash
chronyc tracking
chronyc sources -v
chronyc sourcestats
```

`chronyc sources -v` should show one `^*` (selected best), one or more `^+` (combined), and the rest `^-` / `^?`. No `x` (falseticker) or `~` (too variable).

## Production deployment results (2026-05-05)

Replaced `systemd-timesyncd` with this config on a strom production host (Ubuntu 24.04.3 LTS, kernel 6.8, hosted environment with sub-10 ms RTT to several stratum-2 servers).

strom GUI `System Clock` panel — before vs. after:

| Field             | timesyncd (before) | chrony (after) |
| ----------------- | ------------------:| --------------:|
| Health            |           Degraded |        Healthy |
| TAI − UTC offset  |              0 s   |     **37 s**   |
| Current offset    |          -11.97 µs |           0 µs |
| Estimated error   |              0 µs ¹|         519 µs |
| Max error         |          940000 µs |       4785 µs  |

¹ `timesyncd` does not populate the kernel's estimated-error field meaningfully — it stays at 0.

Two practically important changes:

1. **TAI − UTC went 0 → 37 s.** The `leapsectz right/UTC` payoff. Code paths reading `CLOCK_TAI` were previously reporting times **37 s wrong** with no warning, because UTC-only monitoring cannot surface this. Single most important reason to run chrony with `leapsectz` on any host doing broadcast-style timestamping.
2. **Max error dropped ~200×** (940 ms → 4.8 ms). This is what flipped Health to Healthy. `timesyncd` couldn't shrink max error because it had only one source; chrony with four pools and `minsources 3` produces a real bounded error from corroborating samples.

`PLL active: no` after the switch is **not** a regression — chrony disciplines the system clock via `adjtimex` frequency steering rather than enabling the kernel PLL loop. By design and generally considered more accurate.

`chronyc tracking` ~8 s after restart showed 77 µs RMS already — better than the home-LAN baseline below at T ≈ 5 min, reflecting the better network position.

## Reference baseline (home-LAN + fiber + public internet)

For comparison, a one-off baseline test of the same config on a consumer-grade setup — useful to gauge what's achievable without local stratum-1 infrastructure.

### Test environment

- Debian 12 (bookworm), kernel 6.1, consumer x86_64 workstation
- Home LAN behind a consumer fiber line, over public internet to the Ubuntu NTP pool — no local stratum-1, no PTP
- Chrony 4.3 from Debian bookworm

### Results

Three measurements after a clean `systemctl restart chrony`:

| Metric                    | T ≈ 5 min | T ≈ 20 min | T ≈ 37 min |
| ------------------------- | ---------:| ----------:| ----------:|
| RMS offset                |    427 µs |     228 µs | **167 µs** |
| Last offset               |     68 µs |     235 ns |    30.6 µs |
| System time vs. reference |      56 ns|      1.5 µs|     13.9 µs|
| Skew                      |  0.94 ppm |  0.165 ppm |**0.092 ppm**|
| Residual freq             | 0.028 ppm |  0.000 ppm | -0.004 ppm |
| Update interval           |     65 s  |     130 s  |     261 s  |
| Root dispersion           |    229 µs |     305 µs |     459 µs |

Sources at T ≈ 37 min:

- 8/8 sources reachable (`reach 377` octal = last 8 polls all successful)
- 1 source selected as best (`^*`), a Nordic stratum-2 server with ~65 µs per-sample std dev
- 7 sources marked not-combined (`^-`) — all measured within 0.5–3 ms of the selected source, but with std dev ranging from ~70 µs up to ~1.5 ms
- No falsetickers (`x`), no too-variable (`~`)
- Polls on stable sources reached `log2 = 8` (256 s) = the configured `maxpoll`. The `log2 = 6` (64 s) `minpoll` was only active during the first few minutes of convergence

### Conclusions from the baseline

1. **Practical sync ceiling over home LAN + fiber + public internet is ≈100–200 µs RMS offset.** The floor is set by WAN jitter to the best single source (~65 µs std dev). No amount of chrony tuning will beat that floor without local time infrastructure.
2. **Frequency stability is effectively at the limit of a consumer PC crystal**: skew 0.092 ppm after ~37 min. Kernel clock drift between polls is negligible; chrony is not the bottleneck.
3. **Chrony does not combine sources (`^+`) in this setup** — the best source is ~15× better than the next best, so combining would degrade, not improve, accuracy. This is correct behavior. Seeing `^+` requires multiple sources of comparable quality, which over public internet typically means multiple LAN-reachable servers.
4. **Outlier visibility is cheap.** Adding `log selection` to `/var/log/chrony/selection.log` costs essentially nothing and turns a "why did the clock jump" post-mortem from guesswork into a lookup.

## Going lower than 100 µs

The only path to sub-100 µs over this kind of network is **local time infrastructure**:

- **GPS-disciplined stratum-1 on the LAN**: a dedicated box with a GPS/GNSS receiver acting as NTP stratum 1. Puts the jitter floor around 10–50 µs depending on switch quality.
- **PTP (IEEE 1588) with hardware timestamping** across PTP-aware switches: sub-microsecond is realistic. Requires NIC and switch support end-to-end.
- For a video production context, PTP is the standard (SMPTE 2059-2 profile). NTP is the baseline; PTP is the target for frame-accurate lock.

## Rollback

```bash
sudo apt-get purge -y chrony
sudo systemctl enable --now systemd-timesyncd
```

The pre-Ateliere chrony.conf (if any existed) is preserved at `/etc/chrony/chrony.conf.pre-ateliere`.

## References

- [Ateliere Live NTP guidance](https://help.ateliere.com/live/docs/installation/base-platform/ntp/)
- [chrony documentation](https://chrony-project.org/documentation.html)
- [Strom Documentation](../../../README.md)
