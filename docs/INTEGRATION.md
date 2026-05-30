# Integration Options

How to integrate with Strom from the outside: the REST/OpenAPI API and the MCP server.

## MCP (Model Context Protocol)

Strom speaks MCP so AI assistants (Claude, etc.) can query flows, create/modify them, start/stop
pipelines, and inspect elements. It's available two ways — HTTP at `/api/mcp` on the backend, or
the standalone `strom-mcp-server` (stdio). See [MCP.md](MCP.md) for setup, transports, and the
tool list.

## OpenAPI / Swagger

The REST API is documented with OpenAPI (via `utoipa`):

- **Swagger UI**: `http://localhost:8080/swagger-ui` — interactive, try endpoints in the browser.
- **OpenAPI JSON**: `http://localhost:8080/api-docs/openapi.json`.

A snapshot of the spec is also committed at the repo root (`openapi.json`) and snapshot-tested in CI.

### Generate API clients

Export the spec and generate a client for any language:

```bash
curl http://localhost:8080/api-docs/openapi.json > strom-api.json

openapi-generator-cli generate -i strom-api.json -g python           -o python-client/
openapi-generator-cli generate -i strom-api.json -g typescript-axios -o ts-client/
openapi-generator-cli generate -i strom-api.json -g go              -o go-client/
```

### Validate the spec

```bash
npx @redocly/cli lint strom-api.json
```

## Other integrations

Strom is API-first, so most integrations (metrics export, webhooks, message brokers, alternative
API surfaces, …) can be built on top of the REST/WebSocket API. Ideas for built-in integrations
live in [FEATURE_SUGGESTIONS.md](FEATURE_SUGGESTIONS.md); to propose or contribute one, see
[CONTRIBUTING.md](CONTRIBUTING.md).
