//! Which running Strom flow writes to which TAMS flow.
//!
//! TAMS Output derives its TAMS flow ids from the block instance id (or from
//! its `tams_key` when set), and a block id is only unique within one Strom
//! flow. Two Strom flows with a TAMS Output of the same block id (or key)
//! would write to the same TAMS flows on a shared gateway, interleaving
//! segments of two recordings. The ids stay derived that
//! way on purpose (changing the derivation would change the TAMS ids of every
//! existing recording), so this registry catches the collision instead: a block
//! claims its TAMS flow ids when it is built, and a claim held by another block
//! fails the build: a block of another Strom flow, or another block of the same
//! one with the same `tams_key` (a copied block keeps its properties).
//!
//! Claims are keyed by the TAMS flow id alone, whatever the gateway: the
//! segments of a TAMS flow are parked in one directory per id
//! (`crate::tams::park::flow_dir`), whose `park.json` names one gateway. Two
//! writers of one id on two gateways would share that directory and the
//! drainer would upload only one gateway's segments. The ids are UUIDs derived
//! from the block id or key, so the same id on two gateways is the same
//! recording written twice; give the copy its own TAMS key. Claims are
//! released per run of a Strom flow when that run is torn down, on stop and
//! on a failed start alike, but only once its pipeline has stopped and its
//! uploaders have finished (see [`Writer`]): until then the old run still
//! writes, and a flow that claimed the ids at once would be a second writer.
//!
//! Claims and writers belong to a [`Run`], not just to the Strom flow id: a
//! flow stopped and started again at once has a new run that takes its
//! blocks' claims over while the old run's teardown still waits for its
//! uploaders. That teardown waits only for the old run's writers and
//! releases only the claims the old run still holds, never the new run's.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex};
use std::time::Duration;
use strom_types::FlowId;
use tokio::sync::Notify;

/// One run of a Strom flow: from a start to the teardown of what it built.
/// Claims and [`Writer`]s are taken by the flow's current run (see
/// [`begin_run`]), and its teardown waits for and releases only its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Run {
    pub flow_id: FlowId,
    generation: u64,
}

/// Run generations handed out so far; 0 is "no run begun" (a bare build).
static NEXT_GENERATION: AtomicU64 = AtomicU64::new(1);

/// Strom flow -> its current run's generation.
static CURRENT_RUNS: LazyLock<Mutex<HashMap<FlowId, u64>>> = LazyLock::new(Default::default);

/// Begin a new run of Strom flow `flow_id`, before its pipeline is built:
/// the claims its blocks take and the uploaders they start from now on
/// belong to this run.
pub fn begin_run(flow_id: FlowId) -> Run {
    let generation = NEXT_GENERATION.fetch_add(1, Ordering::Relaxed);
    CURRENT_RUNS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(flow_id, generation);
    Run {
        flow_id,
        generation,
    }
}

/// The current run of Strom flow `flow_id` (generation 0 when none was
/// begun: a block built outside `start_flow`).
pub fn current_run(flow_id: FlowId) -> Run {
    let generation = CURRENT_RUNS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&flow_id)
        .copied()
        .unwrap_or(0);
    Run {
        flow_id,
        generation,
    }
}

/// The block that holds a claim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaimOwner {
    pub flow_id: FlowId,
    pub block_id: String,
    /// The gateway it writes to, without credentials.
    pub gateway: String,
    /// The run of the flow that took it last.
    pub run: Run,
}

/// A TAMS flow another Strom flow already writes to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaimConflict {
    pub tams_flow_id: String,
    pub owner: ClaimOwner,
}

/// TAMS flow id -> owner.
static CLAIMS: LazyLock<Mutex<HashMap<String, ClaimOwner>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Claim `tams_flow_ids` (written to `gateway_url`) for block `block_id` of Strom flow
/// `flow_id`, in the flow's current run. All or nothing: if any of them is
/// held by another block (of another Strom flow, or another block of this
/// one: two blocks with the same `tams_key`), nothing is claimed and the
/// first conflict is returned. Claims the same block already holds (a rebuild
/// of the flow, or a new run while the old one is still torn down) are taken
/// over by the current run. The gateway does not tell claims apart (see the
/// module docs).
pub fn claim(
    gateway_url: &str,
    tams_flow_ids: &[String],
    flow_id: FlowId,
    block_id: &str,
) -> Result<(), ClaimConflict> {
    let gateway = crate::tams::uploader::ParkTarget::new(gateway_url, "").gateway;
    let run = current_run(flow_id);
    let mut claims = CLAIMS.lock().unwrap_or_else(|e| e.into_inner());
    for id in tams_flow_ids {
        if let Some(owner) = claims.get(id) {
            if owner.flow_id != flow_id || owner.block_id != block_id {
                return Err(ClaimConflict {
                    tams_flow_id: id.clone(),
                    owner: owner.clone(),
                });
            }
        }
    }
    for id in tams_flow_ids {
        claims.insert(
            id.clone(),
            ClaimOwner {
                flow_id,
                block_id: block_id.to_string(),
                gateway: gateway.clone(),
                run,
            },
        );
    }
    Ok(())
}

