# Authentication

> Code is the source of truth — this may have drifted; read the code for the current implementation.

Strom supports two authentication methods to protect your installation.

## 1. Session-Based Authentication (Web Login)

Perfect for web UI access with username/password login.

### Setup

```bash
# Generate a password hash
cargo run -- hash-password
# Or with Docker:
docker run --rm -it eyevinntechnology/strom:latest /app/strom hash-password

# Enter your desired password when prompted
# Copy the generated hash
```

### Configure environment variables

```bash
export STROM_ADMIN_USER="admin"
export STROM_ADMIN_PASSWORD_HASH='$2b$12$...'  # Use single quotes to preserve special characters

# Run Strom
cargo run --release
```

### Usage

- Navigate to `http://localhost:8080`
- Login with your configured username and password
- Session persists for 24 hours of inactivity, at most 30 days after login, and across restarts
- Click "Logout" button in the top-right to end session

### Keeping logins across restarts

The login cookie is signed with a key. On first start Strom generates one and
saves it as `session.key` in the data directory, so a login survives a restart
as long as that directory does (in Docker: mount a volume on `/data`).

To supply the key yourself instead, for example when the data directory is not
persistent:

```bash
export STROM_SESSION_SECRET="$(openssl rand -base64 32)"  # at least 32 characters
```

Keep the key secret: anyone holding it can log in. Changing or deleting it logs
everyone out, and so does changing `STROM_ADMIN_USER` or
`STROM_ADMIN_PASSWORD_HASH`. Logout clears the cookie in the browser, but a copy
of the cookie taken elsewhere stays valid until it expires or one of those
changes.

When Strom serves HTTPS itself (`--tls-cert`), the cookie is marked `Secure`.
Behind a TLS-terminating proxy it is not, so the browser would also send it
over plain HTTP to the same host.

## 2. API Key Authentication (Bearer Token)

Perfect for programmatic access, scripts, and CI/CD.

### Setup

```bash
export STROM_API_KEY="your-secret-api-key-here"

# Run Strom
cargo run --release
```

### Usage

```bash
# All API requests must include the Authorization header
curl -H "Authorization: Bearer your-secret-api-key-here" \
  http://localhost:8080/api/flows
```

Where a header cannot be set (a WebSocket from a browser), pass the key as an
`?auth_token=` query parameter instead. The MCP endpoint (`/api/mcp`) also accepts
it as an `X-API-Key` header; the rest of the API does not.

## Using Both Methods

You can enable both authentication methods simultaneously:

```bash
# Enable both session and API key authentication
export STROM_ADMIN_USER="admin"
export STROM_ADMIN_PASSWORD_HASH='$2b$12$...'
export STROM_API_KEY="your-secret-api-key-here"

cargo run --release
```

Users can then:
- Login via web UI with username/password
- Access API with Bearer token

## Docker Authentication

```bash
docker run -p 8080:8080 \
  -e STROM_ADMIN_USER="admin" \
  -e STROM_ADMIN_PASSWORD_HASH='$2b$12$...' \
  -e STROM_API_KEY="your-api-key" \
  -v $(pwd)/data:/data \
  eyevinntechnology/strom:latest
```

## Disabling Authentication

Authentication is **disabled by default** if no credentials are configured. To run without authentication (development only):

```bash
# Simply run without setting auth environment variables
cargo run --release
```

**Warning:** Never expose an unauthenticated Strom instance to the internet or untrusted networks.

## Protected Endpoints

When authentication is enabled, all API endpoints require authentication except:

- `GET /health` - Health check
- `POST /api/login`, `POST /api/logout`, `GET /api/auth/status` - Login flow
- `GET /api/whep-streams`, `GET /api/whip-endpoints`, `GET /api/ice-servers`,
  `POST /api/client-log` - Used by the browser player and ingest pages
- `/player/*` - Player, ingest and vision mixer pages
- `/whep/*`, `/whip/*` - WebRTC signalling endpoints
- `/devtools/{key}/*` - Remote control of an HTML source; the key in the path is the
  credential (see [HTML_RENDER.md](HTML_RENDER.md))
- Static assets (frontend files)

`/api/mcp` is routed outside the authentication middleware but checks credentials
itself (API key as `X-API-Key` or Bearer, or the login cookie). See [MCP.md](MCP.md).
