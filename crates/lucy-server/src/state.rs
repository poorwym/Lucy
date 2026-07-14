use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use lucy_core::source::{ConfigError, SourceCatalog, SourceConfig};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::task::JoinHandle;
use tokio_postgres::{Client, NoTls, Statement};
use tracing::{debug, error};

use crate::error::{RouteError, ServerError};
use crate::settings::ServerSettings;

const POSTGRES_CONNECTIONS_PER_POOL: usize = 8;

#[derive(Clone, Debug)]
pub struct AppState {
    catalog: Arc<SourceCatalog>,
    default_source_id: Arc<str>,
    settings: ServerSettings,
    postgres_pools: Arc<PostgresPools>,
}

impl AppState {
    pub fn new(catalog: SourceCatalog, settings: ServerSettings) -> Result<Self, ServerError> {
        let default_source_id =
            catalog
                .default_source_id()
                .map(str::to_string)
                .ok_or(ServerError::Config(ConfigError::Validation(
                    "at least one source must be configured".to_string(),
                )))?;

        Ok(Self {
            catalog: Arc::new(catalog),
            default_source_id: Arc::from(default_source_id),
            settings,
            postgres_pools: Arc::new(PostgresPools::new(POSTGRES_CONNECTIONS_PER_POOL)),
        })
    }

    pub(crate) fn default_source(&self) -> Result<SourceConfig, RouteError> {
        self.source(&self.default_source_id)
    }

    pub(crate) fn source(&self, source_id: &str) -> Result<SourceConfig, RouteError> {
        self.catalog
            .sources
            .get(source_id)
            .cloned()
            .ok_or_else(|| RouteError::not_found(format!("unknown source {source_id}")))
    }

    pub(crate) fn source_count(&self) -> usize {
        self.catalog.sources.len()
    }

    pub(crate) fn default_source_id(&self) -> &str {
        &self.default_source_id
    }

    pub(crate) fn config_path(&self) -> Option<String> {
        self.settings.config_path.as_deref().map(str::to_string)
    }

    pub(crate) async fn postgres_client(
        &self,
        source_id: &str,
        connection: &str,
    ) -> Result<PooledPostgresClient, tokio_postgres::Error> {
        self.postgres_pools
            .pool(source_id, connection)
            .acquire()
            .await
    }
}

struct PostgresPools {
    max_connections_per_pool: usize,
    pools: Mutex<HashMap<String, HashMap<String, Arc<PostgresPool>>>>,
}

impl PostgresPools {
    fn new(max_connections_per_pool: usize) -> Self {
        assert!(
            max_connections_per_pool > 0,
            "Postgres connection pool limit must be positive"
        );
        Self {
            max_connections_per_pool,
            pools: Mutex::new(HashMap::new()),
        }
    }

    fn pool(&self, source_id: &str, connection: &str) -> Arc<PostgresPool> {
        let mut pools = self
            .pools
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(pool) = pools
            .get(source_id)
            .and_then(|connections| connections.get(connection))
        {
            return Arc::clone(pool);
        }

        let pool = Arc::new(PostgresPool::new(
            source_id,
            connection,
            self.max_connections_per_pool,
        ));
        pools
            .entry(source_id.to_string())
            .or_default()
            .insert(connection.to_string(), Arc::clone(&pool));
        pool
    }
}

impl fmt::Debug for PostgresPools {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PostgresPools")
            .field("max_connections_per_pool", &self.max_connections_per_pool)
            .finish_non_exhaustive()
    }
}

struct PostgresPool {
    source_id: Arc<str>,
    connection: Arc<str>,
    max_connections: usize,
    idle: Mutex<Vec<PoolConnection>>,
    checkout_permits: Arc<Semaphore>,
}

impl PostgresPool {
    fn new(source_id: &str, connection: &str, max_connections: usize) -> Self {
        Self {
            source_id: Arc::from(source_id),
            connection: Arc::from(connection),
            max_connections,
            idle: Mutex::new(Vec::with_capacity(max_connections)),
            checkout_permits: Arc::new(Semaphore::new(max_connections)),
        }
    }

    async fn acquire(self: Arc<Self>) -> Result<PooledPostgresClient, tokio_postgres::Error> {
        let started = Instant::now();
        let permit = Arc::clone(&self.checkout_permits)
            .acquire_owned()
            .await
            .expect("Postgres pool semaphore is never closed");
        let pool_wait_ms = started.elapsed().as_secs_f64() * 1_000.0;

        while let Some(connection) = self.take_idle() {
            if !connection.client.is_closed() {
                debug!(
                    source_id = %self.source_id,
                    pool_wait_ms,
                    "pooled PostGIS connection checked out"
                );
                return Ok(PooledPostgresClient::new(self, connection, permit));
            }
            debug!(
                source_id = %self.source_id,
                "discarded closed PostGIS connection from pool"
            );
        }

        let connection_started = Instant::now();
        let (client, connection_driver) =
            tokio_postgres::connect(self.connection.as_ref(), NoTls).await?;
        debug!(
            source_id = %self.source_id,
            pool_wait_ms,
            duration_ms = connection_started.elapsed().as_secs_f64() * 1_000.0,
            "PostGIS connection established"
        );
        let source_id = Arc::clone(&self.source_id);
        let connection_task = tokio::spawn(async move {
            if let Err(connection_error) = connection_driver.await {
                error!(
                    source_id = %source_id,
                    error = %connection_error,
                    "pooled PostGIS connection task failed"
                );
            }
        });

        Ok(PooledPostgresClient::new(
            self,
            PoolConnection::new(client, connection_task),
            permit,
        ))
    }

