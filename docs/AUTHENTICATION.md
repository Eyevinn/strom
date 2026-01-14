# Authentication

Strom supports two authentication methods to protect your installation.

## 1. Session-Based Authentication (Web Login)

Perfect for web UI access with username/password login.

### Setup

```bash
# Generate a password hash
cargo run -- hash-password
# Or with Docker:
docker run eyevinntechnology/strom:latest hash-password

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
- Session persists for 24 hours of inactivity
- Click "Logout" button in the top-right to end session

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

When authentication is enabled, all API endpoints except the following require authentication:

- `GET /health` - Health check
- `POST /api/login` - Login endpoint
- `POST /api/logout` - Logout endpoint
- `GET /api/auth/status` - Check auth status
- Static assets (frontend files)
