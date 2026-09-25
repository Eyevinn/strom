//! Port pool configuration, persistence, and reservation operations.

use super::AppState;
use crate::ports::{PortPool, PortPoolError, PortReservationStore, ReservationOutcome};
use chrono::Utc;
use std::collections::HashSet;
use strom_types::ports::{PortPoolStatus, PortReservation};
use strom_types::FlowId;
use tracing::{debug, info, warn};
use uuid::Uuid;

impl AppState {
    /// Configure the port pool. Called once from main after the
    /// configuration is loaded; persisted reservations are kept either way.
    pub async fn configure_port_pool(
        &self,
        ports: std::collections::BTreeSet<u16>,
        store: PortReservationStore,
        default_ttl: u64,
        probe: bool,
    ) -> anyhow::Result<()> {
        // Load before changing any state: a failed startup must leave the
        // existing reservations file and the pool untouched.
        let reservations = store.load().await?;
        *self.inner.port_lease_ttl.lock() = default_ttl;
        *self.inner.port_probe.lock() = probe;
        {
            let mut pool = self.inner.port_pool.write().await;
            pool.set_ports(ports.iter().copied());
            pool.load(&reservations);
        }
        *self.inner.port_store.write().await = Some(store);
        if ports.is_empty() {
            debug!("Port pool disabled (no ports.ports / STROM_PORTS); /api/ports/reservations answers 503");
        } else {
            info!(
                "Port pool enabled with {} ports ({}), reservations expire after {}s, bind probe {}",
                ports.len(),
                strom_types::ports::spans(ports.iter().copied())
                    .iter()
                    .map(|s| if s.first == s.last {
                        s.first.to_string()
                    } else {
                        format!("{}-{}", s.first, s.last)
                    })
                    .collect::<Vec<_>>()
                    .join(","),
                default_ttl,
                if probe { "on" } else { "off" },
            );
            if !reservations.is_empty() {
                info!("Loaded {} port reservations from disk", reservations.len());
            }
        }
        Ok(())
    }

    /// Ids of every flow that still exists, for reconciling associations.
    async fn live_flow_ids(&self) -> HashSet<FlowId> {
        self.inner.flows.read().await.keys().copied().collect()
    }

    /// Persist the pool's reservations. A no-op before main configures a store.
    async fn save_port_reservations(&self, pool: &PortPool) -> anyhow::Result<()> {
        if let Some(store) = self.inner.port_store.read().await.as_ref() {
            store.save(&pool.snapshot()).await?;
        }
        Ok(())
    }

    /// The pool and everything in it that is not free. Answers on an
    /// unconfigured server too, which is what makes it worth asking.
    pub async fn port_pool_status(&self) -> PortPoolStatus {
        let live = self.live_flow_ids().await;
        let mut pool = self.inner.port_pool.write().await;
        pool.reconcile(&live, Utc::now());
        pool.status(Utc::now())
    }

    /// Every live reservation.
    ///
    /// Reconciles first, like every other way into the pool: an association
    /// whose flow is gone must never be observable, or a caller reads a port
    /// as in use by a flow that no longer exists.
    pub async fn list_port_reservations(&self) -> Result<Vec<PortReservation>, PortPoolError> {
        let live = self.live_flow_ids().await;
        let mut pool = self.inner.port_pool.write().await;
        pool.reconcile(&live, Utc::now());
        pool.list(Utc::now())
    }

    /// One live reservation.
    pub async fn get_port_reservation(&self, id: Uuid) -> Result<PortReservation, PortPoolError> {
        let live = self.live_flow_ids().await;
        let mut pool = self.inner.port_pool.write().await;
        pool.reconcile(&live, Utc::now());
        pool.get(id, Utc::now())
    }

