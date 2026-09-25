# Port pool

> Code is the source of truth — this may have drifted; read the code for the current implementation.

Nothing else in Strom can answer "which ports may I use?". A caller that builds flows over the
API has to guess, and two flows that bind the same UDP port fail only at start, with an error
that says nothing about the collision.

The port pool is a general resource allocator: Strom administers a set of port numbers and hands
them out. It knows nothing about who is asking or what they do with the numbers.
[Open Live](https://github.com/Eyevinn/open-live) is the first user; nothing in the design is
specific to it.

## When you need it

Only when more than one caller builds flows on the same Strom. **The pool is off until you
configure ports**, and a Strom with a single caller should leave it off: there is nothing to keep
apart, and a pool would only narrow which ports its own flows may bind.

## Opt-in, at three levels

Nothing here changes behaviour for anyone who does not ask for it.

1. **Operator** — with no ports configured the feature is off. The reservation routes answer
   `409` and nothing else in Strom behaves differently.
2. **Caller** — with a pool configured, a client that never calls the reservation API is
   unaffected. Someone typing `srt://:5000?mode=listener` into the GUI notices nothing.
3. **Flow** — telling the pool which ports a flow uses is a separate, optional call.

No flow path changes. The pool reads the flow list to clean up after deleted flows, so the
dependency runs one way: flow create, update, start and delete are untouched.

## Turning it on

```toml
[ports]
ports = ["47100-47199", 47250, "47300-47399"]
lease_ttl_seconds = 600
```

or `STROM_PORTS=47100-47199,47250` — same grammar, comma-separated. Each entry is a range or a
single port; both expand into one set, so a hole is expressed by listing the pieces around it.
Open every port inbound on the firewall and publish it into the container:

```bash
docker run -d -e STROM_PORTS=47100-47199 -p 8080:8080 -p 47100-47199:47100-47199/udp ...
```

Size the pool for the number of owners times what each reserves. Over-reserving is the intended
usage, not waste: an owner that holds 10 and uses 3 has headroom for what it adds later.

## Model

Three levels, each with its own lifetime:

- **Pool** — the configured port numbers. Static.
- **Reservation** — ports held by an `owner_id` (a caller-chosen string) under a renewable TTL.
  Lives as long as the thing that owns the ports does.
- **Association** — optional: which of a reservation's ports a given flow uses. Bookkeeping and
  a safety net, not a lifetime.

A port returns to the pool only when no live reservation **and** no existing flow holds it.

### Invariants

1. A port number is handed to one owner at a time.
2. **An owner's ports never move while its reservation lives.** Asking again under the same
   `owner_id` returns the same ports, renewed — a restarting client keeps the numbers whatever
   dials it is already configured for. Asking for more adds to the set; the existing ports are
   untouched.
3. Releasing a flow's association returns its ports **to the reservation**, never to the pool.
   An owner keeps its ports across any amount of flow churn.
4. A reservation that lapses while some of its ports are associated with existing flows releases
   only the idle ones. The rest stay held until those flows are gone.

## Routes

All of them sit under Strom's normal API authentication.

| Method | Path | Body | Answer |
|---|---|---|---|
| `GET` | `/api/ports` | | 200 the pool, configured or not |
| `POST` | `/api/ports/reservations` | `{"owner_id": "...", "count": 10, "ttl_secs"?: 600}` | 201 new, 200 the owner's existing reservation renewed, 400 bad input, 409 not enough free ports or no pool |
| `GET` | `/api/ports/reservations` | | 200 `[reservation]`, live ones only |
| `GET` | `/api/ports/reservations/{id}` | | 200, 404 |
| `POST` | `/api/ports/reservations/{id}/renew` | `{"ttl_secs"?: 600}` or empty | 200, 404 |
| `DELETE` | `/api/ports/reservations/{id}` | | 204, 404 |
| `POST` | `/api/ports/reservations/{id}/assign` | `{"flow_id": "...", "ports": [47100, 47101]}` | 200, 400 a port is not in this reservation, 404 |
| `DELETE` | `/api/ports/reservations/{id}/assign/{flow_id}` | | 204, 404 |

A reservation:

```json
{
  "id": "bd9f91b3-1619-434f-b726-f81343c728ae",
  "owner_id": "my-production",
  "ports": [47100, 47101, 47102],
  "created_at": "2026-09-23T13:39:35Z",
  "expires_at": "2026-09-23T13:49:35Z",
  "in_use": [{ "port": 47100, "flow_id": "..." }]
}
```

Allocation prefers a contiguous run, and growth prefers extending the block the owner already
has, but **neither is guaranteed** — a hole in the pool, another owner's ports, or a blocked
port can put a gap anywhere. `ports` is an explicit list and callers must not assume contiguity.
A `count` lower than the current size is a no-op; ports are given back by deleting the
reservation, not by shrinking it.

## Flow association

`POST .../assign` records "these ports of this reservation are used by that flow". The pool will
not return those ports while the flow exists, which is what makes invariant 4 work. Assigning
again for the same flow replaces what was recorded, so a caller can correct itself. Assigning a
port the reservation does not hold is a `400`.

The association is dropped when the caller releases it, or when the flow no longer exists — the
pool reconciles against the current flow list at startup and on every route that reads or changes
it, so no hook in the flow lifecycle is needed and an association whose flow is gone is never
observable.

Nothing verifies that a flow actually binds the ports it was assigned. A caller that never
assigns anything gets plain TTL behaviour and nothing breaks.

## Reading the pool

`GET /api/ports` answers `200` whether or not a pool is configured, so a client never has to
read that off a status code:

```json
{
  "enabled": true,
  "ports": [{ "first": 47100, "last": 47199 }],
  "total": 100,
  "free": 88,
  "entries": [
    { "port": 47100, "state": "assigned", "owner_id": "my-production", "reservation_id": "…", "flow_id": "…" },
    { "port": 47101, "state": "reserved", "owner_id": "my-production", "reservation_id": "…" },
    { "port": 47150, "state": "blocked" }
  ]
}
```

`entries` lists only ports that are **not** free, so the response is proportional to what is
interesting rather than to pool size — a pool of nine hundred idle ports answers with an empty
list.

Every reservation route answers `409` while no pool is configured, with the setting to change in
the body. A client can treat that the same way it treats an exhausted pool: carry on without
reserved ports.

## Probing before handing out

`probe_before_handout` (default on) binds a candidate port — UDP and TCP, on `0.0.0.0`, without
`SO_REUSEPORT` — before handing it out. If either bind fails, something outside Strom holds the
number: it is skipped, marked `blocked` in `GET /api/ports`, and logged at `WARN` on the
transition into that state. A blocked port is re-probed on later allocations and comes back if
whatever held it goes away.

Without it, handing out a number another process already holds produces exactly the failure the
pool exists to prevent, in its worst form: the flow is built, looks correct, and then fails to
start some of the time with nothing in the record pointing at the cause.

It is a diagnostic, not a guarantee:

- Something can take the number between the probe and the flow's own bind, and nothing in Strom
  can close that window.
- It only sees its own network namespace. Under `network_mode: host` that is the host's real port
  space and the check means something. Behind Docker bridge networking with published ports it is
  not, and a conflict on the host stays invisible — that, not cost, is the reason to turn it off.

## Persistence

Reservations and their associations are written to `port_reservations.json` in the data
directory, **not** through the flow storage backend. Port numbers are host-local, and a shared
PostgreSQL serving two Strom nodes would conflate two different hosts' port spaces and hand the
same numbers to both. Without persistence a restart would drop every owner's reservation and the
next request could hand out different numbers than the clients are already configured to dial.

## Known limitations

These are deliberate. The feature hands out numbers; it does not model what a pipeline opens.

1. **Strom tracks numbers, not bindings.** Nothing checks that a flow uses the ports it was
   assigned, or that it does not bind one nobody reserved. A flow built by hand in the GUI on a
   pool port is invisible to the pool unless someone assigns it. The probe narrows this: a
   hand-built flow that is *running* holds the port, so the probe finds it bound and skips the
   number. One that is merely stopped does not, and collides when it starts.
2. **No protocol distinction.** A number is handed out once, covering both UDP and TCP use. This
   is also why the probe has to try both.
3. **Limited knowledge of the rest of the host.** See the probe's caveats above.
4. **`owner_id` is not authenticated.** Any caller holding Strom's API key can renew or delete
   another owner's reservation. This matches Strom's current single-key posture, and the exposure
   grows with the number of owners sharing one Strom — who also see and can edit each other's
   flows. A port pool does not make a shared Strom multi-tenant; it closes one specific hole.
   Tenant isolation is a separate problem.
5. **Reachability is not verified.** The ports still have to be opened in the firewall and
   published into the container; Strom cannot check either.