/// Release the claims run `run` still holds: not those a later run of the
/// same flow has taken over. Called when the run is torn down, after
/// [`writers_done`]; a run that holds none is ignored.
pub fn release_run(run: Run) {
    let mut claims = CLAIMS.lock().unwrap_or_else(|e| e.into_inner());
    claims.retain(|_, owner| owner.run != run);
    drop(claims);
    let mut runs = CURRENT_RUNS.lock().unwrap_or_else(|e| e.into_inner());
    if runs.get(&run.flow_id) == Some(&run.generation) {
        runs.remove(&run.flow_id);
    }
}

/// Uploaders still running, per run.
static WRITERS: LazyLock<Mutex<HashMap<Run, usize>>> = LazyLock::new(Default::default);

/// Woken whenever a [`Writer`] ends.
static WRITER_ENDED: Notify = Notify::const_new();

/// A task that writes to the TAMS flows of Strom flow `flow_id`: a TAMS
/// Output's uploader, which keeps uploading its queue and the final segment
/// after the pipeline has stopped. Counted under the flow's current run
/// while it lives.
pub struct Writer(Run);

impl Writer {
    pub fn new(flow_id: FlowId) -> Self {
        let run = current_run(flow_id);
        *WRITERS
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(run)
            .or_insert(0) += 1;
        Writer(run)
    }
}

impl Drop for Writer {
    fn drop(&mut self) {
        let mut writers = WRITERS.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(n) = writers.get_mut(&self.0) {
            *n -= 1;
            if *n == 0 {
                writers.remove(&self.0);
            }
        }
        drop(writers);
        WRITER_ENDED.notify_waiters();
    }
}

fn writers_of(run: Run) -> usize {
    WRITERS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&run)
        .copied()
        .unwrap_or(0)
}

