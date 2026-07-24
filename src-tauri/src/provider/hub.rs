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
//! That three-phase shape leaves a window with **no lock held** between
//! "load the connection snapshot" and "take the cache lock to insert". If a
//! full [`ProviderHub::mutate`] cycle (load -> modify -> save ->
//! `invalidate_all`) completes entirely inside that window, an unguarded
//! insert would repopulate the cache `invalidate_all` just cleared with a
//! client built from the *pre-mutation* snapshot -- and, unlike an in-flight
//! upload finishing on the client it started with (see below), that stale
//! client would then sit in the cache serving every *subsequent* caller
//! until the next mutation. An `epoch: AtomicU64` closes this without ever
//! holding both locks at once: `invalidate_all` bumps it in the same
//! critical section as `clear()` (both under the `clients` lock, so
//! "cleared" and "epoch moved" can never be observed apart), and `provider`
//! reads the epoch before loading the snapshot and again right before
//! inserting. A mismatch means a mutation landed mid-lookup, so the freshly
//! built client is discarded and the lookup retried (bounded -- see
//! [`ProviderHub::provider`]) rather than ever being cached.
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
//! client it started with. That is the one and only accepted staleness: the
//! epoch guard above exists precisely so "blunt invalidation" can't be
//! quietly undone by a *new* lookup re-caching pre-mutation data moments
//! after the clear.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use tokio::sync::Mutex;

use crate::error::{AppError, AppResult};
use crate::provider::s3::{from_connection, S3Provider};
use crate::store::{Connection, SecureStore};

/// Bounds the epoch-guard retry loop in [`ProviderHub::provider`]. A handful
/// of attempts is plenty in practice -- connection edits happen at human
/// speed, so losing the race even once, let alone three times running, needs
/// sustained concurrent mutation pressure that real usage doesn't produce.
const MAX_PROVIDER_ATTEMPTS: u32 = 3;

pub struct ProviderHub {
    store: Mutex<SecureStore>,
    clients: Mutex<HashMap<String, Arc<S3Provider>>>,
    /// Bumped under the `clients` lock every time [`ProviderHub::invalidate_all`]
    /// clears the map. [`ProviderHub::provider`] reads this before loading a
    /// connection snapshot and again right before caching the client it
    /// built from that snapshot; a mismatch means a `mutate()` completed in
    /// between, so the snapshot may be stale and the build is discarded
    /// instead of cached. See the module docs' "Lock discipline" section.
    epoch: AtomicU64,
}

impl ProviderHub {
    pub fn new(store: SecureStore) -> Self {
        Self {
            store: Mutex::new(store),
            clients: Mutex::new(HashMap::new()),
            epoch: AtomicU64::new(0),
        }
    }

    /// Loads the full decrypted connection list. The lock is scoped to the
    /// `load()` alone -- nothing network-bound ever holds it.
    pub async fn connections(&self) -> AppResult<Vec<Connection>> {
        let store = self.store.lock().await;
        store.load()
    }

