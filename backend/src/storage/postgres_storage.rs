//! PostgreSQL-based storage implementation.

use super::{migrate_flow, Result, Storage, StorageError};
use async_trait::async_trait;
use sqlx::postgres::{PgPool, PgPoolOptions};
use sqlx::Row;
use std::collections::HashMap;
use strom_types::{Flow, FlowId};
use tracing::{debug, info, warn};

/// Storage backend that persists flows to PostgreSQL.
pub struct PostgresStorage {
    pool: PgPool,
}

impl PostgresStorage {
    /// Create a new PostgreSQL storage backend.
    ///
    /// The database_url should be in the format:
    /// `postgresql://user:password@localhost/database_name`
    pub async fn new(database_url: &str) -> anyhow::Result<Self> {
        info!("Connecting to PostgreSQL database...");

        let pool = PgPoolOptions::new()
            .max_connections(5)
            .connect(database_url)
            .await?;

        info!("PostgreSQL connection established");

        Ok(Self { pool })
    }

    /// Run database migrations to create the required schema.
    ///
    /// This should be called once at startup to ensure the database schema exists.
    pub async fn run_migrations(&self) -> anyhow::Result<()> {
        info!("Running database migrations...");

        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS flows (
                id UUID PRIMARY KEY,
                name TEXT NOT NULL,
                state TEXT,
                auto_restart BOOLEAN DEFAULT false,
                data JSONB NOT NULL,
                created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
                updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
            )
            "#,
        )
        .execute(&self.pool)
        .await?;

        // Create indexes
        sqlx::query(
            r#"
            CREATE INDEX IF NOT EXISTS idx_flows_name ON flows(name)
            "#,
        )
        .execute(&self.pool)
        .await?;

        sqlx::query(
            r#"
            CREATE INDEX IF NOT EXISTS idx_flows_state ON flows(state)
            "#,
        )
        .execute(&self.pool)
        .await?;

        sqlx::query(
            r#"
            CREATE INDEX IF NOT EXISTS idx_flows_auto_restart ON flows(auto_restart)
            "#,
        )
        .execute(&self.pool)
        .await?;

        sqlx::query(
            r#"
            CREATE INDEX IF NOT EXISTS idx_flows_data ON flows USING gin(data)
            "#,
        )
        .execute(&self.pool)
        .await?;

        info!("Database migrations completed");
        Ok(())
    }

    /// Get a reference to the connection pool for advanced queries.
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }
}

#[async_trait]
impl Storage for PostgresStorage {
    async fn load_all(&self) -> Result<HashMap<FlowId, Flow>> {
        debug!("Loading all flows from PostgreSQL");

        let rows = sqlx::query("SELECT data FROM flows")
            .fetch_all(&self.pool)
            .await
            .map_err(|e| StorageError::Io(std::io::Error::other(e)))?;

        let mut flows = HashMap::new();

        for row in rows {
            let data: serde_json::Value = row.get("data");
            let flow: Flow = serde_json::from_value(data)?;
            let flow = migrate_flow(flow);
            flows.insert(flow.id, flow);
        }

        info!("Loaded {} flows from PostgreSQL", flows.len());
        Ok(flows)
    }

    async fn save_all(&self, flows: &HashMap<FlowId, Flow>) -> Result<()> {
        debug!("Saving {} flows to PostgreSQL", flows.len());

        // Begin a transaction
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| StorageError::Io(std::io::Error::other(e)))?;

        // Delete all existing flows
        sqlx::query("DELETE FROM flows")
            .execute(&mut *tx)
            .await
            .map_err(|e| StorageError::Io(std::io::Error::other(e)))?;

        // Insert all flows
        for flow in flows.values() {
            let data = serde_json::to_value(flow)?;
            let state = flow
                .state
                .as_ref()
                .map(|s| format!("{:?}", s))
                .unwrap_or_else(|| "Null".to_string());

            sqlx::query(
                r#"
                INSERT INTO flows (id, name, state, auto_restart, data, created_at, updated_at)
                VALUES ($1, $2, $3, $4, $5, NOW(), NOW())
                "#,
            )
            .bind(flow.id)
            .bind(&flow.name)
            .bind(&state)
            .bind(flow.properties.auto_restart)
            .bind(&data)
            .execute(&mut *tx)
            .await
            .map_err(|e| StorageError::Io(std::io::Error::other(e)))?;
        }

