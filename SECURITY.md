# Security Policy

## Supported Versions

| Version | Supported          |
| ------- | ------------------ |
| 0.3.x   | :white_check_mark: |
| < 0.3   | :x:                |

## Reporting a Vulnerability

If you discover a security vulnerability in Strom, please report it responsibly.

**Do NOT open a public GitHub issue for security vulnerabilities.**

Instead, please send an email to the maintainers with:

1. A description of the vulnerability
2. Steps to reproduce the issue
3. Potential impact
4. Any suggested fixes (optional)

We will acknowledge receipt within 48 hours and aim to provide a fix within 7 days for critical issues.

## Security Considerations

Strom manages GStreamer pipelines and exposes a web interface. When deploying:

- **Authentication**: Enable authentication in production (see [docs/AUTHENTICATION.md](docs/AUTHENTICATION.md))
- **Network**: Run behind a reverse proxy with TLS in production
- **API Keys**: Rotate API keys regularly and use environment variables, not config files
- **Docker**: Use read-only containers where possible and limit capabilities

## Scope

The following are in scope for security reports:

- Authentication/authorization bypass
- Remote code execution
- Path traversal
- Injection vulnerabilities (SQL, command, etc.)
- Sensitive data exposure

The following are generally out of scope:

- Denial of service via resource exhaustion (GStreamer pipelines can be resource-intensive by design)
- Issues requiring physical access
- Social engineering