/// Wait until no [`Writer`] of run `run` runs, at most `max`. Writers of a
/// later run of the same flow do not count. False if some still ran then
/// (an uploader stuck on the gateway, or held by a pipeline that was never
/// freed).
pub async fn writers_done(run: Run, max: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + max;
    loop {
        let ended = WRITER_ENDED.notified();
        tokio::pin!(ended);
        // Registered before the count is read, so an end in between wakes it.
        ended.as_mut().enable();
        if writers_of(run) == 0 {
            return true;
        }
        if tokio::time::timeout_at(deadline, ended).await.is_err() {
            return writers_of(run) == 0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    fn ids(block: &str) -> Vec<String> {
        vec![format!("{block}-video"), format!("{block}-mux")]
    }

    /// Release what flow `flow_id`'s current run holds (a teardown).
    fn release(flow_id: FlowId) {
        release_run(current_run(flow_id));
    }

    /// A flow stopped and started again at once: the new run takes its
    /// block's claims over and starts its own uploader while the old run's
    /// teardown still waits for the old uploader. That teardown must neither
    /// wait for the new run's writer nor release the new run's claims, or
    /// another flow could take the TAMS ids and be a second writer.
    #[tokio::test]
    async fn teardown_of_an_old_run_leaves_the_new_run_alone() {
        let gw = format!("http://gw-{}.example.com", Uuid::new_v4().simple());
        let block = format!("cam-{}", Uuid::new_v4().simple());
        let (a, b) = (Uuid::new_v4(), Uuid::new_v4());

        let old = begin_run(a);
        claim(&gw, &ids(&block), a, &block).unwrap();
        let old_writer = Writer::new(a);

        // Restart: the old pipeline is gone, its uploader still flushes.
        let new = begin_run(a);
        claim(&gw, &ids(&block), a, &block).unwrap();
        let _new_writer = Writer::new(a);

        let teardown = tokio::spawn(async move {
            let done = writers_done(old, Duration::from_secs(5)).await;
            release_run(old);
            done
        });
        drop(old_writer);
        let done = tokio::time::timeout(Duration::from_secs(1), teardown)
            .await
            .expect("the old teardown does not wait for the new run's writer")
            .unwrap();
        assert!(done, "the old run's writers have all ended");

        // The new run still holds the ids.
        let conflict = claim(&gw, &ids(&block), b, &block).unwrap_err();
        assert_eq!(conflict.owner.flow_id, a);
        assert_eq!(conflict.owner.run, new);
        assert_eq!(current_run(a), new);

        release_run(new);
        claim(&gw, &ids(&block), b, &block).unwrap();
        release(b);
    }

    #[test]
    fn tams_claim_held_by_another_flow_fails_until_released() {
        let gw = format!("http://gw-{}.example.com", Uuid::new_v4().simple());
        let (a, b) = (Uuid::new_v4(), Uuid::new_v4());

        claim(&gw, &ids("cam1"), a, "cam1").unwrap();
        // A rebuild of the same flow takes its own claims over.
        claim(&gw, &ids("cam1"), a, "cam1").unwrap();

        let conflict = claim(&format!("{gw}/"), &ids("cam1"), b, "cam1").unwrap_err();
        assert_eq!(conflict.owner.flow_id, a);
        assert_eq!(conflict.owner.block_id, "cam1");

        release(a);
        claim(&gw, &ids("cam1"), b, "cam1").unwrap();
        release(b);
    }

    /// Two blocks of one Strom flow with the same `tams_key` derive the same
    /// TAMS flow ids: the second is refused, as from another Strom flow.
    #[test]
    fn tams_claim_held_by_another_block_of_the_same_flow_fails() {
        let gw = format!("http://gw-{}.example.com", Uuid::new_v4().simple());
        let a = Uuid::new_v4();
        claim(&gw, &ids("key"), a, "cam1").unwrap();
        let conflict = claim(&gw, &ids("key"), a, "cam1_copy").unwrap_err();
        assert_eq!(conflict.owner.flow_id, a);
        assert_eq!(conflict.owner.block_id, "cam1");
        // The block that holds them may build again.
        claim(&gw, &ids("key"), a, "cam1").unwrap();
        release(a);
    }

    /// The same TAMS flow ids on another gateway share the park directory of
    /// those ids: refused like on the same gateway.
    #[test]
    fn tams_claim_holds_across_gateways() {
        let gw = format!("http://gw-{}.example.com", Uuid::new_v4().simple());
        let other = format!("http://u:p@other-{}.example.com", Uuid::new_v4().simple());
        let block = format!("cam-{}", Uuid::new_v4().simple());
        let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
        claim(&gw, &ids(&block), a, &block).unwrap();
        let conflict = claim(&other, &ids(&block), b, &block).unwrap_err();
        assert_eq!(conflict.owner.flow_id, a);
        assert_eq!(conflict.owner.gateway, gw);
        release(a);
        claim(&other, &ids(&block), b, &block).unwrap();
        // The gateway is kept without its credentials.
        let conflict = claim(&gw, &ids(&block), a, &block).unwrap_err();
        assert!(!conflict.owner.gateway.contains("u:p"));
        release(b);
    }

    #[tokio::test]
    async fn writers_done_waits_for_the_last_writer() {
        let a = Uuid::new_v4();
        assert!(
            writers_done(current_run(a), Duration::ZERO).await,
            "no writer"
        );
        let (w1, w2) = (Writer::new(a), Writer::new(a));
        // Another flow's writer does not count.
        let _other = Writer::new(Uuid::new_v4());
        assert!(!writers_done(current_run(a), Duration::from_millis(20)).await);
        let wait =
            tokio::spawn(async move { writers_done(current_run(a), Duration::from_secs(5)).await });
        drop(w1);
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(!wait.is_finished(), "one writer still runs");
        drop(w2);
        assert!(wait.await.unwrap());
    }

    #[test]
    fn tams_claim_is_all_or_nothing() {
        let gw = format!("http://gw-{}.example.com", Uuid::new_v4().simple());
        let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
        claim(&gw, &["x-mux".to_string()], a, "x").unwrap();
        let wanted = vec!["x-video".to_string(), "x-mux".to_string()];
        assert!(claim(&gw, &wanted, b, "x").is_err());
        release(a);
        // b took nothing in the failed claim, so x-video is free for a third flow.
        let c = Uuid::new_v4();
        claim(&gw, &["x-video".to_string()], c, "x").unwrap();
        release(c);
    }
}