    /// Grant or grow the reservation for `owner_id`.
    pub async fn reserve_ports(
        &self,
        owner_id: &str,
        count: u16,
        ttl_secs: Option<u64>,
    ) -> anyhow::Result<Result<ReservationOutcome, PortPoolError>> {
        let live = self.live_flow_ids().await;
        let default_ttl = *self.inner.port_lease_ttl.lock();
        let probing = *self.inner.port_probe.lock();
        let mut pool = self.inner.port_pool.write().await;
        pool.reconcile(&live, Utc::now());
        let mut candidate = pool.clone();
        let mut probe = |port: u16| !probing || crate::ports::pool::port_is_free_on_host(port);
        let outcome = candidate.reserve(
            owner_id,
            count,
            ttl_secs,
            default_ttl,
            Utc::now(),
            &mut probe,
        );
        let newly_blocked = match &outcome {
            Ok(outcome) => outcome.newly_blocked.as_slice(),
            Err(PortPoolError::Exhausted { newly_blocked, .. }) => newly_blocked.as_slice(),
            Err(_) => &[],
        };
        for port in newly_blocked {
            warn!(
                "Port {port} is held by something outside Strom and will not be handed out; \
                 remove it from the pool or free it on the host"
            );
        }
        if let Ok(outcome) = &outcome {
            self.save_port_reservations(&candidate).await?;
            *pool = candidate;
            info!(
                "Port reservation {} for '{}': {} ports ({:?})",
                outcome.reservation.id,
                outcome.reservation.owner_id,
                outcome.reservation.ports.len(),
                outcome.how
            );
        } else if matches!(outcome, Err(PortPoolError::Exhausted { .. })) {
            // A failed allocation does not change reservations, but does keep
            // the bind-probe results so status and later probes stay accurate.
            *pool = candidate;
        }
        Ok(outcome)
    }

    /// Extend a reservation.
    pub async fn renew_port_reservation(
        &self,
        id: Uuid,
        ttl_secs: Option<u64>,
    ) -> anyhow::Result<Result<PortReservation, PortPoolError>> {
        let default_ttl = *self.inner.port_lease_ttl.lock();
        let live = self.live_flow_ids().await;
        let mut pool = self.inner.port_pool.write().await;
        pool.reconcile(&live, Utc::now());
        let mut candidate = pool.clone();
        let outcome = candidate.renew(id, ttl_secs, default_ttl, Utc::now());
        if outcome.is_ok() {
            self.save_port_reservations(&candidate).await?;
            *pool = candidate;
        }
        Ok(outcome)
    }

    /// Give a reservation back.
    pub async fn release_port_reservation(
        &self,
        id: Uuid,
    ) -> anyhow::Result<Result<(), PortPoolError>> {
        let live = self.live_flow_ids().await;
        let mut pool = self.inner.port_pool.write().await;
        pool.reconcile(&live, Utc::now());
        let mut candidate = pool.clone();
        let outcome = candidate.release(id, Utc::now());
        if outcome.is_ok() {
            self.save_port_reservations(&candidate).await?;
            *pool = candidate;
            info!("Port reservation {id} released");
        }
        Ok(outcome)
    }

    /// Record which of a reservation's ports a flow uses.
    pub async fn assign_ports(
        &self,
        id: Uuid,
        flow_id: FlowId,
        ports: &[u16],
    ) -> anyhow::Result<Result<PortReservation, PortPoolError>> {
        let live = self.live_flow_ids().await;
        let mut pool = self.inner.port_pool.write().await;
        pool.reconcile(&live, Utc::now());
        let mut candidate = pool.clone();
        let outcome = candidate.assign(id, flow_id, ports, Utc::now());
        if outcome.is_ok() {
            self.save_port_reservations(&candidate).await?;
            *pool = candidate;
        }
        Ok(outcome)
    }

    /// Drop a flow's declaration.
    pub async fn unassign_ports(
        &self,
        id: Uuid,
        flow_id: FlowId,
    ) -> anyhow::Result<Result<(), PortPoolError>> {
        let live = self.live_flow_ids().await;
        let mut pool = self.inner.port_pool.write().await;
        pool.reconcile(&live, Utc::now());
        let mut candidate = pool.clone();
        let outcome = candidate.unassign(id, flow_id, Utc::now());
        if outcome.is_ok() {
            self.save_port_reservations(&candidate).await?;
            *pool = candidate;
        }
        Ok(outcome)
    }
}

#[cfg(test)]
mod port_pool_persistence_tests {
    use super::*;
    use crate::storage::JsonFileStorage;
    use std::collections::BTreeSet;
    use strom_types::Flow;
    use tempfile::TempDir;

