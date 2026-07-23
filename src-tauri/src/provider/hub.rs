//! Connection store access plus a cache of built provider clients.
//!
//! ## Why a hub instead of `AppState` + `from_connection` per call
//!
//! Two problems this fixes, both surfaced by the M3 whole-milestone review:
//!
//! 1. **Client churn.** Every command used to call `from_connection`, which
//!    builds a fresh `aws_sdk_s3::Client` -- and with it a fresh connector,
//!    connection pool and TLS session cache. Harmless at one click per
//!    second; wasteful at the 12 concurrent part uploads the transfer engine
//!    is designed for (design §5: N×M = 3×4). The client is cheap to *clone*
//!    and expensive to *build*, so it is built once per connection and shared.
//! 2. **Reachability.** The transfer engine is decoupled from IPC by design
//!    (§3 principle 4), so it never has a `State<'_, AppState>` to look a
//!    connection up through. `Arc<ProviderHub>` is a plain value the engine
//!    can hold.
//!
//! ## Lock discipline
//!
//! Two independent locks, and **they are never held at the same time**:
//!
//! - `store` guards the encrypted file's load-modify-save cycles.
//! - `clients` guards the id → client map.
//!
//! [`ProviderHub::provider`] deliberately takes them one after another --
//! peek at the cache and release, load connections and release, then take the
//! cache lock again to insert. Holding both would create a lock-ordering
//! hazard for no benefit, and holding either across a network `.await` is
//! forbidden outright (the discipline M2 Task 5 established).
//!
//! ## Invalidation is blunt on purpose
//!
//! Any successful [`ProviderHub::mutate`] clears the *entire* cache rather
//! than reasoning about which connection's credentials actually changed.
//! Connection edits happen at human speed and a rebuild costs microseconds,
//! so the precision is not worth the risk of missing a case (e.g. an endpoint
//! edit that changes path-style addressing, not just the key). Clients
//! already handed out stay alive in their callers' `Arc`s -- an in-flight
//! upload is not interrupted by an unrelated edit, it just finishes on the
//! client it started with.

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::Mutex;

use crate::error::{AppError, AppResult};
use crate::provider::s3::{from_connection, S3Provider};
use crate::store::{Connection, SecureStore};

pub struct ProviderHub {
    store: Mutex<SecureStore>,
    clients: Mutex<HashMap<String, Arc<S3Provider>>>,
}

impl ProviderHub {
    pub fn new(store: SecureStore) -> Self {
        Self {
            store: Mutex::new(store),
            clients: Mutex::new(HashMap::new()),
        }
    }

    /// Loads the full decrypted connection list. The lock is scoped to the
    /// `load()` alone -- nothing network-bound ever holds it.
    pub async fn connections(&self) -> AppResult<Vec<Connection>> {
        let store = self.store.lock().await;
        store.load()
    }

    /// Runs one load-modify-save cycle under a single lock, then invalidates
    /// the client cache. `f` returning `Err` aborts before `save`, so a failed
    /// edit leaves the persisted list untouched.
    pub async fn mutate<T, F>(&self, f: F) -> AppResult<T>
    where
        F: FnOnce(&mut Vec<Connection>) -> AppResult<T>,
    {
        let outcome = {
            let store = self.store.lock().await;
            let mut connections = store.load()?;
            let value = f(&mut connections)?;
            store.save(&connections)?;
            value
        };
        self.invalidate_all().await;
        Ok(outcome)
    }

    /// Drops every cached client. See the module docs on why this is blunt.
    pub async fn invalidate_all(&self) {
        let mut clients = self.clients.lock().await;
        if !clients.is_empty() {
            tracing::debug!(dropped = clients.len(), "provider cache invalidated");
            clients.clear();
        }
    }