    fn take_idle(&self) -> Option<PoolConnection> {
        self.idle
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pop()
    }

    fn return_idle(&self, connection: PoolConnection) {
        let mut idle = self
            .idle
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        debug_assert!(idle.len() < self.max_connections);
        idle.push(connection);
    }
}

struct PoolConnection {
    client: Client,
    statements: HashMap<String, Statement>,
    connection_task: JoinHandle<()>,
}

impl PoolConnection {
    fn new(client: Client, connection_task: JoinHandle<()>) -> Self {
        Self {
            client,
            statements: HashMap::new(),
            connection_task,
        }
    }
}

impl Drop for PoolConnection {
    fn drop(&mut self) {
        // Dropping a RowStream asks tokio-postgres to drain the pending result.
        // An explicitly discarded pooled connection may have exited a stream
        // early after an exact feature-limit failure, so abort its driver to
        // cancel that work instead of leaving a detached draining socket.
        self.connection_task.abort();
    }
}

pub(crate) struct PooledPostgresClient {
    pool: Arc<PostgresPool>,
    connection: Option<PoolConnection>,
    reusable: bool,
    _permit: OwnedSemaphorePermit,
}

impl PooledPostgresClient {
    fn new(
        pool: Arc<PostgresPool>,
        connection: PoolConnection,
        permit: OwnedSemaphorePermit,
    ) -> Self {
        Self {
            pool,
            connection: Some(connection),
            reusable: true,
            _permit: permit,
        }
    }

    pub(crate) fn client(&self) -> &Client {
        &self
            .connection
            .as_ref()
            .expect("pooled Postgres client exists until drop")
            .client
    }

    pub(crate) fn client_mut(&mut self) -> &mut Client {
        &mut self
            .connection
            .as_mut()
            .expect("pooled Postgres client exists until drop")
            .client
    }

    pub(crate) async fn prepare_cached(
        &mut self,
        sql: &str,
    ) -> Result<Statement, tokio_postgres::Error> {
        if let Some(statement) = self
            .connection
            .as_ref()
            .expect("pooled Postgres client exists until drop")
            .statements
            .get(sql)
        {
            return Ok(statement.clone());
        }

        let started = Instant::now();
        let statement = self.client().prepare(sql).await?;
        debug!(
            source_id = %self.pool.source_id,
            duration_ms = started.elapsed().as_secs_f64() * 1_000.0,
            "PostGIS statement prepared"
        );
        self.connection
            .as_mut()
            .expect("pooled Postgres client exists until drop")
            .statements
            .insert(sql.to_string(), statement.clone());
        Ok(statement)
    }

    /// Prevent this connection from returning to the pool.
    ///
    /// This is required when a streaming query exits before PostgreSQL has
    /// finished sending rows: dropping the client closes that connection
    /// instead of making a still-draining connection available to a new
    /// request.
    pub(crate) fn discard(&mut self) {
        self.reusable = false;
    }

    /// Allow a connection guarded with [`Self::discard`] to return to the pool
    /// after its potentially streaming operation completed successfully.
    pub(crate) fn reuse(&mut self) {
        self.reusable = true;
    }
}

impl Drop for PooledPostgresClient {
    fn drop(&mut self) {
        let Some(connection) = self.connection.take() else {
            return;
        };
        if self.reusable && !connection.client.is_closed() {
            // Return the client before `_permit` is dropped so a waiter cannot
            // acquire a slot, observe no idle client, and open a ninth socket.
            self.pool.return_idle(connection);
        } else {
            debug!(
                source_id = %self.pool.source_id,
                explicitly_discarded = !self.reusable,
                "PostGIS connection not returned to pool"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn postgres_pools_are_scoped_by_source_and_connection() {
        let pools = PostgresPools::new(2);
        let first = pools.pool("buildings", "postgres://localhost/one");
        let same = pools.pool("buildings", "postgres://localhost/one");
        let other_source = pools.pool("trees", "postgres://localhost/one");
        let other_connection = pools.pool("buildings", "postgres://localhost/two");

        assert!(Arc::ptr_eq(&first, &same));
        assert!(!Arc::ptr_eq(&first, &other_source));
        assert!(!Arc::ptr_eq(&first, &other_connection));
        assert_eq!(first.checkout_permits.available_permits(), 2);
    }
}
