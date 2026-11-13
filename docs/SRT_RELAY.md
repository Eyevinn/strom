# SRT relays

> Code is the source of truth — this may have drifted; read the code for the current implementation.

An SRT relay lets an SRT block reach the outside world without a port of its own that the
outside can reach. Strom dials the relay as an SRT caller, the outside party dials the relay
too, and the relay pairs the two by a stream id. Strom uses
[srt-splice](https://github.com/Eyevinn/srt-splice) as the relay.

## When you need one

- **No relay needed** when Strom's own ports can be reached: Strom on its own server or VM,
  or in Docker with `network_mode: host`, with the firewall open. Use SRT listener mode, and
  the outside party dials Strom directly. Strom does not need to be told its own public host:
  clients derive it from how they reach Strom.
- **A relay** when they cannot: Strom in Kubernetes, in Docker on a bridge network (NAT), on
  [Eyevinn Open Source Cloud](https://www.osaas.io) (OSC), or anywhere a listener port is not
  reachable. Set `STROM_INBOUND_REACHABLE=false` there too, so the UI warns on listener mode
  and suggests the relay.
- Caller mode needs neither, when the outside party has a listener Strom can reach.

A relay pairs one sender with one receiver per stream id, like a TURN server pairs two peers.
It is not a distribution server: to send one output to many viewers, use a separate
distribution step.

## Using it in a flow

An SRT block's `srt_uri` names a stream on a relay:

```
srt+relay://<id>[@<relay>][?<query>]
```

- `<id>`: the stream id, 1 to 64 characters from `A-Z a-z 0-9 _ -`. The GUI generates a random
  22-character one.
- `@<relay>`: which configured relay to use. Left out, the block uses the relay called
  `default`. A name the server does not have fails the flow with the configured names.
- `<query>`: SRT parameters such as `passphrase` and `pbkeylen`. `mode` and `streamid` are set
  by the relay connection and not allowed.

Strom resolves it when the flow starts, to
`srt://<relay>?mode=caller&streamid=#!::r=<id>,m=publish|request&<query>`. The outside party
dials `srt://<public>?streamid=<id>` (with the passphrase, if the block has one). The block's
properties panel ("Relay" connection type) and the Links page show that address, with copy, QR
code and VLC.

## Configuring relays

Each relay has a name, a `public` address and an optional `internal` address:

| | Who uses it | Set it to |
|---|---|---|
| `public` | The outside party, and Strom when `internal` is not set | The relay's SRT UDP port as the internet reaches it. Use a DNS name, so the address survives a new IP |
| `internal` | Strom only, never shown | The relay's address inside the network Strom and the relay share. Avoids hairpin NAT |

```toml
[relays.srt.default]
public   = "srt.example.com:9000"
internal = "srt-splice:9000"

[relays.srt.eu-north]
public   = "srt-eu.example.com:9000"
```

Or through the environment:

```sh
STROM_SRT_RELAY_PUBLIC=srt.example.com:9000     # the default relay
STROM_SRT_RELAY_INTERNAL=srt-splice:9000
STROM_SRT_RELAYS=eu-north=srt-eu.example.com:9000,us=srt-us.example.com:9000|srt-splice-us:9000
```

Each address of a relay is merged on its own. Lowest to highest precedence: the file's
`[relays.srt.<name>]`, then the `STROM_SRT_RELAYS` entry of that name, then
`STROM_SRT_RELAY_PUBLIC` / `STROM_SRT_RELAY_INTERNAL` (for `default` only). So `public` can live
in `.strom.toml` and `internal` in the compose environment. `name=public` in `STROM_SRT_RELAYS`
sets only the public address; an empty address after `|` is an error. When more than one source
sets the same address of a relay, Strom logs a warning at startup naming the sources and the one it
uses. Setting `internal` without `public` (after merging) is an error. Names are 1 to 32
characters from `a-z 0-9 -`. Addresses are checked as `host:port` at startup; no relay is
contacted, so a relay may start after Strom. `GET /api/capabilities` reports each relay's name
and public address under `protocols.srt.relays`, never the internal one.

Several relays serve routing (a relay near the outside party), regions, high availability, or
keeping productions apart. The GUI shows a relay selector when more than one is configured.

### Docker Compose

Run srt-splice as a service next to Strom and publish its UDP port on the host:

```yaml
services:
  strom:
    environment:
      STROM_INBOUND_REACHABLE: "false"
      STROM_SRT_RELAY_PUBLIC: srt.example.com:9000   # the host's DNS name
      STROM_SRT_RELAY_INTERNAL: srt-splice:9000      # the compose service
  srt-splice:
    ports:
      - "9000:9000/udp"
```

### Kubernetes

Expose the relay's SRT port to the internet with UDP:

- a `LoadBalancer` Service with `externalTrafficPolicy: Local` (keeps the client's source
  address and skips the extra hop between nodes), or
- a `NodePort` Service, or
- `hostNetwork: true` on the relay pod.

Point a DNS name at that address and use it for `public`. Set `internal` to the relay's
ClusterIP Service (`srt-splice.<namespace>.svc:9000`), so Strom's own traffic stays inside the
cluster.

### OSC

On Eyevinn Open Source Cloud Strom cannot take inbound UDP. Point `STROM_SRT_RELAY_PUBLIC` at
a relay the internet can reach, and leave `internal` unset unless Strom and the relay share a
network.

## Security

The stream id is the only thing that pairs the two sides, so a relay stream without a
passphrase is open to anyone who learns or guesses its id. The GUI generates a random 128-bit id
and a random passphrase for every relay address, and warns on short ids. Strom keeps the id and
the passphrase out of its logs. Share the dial-in address (it carries the passphrase) only with
the party that should connect.

## Limits

- srt-splice is IPv4-only today: give `public` and `internal` IPv4 addresses or names that
  resolve to IPv4.
- One sender and one receiver per stream id.