    /// Returns the cached client for `connection_id`, building (and caching)
    /// it on a miss. Fails with `storage/connection-not-found` when no saved
    /// connection has that id.
    pub async fn provider(&self, connection_id: &str) -> AppResult<Arc<S3Provider>> {
        if let Some(hit) = self.clients.lock().await.get(connection_id) {
            return Ok(Arc::clone(hit));
        }

        let connections = self.connections().await?;
        let connection = connections
            .into_iter()
            .find(|c| c.id == connection_id)
            .ok_or_else(|| AppError::ConnectionNotFound {
                id: connection_id.to_string(),
            })?;

        // Built *before* taking the cache lock: `from_connection` is pure
        // configuration work, but keeping it outside the critical section
        // keeps the lock's held time obviously bounded, and a build failure
        // (e.g. an empty endpoint) must not leave a poisoned entry behind.
        let built = Arc::new(from_connection(&connection)?);

        let mut clients = self.clients.lock().await;
        // Double-check: a concurrent caller may have inserted while we were
        // loading. Reusing theirs keeps `Arc::ptr_eq` stable for callers.
        let entry = clients.entry(connection_id.to_string()).or_insert_with(|| {
            tracing::debug!(connection_id, "provider client built");
            built
        });
        Ok(Arc::clone(entry))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hub_with(dir: &tempfile::TempDir) -> ProviderHub {
        ProviderHub::new(SecureStore {
            path: dir.path().join("connections.enc"),
        })
    }

    fn sample(id: &str, secret: &str) -> Connection {
        Connection {
            id: id.to_string(),
            provider: "minio".to_string(),
            name: "n".to_string(),
            endpoint: "http://127.0.0.1:9000".to_string(),
            region: String::new(),
            access_key_id: "AK".to_string(),
            secret_access_key: secret.to_string(),
            default_bucket: None,
        }
    }

    #[tokio::test]
    async fn provider_is_cached_across_calls() {
        let dir = tempfile::tempdir().unwrap();
        let hub = hub_with(&dir);
        hub.mutate(|list| {
            list.push(sample("c1", "s1"));
            Ok(())
        })
        .await
        .unwrap();

        let a = hub.provider("c1").await.unwrap();
        let b = hub.provider("c1").await.unwrap();
        assert!(
            Arc::ptr_eq(&a, &b),
            "second lookup must reuse the cached client, not rebuild it"
        );
    }

    #[tokio::test]
    async fn mutate_invalidates_the_cache() {
        let dir = tempfile::tempdir().unwrap();
        let hub = hub_with(&dir);
        hub.mutate(|list| {
            list.push(sample("c1", "s1"));
            Ok(())
        })
        .await
        .unwrap();
        let before = hub.provider("c1").await.unwrap();

        // Rotating the secret must not leave a client signing with the old one.
        hub.mutate(|list| {
            list[0].secret_access_key = "s2".to_string();
            Ok(())
        })
        .await
        .unwrap();
        let after = hub.provider("c1").await.unwrap();

        assert!(
            !Arc::ptr_eq(&before, &after),
            "a mutation must drop the cached client so new credentials take effect"
        );
    }

    #[tokio::test]
    async fn unknown_connection_id_is_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let hub = hub_with(&dir);
        hub.mutate(|list| {
            list.push(sample("c1", "s1"));
            Ok(())
        })
        .await
        .unwrap();

        let err = hub.provider("nope").await.unwrap_err();
        assert_eq!(err.code(), "storage/connection-not-found");
    }

    #[tokio::test]
    async fn mutate_rolls_back_nothing_on_closure_error() {
        // A closure that fails must not persist a half-applied edit.
        let dir = tempfile::tempdir().unwrap();
        let hub = hub_with(&dir);
        hub.mutate(|list| {
            list.push(sample("c1", "s1"));
            Ok(())
        })
        .await
        .unwrap();

        let err = hub
            .mutate(|list| {
                list.clear();
                Err::<(), _>(AppError::Internal {
                    message: "boom".to_string(),
                })
            })
            .await
            .unwrap_err();
        assert_eq!(err.code(), "internal");

        let list = hub.connections().await.unwrap();
        assert_eq!(list.len(), 1, "failed mutation must not have been saved");
    }

    #[tokio::test]
    async fn invalid_connection_profile_does_not_poison_the_cache() {
        // An endpoint-less connection fails to build; a later fix must be
        // able to succeed rather than hitting a cached failure.
        let dir = tempfile::tempdir().unwrap();
        let hub = hub_with(&dir);
        hub.mutate(|list| {
            let mut c = sample("c1", "s1");
            c.endpoint = String::new();
            list.push(c);
            Ok(())
        })
        .await
        .unwrap();
        assert_eq!(hub.provider("c1").await.unwrap_err().code(), "internal");

        hub.mutate(|list| {
            list[0].endpoint = "http://127.0.0.1:9000".to_string();
            Ok(())
        })
        .await
        .unwrap();
        assert!(hub.provider("c1").await.is_ok());
    }
}
