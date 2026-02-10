# WHIP ICE Disconnect Investigation (2026-02-10)

## Symptom

Browser ICE disconnects after ~6-7 seconds on **recreated** whipserversrc elements.
The initial element (first connection after server start) works fine — ICE holds 10+ seconds.

- Server-side ICE: stays COMPLETED (never reports DISCONNECTED)
- Browser-side ICE: connected → disconnected after ~6.5s
- Consistent across all recreated elements (webrtcbin1, webrtcbin2, ...)

## What we tested

### Local: Ubuntu 24.04, GStreamer 1.24.2, libnice 0.1.21 (stock)
- Initial element: PASS (10s stability check OK)
- Recreated elements: FAIL (ICE drops after ~6s)

### Local: libnice upgraded to 0.1.22
- Same result. The runtime library upgrade doesn't help because `libgstwebrtcnice-1.0.so`
  was compiled against 0.1.21 and the consent-freshness flag is a **compile-time** check.

### Docker: GStreamer 1.26.6, libnice 0.1.22 (eyevinntechnology/strom:latest)
- Initial element: PASS (10s stability check OK)
- Recreated elements: FAIL (ICE drops after ~6s)
- **Same behavior despite newer GStreamer with consent-freshness compiled in.**

## Root cause analysis

### ICE consent freshness (RFC 7675)

GStreamer's `libgstwebrtcnice` enables consent freshness via `NICE_AGENT_OPTION_CONSENT_FRESHNESS`
when compiled against libnice > 0.1.21.1 (gated by `HAVE_LIBNICE_CONSENT_FIX`).

The `consent-freshness` property on NiceAgent is `CONSTRUCT_ONLY` — it can only be set
at agent creation time via `nice_agent_new_full()`.

Source: `gstreamer/subprojects/gst-plugins-bad/gst-libs/gst/webrtc/nice/nice.c` line 1685:
```c
#if HAVE_LIBNICE_CONSENT_FIX
  options |= NICE_AGENT_OPTION_CONSENT_FRESHNESS;
#endif
  ice->priv->nice_agent = nice_agent_new_full(ice->priv->main_context, ...options);
```

### Why the initial element works but recreated elements don't

The initial whipserversrc element is created during pipeline construction and goes through
the normal state change flow (NULL → READY → PAUSED → PLAYING). Its NiceAgent gets the
correct GLib MainContext and consent freshness timers run properly.

Recreated elements are hot-swapped into a running pipeline:
1. Old element removed from pipeline + set to NULL
2. New element created, added to pipeline, synced to PLAYING

Despite creating a fresh whipserversrc (new webrtcbin, new NiceAgent), something about
the hot-swap causes the consent freshness mechanism to not work on the new element.
The NiceAgent's consent timer may not be running on the correct GLib MainContext, or
the main context may not be iterated for the new agent.

### Key evidence

- The ~6.5 second timeout matches RFC 7675 consent freshness exactly
- Server ICE never reports DISCONNECTED — it doesn't know consent failed
- The problem is **not** environmental (same on GStreamer 1.24.2 and 1.26.6)
- The problem is specific to **recreated** elements in a running pipeline

## What we ruled out

| Hypothesis | Result |
|---|---|
| libnice 0.1.21 missing consent fix | Upgrading to 0.1.22 didn't help |
| libgstwebrtcnice compiled without consent flag | Same problem with GStreamer 1.26.6 Docker image |
| Socket/resource leak from old elements | Fixed (CPU 63% → 5.6%, RTT 54ms → 1ms) but ICE still drops |
| STUN server unreachable | Initial element works fine with same STUN config |
| SDP extmap ID rewriting | Fixed in earlier session, not related |

## Next steps

- Investigate whether the recreated element's NiceAgent gets the correct GLib MainContext
- Check if `gst_webrtc_nice_constructed()` (where NiceAgent is created) receives a
  working main context when the element is added to a running pipeline
- Consider whether the NiceAgent needs explicit main context attachment after hot-swap
- Alternative: instead of destroying/recreating whipserversrc, explore state cycling
  (PLAYING → NULL → PLAYING) on the same element if session cleanup issues can be solved
