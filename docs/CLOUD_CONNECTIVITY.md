# Cloud connectivity: SRT and WebRTC on one public UDP port

> Code is the source of truth — this may have drifted; read the code for the current implementation.

Strom in Kubernetes, in Docker on a bridge network, or behind NAT cannot take inbound media
on ports of its own. Two Eyevinn open-source relays solve that with **one public UDP port
each**, and Strom connects out to them:

| Media | Relay | How Strom uses it |
|---|---|---|
| SRT | [srt-splice](https://github.com/Eyevinn/srt-splice) | `srt+relay://<id>` in an SRT block's `srt_uri`, relays named in `[relays.srt.<name>]` |
| WebRTC (WHIP/WHEP) | [uTURN](https://github.com/Eyevinn/uturn) | a `turn:` entry in Strom's ICE servers |

Both also run as managed services in [Eyevinn Open Source Cloud](https://www.osaas.io) (OSC),
listed in its catalog as **SRT Splice** and **uTURN**.

## When you need them

- **Not needed** when Strom's own ports can be reached: Strom on its own server or VM, or in
  Docker with `network_mode: host`, with the firewall open. Use SRT listener mode and STUN.
- **Needed** when they cannot: Strom in Kubernetes, in Docker on a bridge network (NAT), on
  OSC, or anywhere a port Strom binds is not reachable from outside. Then also set:

  ```sh
  STROM_INBOUND_REACHABLE=false
  ```

  ```toml
  [network]
  inbound_reachable = false
  ```

  `GET /api/capabilities` then reports SRT listener and rendezvous as unavailable, and the UI
  warns on them and suggests the relay.

## SRT: srt-splice

srt-splice pairs two SRT callers that use the same stream id, 1:1, and splices their
handshakes. It does not terminate SRT: ARQ, latency and encryption run end to end between
Strom and the outside party, and the relay never sees the passphrase. Strom dials the relay
as a caller for inputs and outputs alike; the outside party dials the same public port.

```sh
docker run --rm -p 9000:9000/udp ghcr.io/eyevinn/srt-splice:latest
```

Point Strom at it:

```sh
STROM_SRT_RELAY_PUBLIC=srt.example.com:9000   # what the outside party dials
STROM_SRT_RELAY_INTERNAL=srt-splice:9000      # optional: what Strom dials when next to the relay
```

```toml
[relays.srt.default]
public   = "srt.example.com:9000"
internal = "srt-splice:9000"
```

In a flow, set an SRT block's `srt_uri` to `srt+relay://<id>?passphrase=...` (the GUI's
"Relay" connection type generates both). The outside party dials
`srt://srt.example.com:9000?streamid=<id>` with the same passphrase.

Named relays (`srt+relay://<id>@<name>`, `STROM_SRT_RELAYS`), Docker Compose, Kubernetes, OSC
and security: [SRT_RELAY.md](SRT_RELAY.md).

## WebRTC: uTURN

uTURN is a TURN relay for WebRTC on one UDP port, instead of the relay port range a standard
TURN server needs. All clients share one relay address, and uTURN routes client-to-client
traffic by the ICE ufrag, so both sides must be ICE agents; Strom's WHIP and WHEP blocks and
browsers are. It has no TCP transport and no TURNS (TLS), and every participant of a session
must reach the same uTURN instance: run one replica per address and scale out with more
instances.

```sh
docker run -p 3478:3478/udp \
    -e UTURN_EXTERNAL_IP=203.0.113.1 \
    -e UTURN_USERS=alice:secretpass \
    ghcr.io/eyevinn/uturn
```

Set `UTURN_EXTERNAL_IP` to the address clients reach uTURN on, for example a cloud load
balancer's; left out, uTURN detects its egress address, which can be the wrong one. Add it to
Strom's ICE servers next to STUN:

```sh
STROM_SERVER_ICE_SERVERS=stun:turn.example.com:3478,turn:alice:secretpass@turn.example.com:3478
```

```toml
[server]
ice_servers = [
  "stun:turn.example.com:3478",
  "turn:alice:secretpass@turn.example.com:3478",
]
```

uTURN answers STUN Binding requests on the same port, so it can serve as the STUN server too.
Use `turn:`, not `turns:`. To send every WebRTC session through the relay, set
`STROM_SERVER_ICE_TRANSPORT_POLICY=relay` (or per block, with the block's ICE Transport Policy
property). More on Strom's ICE settings: section 7 of
[OPEN_LIVE_SETUP.md](OPEN_LIVE_SETUP.md#7-ice-servers-stun--turn-for-webrtc).

`GET /api/ice-servers` hands the list, credentials included, to Strom's browser pages without
authentication. Give Strom a TURN user of its own, not one shared with other services.

## How clients find out

`GET /api/capabilities` tells a client (Open Live, the GUI, a script) what this Strom can do
on its network, without exposing addresses Strom dials or ICE credentials:

- `network.inbound_reachable`: whether listener modes can work.
- `protocols.srt.modes`: which SRT modes can work here.
- `protocols.srt.relays`: each SRT relay's `name` and `public` address, `default` first.
- `protocols.webrtc.ice_servers` and `ice_transport_policy`: whether ICE servers are
  configured, and the server-wide policy.

The ICE server list itself is at `GET /api/ice-servers`.