    /// The id of every saved connection, and nothing else. Used by the startup
    /// checkpoint restore (M4c) to tell a live connection's checkpoints from an
    /// orphan's without ever handling -- or logging -- a credential: it projects
    /// the decrypted list down to ids in memory and drops the rest immediately.
    /// Same lock scope as [`ProviderHub::connections`].
    pub async fn connection_ids(&self) -> AppResult<Vec<String>> {
        let store = self.store.lock().await;
        Ok(store.load()?.into_iter().map(|c| c.id).collect())
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
    ///
    /// Bumps `epoch` in the *same* critical section as `clear()` -- both
    /// under the `clients` lock -- unconditionally, even if the cache
    /// happened to already be empty. What matters for the epoch guard in
    /// [`ProviderHub::provider`] is not whether anything was actually
    /// dropped here, but that a mutation completed between some caller's
    /// snapshot load and its insert attempt; the guard needs to see that
    /// every time, not just when there was something to clear.
    pub async fn invalidate_all(&self) {
        let mut clients = self.clients.lock().await;
        self.epoch.fetch_add(1, Ordering::SeqCst);
        if !clients.is_empty() {
            tracing::debug!(dropped = clients.len(), "provider cache invalidated");
            clients.clear();
        }
    }

    /// Inserts `built` under `connection_id` iff the epoch hasn't moved
    /// since `epoch_before` was read -- i.e. no [`ProviderHub::mutate`]
    /// completed its `invalidate_all` in between. Returns `None` when the
    /// epoch moved, refusing the insert rather than caching a client that
    /// may have been built from a now-superseded snapshot.
    ///
    /// On a match, still re-checks the map itself: a concurrent caller may
    /// have inserted (at the same, current epoch) while this caller was
    /// building. Reusing theirs keeps `Arc::ptr_eq` stable for callers.
    async fn insert_if_epoch_matches(
        &self,
        connection_id: &str,
        epoch_before: u64,
        built: Arc<S3Provider>,
    ) -> Option<Arc<S3Provider>> {
        let mut clients = self.clients.lock().await;
        if self.epoch.load(Ordering::SeqCst) != epoch_before {
            return None;
        }
        let entry = clients.entry(connection_id.to_string()).or_insert_with(|| {
            tracing::debug!(connection_id, "provider client built");
            built
        });
        Some(Arc::clone(entry))
    }

    /// Returns the cached client for `connection_id`, building (and caching)
    /// it on a miss. Fails with `storage/connection-not-found` when no saved
    /// connection has that id.
    ///
    /// Guarded against the race described in the module docs: up to
    /// [`MAX_PROVIDER_ATTEMPTS`] times, this reads the epoch, loads a
    /// snapshot, builds a client, then only caches it if the epoch is still
    /// what it was before the snapshot load. If a concurrent `mutate()`
    /// completes in between, the epoch moved, the build is discarded, and
    /// the whole lookup retries against the now-current state. On the rare
    /// exhaustion of all attempts (sustained concurrent mutation), the last
    /// freshly built client is returned uncached rather than looping
    /// forever or ever caching a possibly-stale build.
    pub async fn provider(&self, connection_id: &str) -> AppResult<Arc<S3Provider>> {
        let mut last_built: Option<Arc<S3Provider>> = None;

        for _ in 0..MAX_PROVIDER_ATTEMPTS {
            if let Some(hit) = self.clients.lock().await.get(connection_id) {
                return Ok(Arc::clone(hit));
            }

            // Read *before* loading the snapshot below: if `invalidate_all`
            // bumps this before we reach `insert_if_epoch_matches`, the
            // snapshot may already be stale by the time we'd otherwise cache
            // a client built from it.
            let epoch_before = self.epoch.load(Ordering::SeqCst);

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

            match self
                .insert_if_epoch_matches(connection_id, epoch_before, Arc::clone(&built))
                .await
            {
                Some(cached) => return Ok(cached),
                None => {
                    // A mutate() completed its invalidate_all while we were
                    // loading the snapshot / building the client above:
                    // `built` may already sign with pre-mutation
                    // credentials. Don't cache it -- retry the whole lookup
                    // against the now-current epoch instead.
                    last_built = Some(built);
                }
            }
        }

        tracing::debug!(
            connection_id,
            attempts = MAX_PROVIDER_ATTEMPTS,
            "provider lookup hit sustained mutation pressure; returning an uncached client"
        );
        Ok(last_built.expect(
            "MAX_PROVIDER_ATTEMPTS >= 1, and every iteration that doesn't return early \
             (cache hit or epoch match) sets last_built before the next one runs",
        ))
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

    // --- Finding 1: the stale-credential repopulation race ------------------
    //
    // Two tests, deliberately different in kind -- and only one of them
    // actually pins the race:
    //
    // - `stale_snapshot_insert_is_rejected_after_a_racing_mutate_completes`
    //   deterministically drives the *exact* sequence the finding describes
    //   by calling the same private `insert_if_epoch_matches` that
    //   `provider()` uses, with a snapshot and `epoch_before` captured
    //   before an intervening `mutate()` actually runs. This is the one that
    //   unconditionally proves the guard: it doesn't depend on scheduling
    //   luck, so it can't fail to hit the window. It is the load-bearing
    //   proof that the stale-credential race is fixed.
    // - `concurrent_mutate_and_provider_lookup_stay_live_under_contention`
    //   races the real public `provider()` against `mutate()` on a genuine
    //   multi-threaded runtime, many rounds. It does **not** detect the
    //   stale-credential race and cannot be made to: its only assertion
    //   compares `Arc` pointer identity, but `from_connection`/`Arc::new`
    //   allocates a brand-new `Arc` on every build regardless of whether the
    //   `Connection` data behind it is stale or fresh, and `invalidate_all`'s
    //   `clear()` unconditionally evicts the previous entry either way -- so
    //   `!Arc::ptr_eq(previous, after)` holds whether or not the epoch guard
    //   is present. This was verified empirically: with the guard disabled
    //   (`if false && self.epoch.load(...) != epoch_before`), this test
    //   passed on all of 5 runs x 200 rounds = 1000 racing attempts, while
    //   `stale_snapshot_insert_is_rejected_after_a_racing_mutate_completes`
    //   failed immediately, as it should. What this test actually covers:
    //   hammering `provider()`'s bounded retry loop concurrently with a real
    //   `mutate()` doesn't deadlock, doesn't livelock (exhaust
    //   `MAX_PROVIDER_ATTEMPTS` and spin or panic), and doesn't panic on the
    //   `last_built.expect(...)` path -- liveness and crash-freedom under
    //   contention, not correctness of which credentials ended up cached.
    //   See the module docs' "Lock discipline" section for the mechanism the
    //   first test targets.

    #[tokio::test]
    async fn stale_snapshot_insert_is_rejected_after_a_racing_mutate_completes() {
        let dir = tempfile::tempdir().unwrap();
        let hub = hub_with(&dir);
        hub.mutate(|list| {
            list.push(sample("c1", "s1"));
            Ok(())
        })
        .await
        .unwrap();

        // Phase 1 + 2, exactly as `provider()` performs them: cache miss,
        // then read the epoch, then load the (soon to be stale) snapshot.
        assert!(hub.clients.lock().await.get("c1").is_none());
        let epoch_before = hub.epoch.load(Ordering::SeqCst);
        let stale_connection = hub
            .connections()
            .await
            .unwrap()
            .into_iter()
            .find(|c| c.id == "c1")
            .unwrap();
        assert_eq!(stale_connection.secret_access_key, "s1");

        // The race window: a full `mutate()` cycle completes entirely here,
        // rotating the secret and -- inside `invalidate_all` -- bumping the
        // epoch and clearing the cache under the same lock.
        hub.mutate(|list| {
            list[0].secret_access_key = "s2".to_string();
            Ok(())
        })
        .await
        .unwrap();

        // Phase 3: build from the now-stale snapshot (mirrors `provider()`
        // building outside the cache lock), then attempt the same guarded
        // insert `provider()` would.
        let stale_built = Arc::new(from_connection(&stale_connection).unwrap());
        let result = hub
            .insert_if_epoch_matches("c1", epoch_before, Arc::clone(&stale_built))
            .await;

        assert!(
            result.is_none(),
            "a client built from a pre-mutation snapshot must be rejected once \
             the epoch has moved, not cached"
        );
        assert!(
            hub.clients.lock().await.get("c1").is_none(),
            "the rejected stale client must not have been inserted into the cache"
        );

        // The real, public `provider()` must still work afterwards, caching
        // a fresh client rather than ever surfacing the stale one.
        let fresh = hub.provider("c1").await.unwrap();
        assert!(
            !Arc::ptr_eq(&fresh, &stale_built),
            "provider() must never return the stale pre-mutation client"
        );
    }

    // What this test does and does not prove (see the block comment above
    // for the full account): it hammers `provider()`'s bounded retry loop
    // with real concurrent `mutate()` calls across 200 rounds on a
    // multi-threaded runtime, and its passing tells you the hub stayed
    // *live* under that contention -- no deadlock, no livelock (retry
    // exhaustion spinning), no panic on the `last_built.expect(...)` path.
    // It cannot, and does not claim to, tell you whether the stale-snapshot
    // race from Finding 1 was ever actually hit: its assertion is
    // `Arc::ptr_eq` pointer-identity comparison, and every build allocates a
    // fresh `Arc` regardless of whether the data behind it is stale or
    // fresh, so the assertion holds identically with the epoch guard
    // present or removed. `stale_snapshot_insert_is_rejected_after_a_racing_mutate_completes`
    // is the test that pins that race; this one only pins liveness.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_mutate_and_provider_lookup_stay_live_under_contention() {
        let dir = tempfile::tempdir().unwrap();
        let hub = Arc::new(hub_with(&dir));
        hub.mutate(|list| {
            list.push(sample("c1", "s0"));
            Ok(())
        })
        .await
        .unwrap();

        let mut previous = hub.provider("c1").await.unwrap();

        for round in 0..200 {
            let next_secret = format!("s{}", round + 1);
            let hub_a = Arc::clone(&hub);
            let hub_b = Arc::clone(&hub);

            // Spawned (not just `.await`ed inline) so the two can genuinely
            // run on different worker threads at the same time, not merely
            // interleave cooperatively on one.
            let lookup = tokio::spawn(async move { hub_a.provider("c1").await.unwrap() });
            let mutation = tokio::spawn(async move {
                hub_b
                    .mutate(move |list| {
                        list[0].secret_access_key = next_secret;
                        Ok(())
                    })
                    .await
                    .unwrap();
            });
            let (raced, mutated) = tokio::join!(lookup, mutation);
            raced.unwrap();
            mutated.unwrap();

            // By the time `join!` resolves, this round's `mutate()` has
            // unconditionally completed (its future only resolves after
            // `invalidate_all` runs), and neither spawned task panicked
            // (the `.unwrap()`s above would have propagated that) -- so
            // reaching this line already demonstrates the round finished
            // without a deadlock, a livelock, or a panic.
            //
            // The assertion below is a weak sanity check, not evidence
            // about the race: `!Arc::ptr_eq(&previous, &after)` is true
            // because `invalidate_all` unconditionally clears the cache
            // every round and every build allocates a fresh `Arc`, so it
            // holds regardless of whether the client `after` was built from
            // stale or fresh credentials. It duplicates (under contention,
            // rather than deterministically) what `mutate_invalidates_the_cache`
            // already covers -- kept here only to confirm the cache is
            // still actually being cleared each round rather than, say,
            // silently no-op'ing under load.
            let after = hub.provider("c1").await.unwrap();
            assert!(
                !Arc::ptr_eq(&previous, &after),
                "round {round}: cache still serves the exact same Arc allocation as \
                 before this round's mutate() -- invalidate_all did not clear it"
            );
            previous = after;
        }
    }
}
