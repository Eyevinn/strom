# PostgreSQL Storage

Strom supports PostgreSQL as an alternative to JSON file storage for persisting flows.

## Setup

### 1. Create a Database

Each Strom instance should have its own database within your PostgreSQL instance:

```bash
# Create database for a Strom instance
createdb strom_production

# Or for multiple instances
createdb strom_staging
createdb strom_development
```

### 2. Configure STROM_DATABASE_URL

Set the `STROM_DATABASE_URL` environment variable or use the `--database-url` flag:

```bash
# Using environment variable
export STROM_DATABASE_URL="postgresql://user:password@localhost/strom_production"
cargo run

# Or using CLI flag
cargo run -- --database-url "postgresql://user:password@localhost/strom_production"
```

### 3. Automatic Migrations

Strom automatically creates the required database schema on startup. No manual migration is needed.

## Database Schema

The PostgreSQL storage creates a single `flows` table:

```sql
CREATE TABLE flows (
    id UUID PRIMARY KEY,
    name TEXT NOT NULL,
    state TEXT,
    auto_restart BOOLEAN DEFAULT false,
    data JSONB NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- Indexes for common queries
CREATE INDEX idx_flows_name ON flows(name);
CREATE INDEX idx_flows_state ON flows(state);
CREATE INDEX idx_flows_auto_restart ON flows(auto_restart);
CREATE INDEX idx_flows_data ON flows USING gin(data);
```

The `data` column stores the complete Flow document as JSONB, maintaining compatibility with the JSON file format while enabling PostgreSQL's query capabilities.

## Multiple Instances with One PostgreSQL Server

You can run multiple Strom instances sharing the same PostgreSQL server by giving each instance its own database:

```bash
# Instance 1 - Production
STROM_DATABASE_URL=postgresql://user:pass@localhost/strom_production cargo run -- --port 3000

# Instance 2 - Staging
STROM_DATABASE_URL=postgresql://user:pass@localhost/strom_staging cargo run -- --port 3001

# Instance 3 - Development
STROM_DATABASE_URL=postgresql://user:pass@localhost/strom_dev cargo run -- --port 3002
```

Each instance operates independently with complete isolation.

## Connection Pooling

Strom uses sqlx with a connection pool (max 5 connections per instance). This provides efficient database access while limiting resource usage.

## Switching Between JSON and PostgreSQL

### JSON to PostgreSQL Migration

1. Export your flows from JSON (they're already in JSON format)
2. Start Strom with `STROM_DATABASE_URL` configured
3. Import flows via the API or manually insert them

### PostgreSQL to JSON Migration

1. Query flows from PostgreSQL: `SELECT data FROM flows;`
2. Save the `data` column values to a JSON file in the format:
   ```json
   {
     "version": 1,
     "flows": [...]
   }
   ```
3. Start Strom without `STROM_DATABASE_URL` and point to the JSON file

## Benefits of PostgreSQL Storage

- **Reliability**: ACID transactions ensure data consistency
- **Scalability**: Better performance with many flows
- **Backup**: Standard PostgreSQL backup tools (pg_dump, pg_restore)
- **Monitoring**: Use PostgreSQL monitoring tools
- **Query Flexibility**: Can query flows by state, name, or any field
- **Multi-instance**: Easy to run multiple isolated Strom instances

## Fallback to JSON

If `STROM_DATABASE_URL` is not set, Strom falls back to JSON file storage using the configured flows path (default: `~/.local/share/strom/flows.json`).