        // Commit transaction
        tx.commit()
            .await
            .map_err(|e| StorageError::Io(std::io::Error::other(e)))?;

        info!("Successfully saved {} flows to PostgreSQL", flows.len());
        Ok(())
    }

    async fn save_flow(&self, flow: &Flow) -> Result<()> {
        debug!("Saving flow {} to PostgreSQL", flow.id);

        let data = serde_json::to_value(flow)?;
        let state = flow
            .state
            .as_ref()
            .map(|s| format!("{:?}", s))
            .unwrap_or_else(|| "Null".to_string());

        sqlx::query(
            r#"
            INSERT INTO flows (id, name, state, auto_restart, data, created_at, updated_at)
            VALUES ($1, $2, $3, $4, $5, NOW(), NOW())
            ON CONFLICT (id) DO UPDATE SET
                name = EXCLUDED.name,
                state = EXCLUDED.state,
                auto_restart = EXCLUDED.auto_restart,
                data = EXCLUDED.data,
                updated_at = NOW()
            "#,
        )
        .bind(flow.id)
        .bind(&flow.name)
        .bind(&state)
        .bind(flow.properties.auto_restart)
        .bind(&data)
        .execute(&self.pool)
        .await
        .map_err(|e| StorageError::Io(std::io::Error::other(e)))?;

        info!("Successfully saved flow {} to PostgreSQL", flow.id);
        Ok(())
    }

    async fn delete_flow(&self, id: &FlowId) -> Result<()> {
        debug!("Deleting flow {} from PostgreSQL", id);

        let result = sqlx::query("DELETE FROM flows WHERE id = $1")
            .bind(id)
            .execute(&self.pool)
            .await
            .map_err(|e| StorageError::Io(std::io::Error::other(e)))?;

        if result.rows_affected() == 0 {
            warn!("Attempted to delete non-existent flow: {}", id);
            return Err(StorageError::NotFound(*id));
        }

        info!("Successfully deleted flow {} from PostgreSQL", id);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Note: These tests require a PostgreSQL instance to be running.
    // Set the STROM_DATABASE_URL environment variable to run them:
    // STROM_DATABASE_URL=postgresql://user:pass@localhost/strom_test cargo test

    async fn create_test_storage() -> Option<PostgresStorage> {
        let database_url = std::env::var("STROM_DATABASE_URL").ok()?;
        let storage = PostgresStorage::new(&database_url).await.ok()?;
        storage.run_migrations().await.ok()?;

        // Clean up any existing test data
        sqlx::query("DELETE FROM flows")
            .execute(storage.pool())
            .await
            .ok()?;

        Some(storage)
    }

    #[tokio::test]
    async fn test_save_and_load() {
        let Some(storage) = create_test_storage().await else {
            eprintln!("Skipping test: STROM_DATABASE_URL not set");
            return;
        };

        // Create a flow
        let flow = Flow::new("Test Flow");

        // Save it
        storage.save_flow(&flow).await.unwrap();

        // Load all flows
        let flows = storage.load_all().await.unwrap();
        assert_eq!(flows.len(), 1);
        assert_eq!(flows.get(&flow.id).unwrap().name, "Test Flow");
    }

    #[tokio::test]
    async fn test_delete_flow() {
        let Some(storage) = create_test_storage().await else {
            eprintln!("Skipping test: STROM_DATABASE_URL not set");
            return;
        };

        // Create and save a flow
        let flow = Flow::new("Test Flow");
        storage.save_flow(&flow).await.unwrap();

        // Delete it
        storage.delete_flow(&flow.id).await.unwrap();

        // Verify it's gone
        let flows = storage.load_all().await.unwrap();
        assert_eq!(flows.len(), 0);
    }

    #[tokio::test]
    async fn test_update_flow() {
        let Some(storage) = create_test_storage().await else {
            eprintln!("Skipping test: STROM_DATABASE_URL not set");
            return;
        };

        // Create and save a flow
        let mut flow = Flow::new("Test Flow");
        storage.save_flow(&flow).await.unwrap();

        // Update it
        flow.name = "Updated Flow".to_string();
        storage.save_flow(&flow).await.unwrap();

        // Verify the update
        let flows = storage.load_all().await.unwrap();
        assert_eq!(flows.len(), 1);
        assert_eq!(flows.get(&flow.id).unwrap().name, "Updated Flow");
    }
}