    fn new_state(temp_dir: &TempDir) -> AppState {
        gstreamer::init().expect("gstreamer init failed in test");
        AppState::new(
            JsonFileStorage::new(temp_dir.path().join("flows.json")),
            temp_dir.path().join("blocks.json"),
            temp_dir.path().join("media"),
            vec![],
            "all".to_string(),
            vec![],
        )
    }

    async fn snapshot(state: &AppState) -> Vec<PortReservation> {
        state.inner.port_pool.read().await.snapshot()
    }

    #[tokio::test]
    async fn malformed_reservations_fail_configuration_without_overwriting_the_file() {
        let temp_dir = TempDir::new().unwrap();
        let state = new_state(&temp_dir);
        let store = PortReservationStore::new(temp_dir.path());

        for contents in ["", "   ", "{", r#"{"version":1,"reservations":["#] {
            tokio::fs::write(store.path(), contents).await.unwrap();
            let err = state
                .configure_port_pool(BTreeSet::from([47100]), store.clone(), 600, false)
                .await
                .unwrap_err();
            assert!(format!("{err:#}").contains("parsing"));
            assert!(!state.port_pool_status().await.enabled);
            assert!(state.inner.port_store.read().await.is_none());
            assert!(matches!(
                state.reserve_ports("owner", 1, None).await.unwrap(),
                Err(PortPoolError::NotConfigured)
            ));
            assert_eq!(
                tokio::fs::read_to_string(store.path()).await.unwrap(),
                contents
            );
        }
    }

    #[tokio::test]
    async fn unreadable_reservations_fail_configuration() {
        let temp_dir = TempDir::new().unwrap();
        let state = new_state(&temp_dir);
        let store = PortReservationStore::new(temp_dir.path());
        // A directory at the file path gives a read error on every platform,
        // including when the test runs as root.
        tokio::fs::create_dir(store.path()).await.unwrap();
        let err = state
            .configure_port_pool(BTreeSet::from([47100]), store.clone(), 600, false)
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("reading"));
        assert!(!state.port_pool_status().await.enabled);
        assert!(state.inner.port_store.read().await.is_none());
        assert!(store.path().is_dir());
    }

    #[tokio::test]
    async fn failed_persistence_rolls_back_every_reservation_mutation() {
        let temp_dir = TempDir::new().unwrap();
        let state = new_state(&temp_dir);
        state
            .configure_port_pool(
                BTreeSet::from([47100, 47101, 47102]),
                PortReservationStore::new(temp_dir.path().join("valid-store")),
                600,
                false,
            )
            .await
            .unwrap();

        let reservation = state
            .reserve_ports("owner", 2, None)
            .await
            .unwrap()
            .unwrap()
            .reservation;
        let flow = Flow::new("live flow");
        let flow_id = flow.id;
        state.inner.flows.write().await.insert(flow_id, flow);
        state
            .assign_ports(reservation.id, flow_id, &[47100])
            .await
            .unwrap()
            .unwrap();
        let before = snapshot(&state).await;

        // Making the would-be store directory a regular file forces every
        // atomic save to fail before it can replace the persisted state.
        let blocker = temp_dir.path().join("not-a-directory");
        tokio::fs::write(&blocker, "block store creation")
            .await
            .unwrap();
        *state.inner.port_store.write().await = Some(PortReservationStore::new(&blocker));

        assert!(state.reserve_ports("other", 1, None).await.is_err());
        assert_eq!(snapshot(&state).await, before);

        assert!(state
            .renew_port_reservation(reservation.id, Some(1_200))
            .await
            .is_err());
        assert_eq!(snapshot(&state).await, before);

        assert!(state
            .assign_ports(reservation.id, flow_id, &[47101])
            .await
            .is_err());
        assert_eq!(snapshot(&state).await, before);

        assert!(state.unassign_ports(reservation.id, flow_id).await.is_err());
        assert_eq!(snapshot(&state).await, before);

        assert!(state
            .release_port_reservation(reservation.id)
            .await
            .is_err());
        assert_eq!(snapshot(&state).await, before);
    }
}
