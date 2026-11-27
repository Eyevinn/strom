//! Which running Strom flow writes to which TAMS flow.
//!
//! TAMS Output derives its TAMS flow ids from the block instance id (or from
//! its `tams_key` when set), and a block id is only unique within one Strom
//! flow. Two Strom flows with a TAMS Output of the same block id (or key)
//! would write to the same TAMS flows on a shared gateway, interleaving
//! segments of two recordings. The ids stay derived that
//! way on purpose (changing the derivation would change the TAMS ids of every
//! existing recording), so this registry catches the collision instead: a block
//! claims its TAMS flow ids when it is built, and a claim held by another Strom
//! flow fails the build.
//!
//! Claims are keyed by gateway (a TAMS flow id on another store is another
//! flow) and released per Strom flow when the flow is torn down, on stop and
//! on a failed start alike.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use strom_types::FlowId;

/// The block that holds a claim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaimOwner {
    pub flow_id: FlowId,
    pub block_id: String,
}

/// A TAMS flow another Strom flow already writes to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaimConflict {
    pub tams_flow_id: String,
    pub owner: ClaimOwner,
}

/// (gateway, TAMS flow id) -> owner.
static CLAIMS: LazyLock<Mutex<HashMap<(String, String), ClaimOwner>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// The gateway as a registry key: trimmed, without trailing slashes, so
/// `https://gw/` and `https://gw` are one store.
fn gateway_key(gateway_url: &str) -> String {
    gateway_url.trim().trim_end_matches('/').to_string()
}

/// Claim `tams_flow_ids` on `gateway_url` for block `block_id` of Strom flow
/// `flow_id`. All or nothing: if any of them is held by another Strom flow,
/// nothing is claimed and the first conflict is returned. Claims the same Strom
/// flow already holds (a rebuild of the flow) are taken over.
pub fn claim(
    gateway_url: &str,
    tams_flow_ids: &[String],
    flow_id: FlowId,
    block_id: &str,
) -> Result<(), ClaimConflict> {
    let gateway = gateway_key(gateway_url);
    let mut claims = CLAIMS.lock().unwrap_or_else(|e| e.into_inner());
    for id in tams_flow_ids {
        if let Some(owner) = claims.get(&(gateway.clone(), id.clone())) {
            if owner.flow_id != flow_id {
                return Err(ClaimConflict {
                    tams_flow_id: id.clone(),
                    owner: owner.clone(),
                });
            }
        }
    }
    for id in tams_flow_ids {
        claims.insert(
            (gateway.clone(), id.clone()),
            ClaimOwner {
                flow_id,
                block_id: block_id.to_string(),
            },
        );
    }
    Ok(())
}

/// Release every claim held by Strom flow `flow_id`. Called when the flow is
/// torn down; a flow that holds none is ignored.
pub fn release_flow(flow_id: &FlowId) {
    let mut claims = CLAIMS.lock().unwrap_or_else(|e| e.into_inner());
    claims.retain(|_, owner| owner.flow_id != *flow_id);
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    fn ids(block: &str) -> Vec<String> {
        vec![format!("{block}-video"), format!("{block}-mux")]
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

        // Another gateway is another store.
        let other = format!("http://other-{}.example.com", Uuid::new_v4().simple());
        claim(&other, &ids("cam1"), b, "cam1").unwrap();

        release_flow(&a);
        claim(&gw, &ids("cam1"), b, "cam1").unwrap();
        release_flow(&b);
    }

    #[test]
    fn tams_claim_is_all_or_nothing() {
        let gw = format!("http://gw-{}.example.com", Uuid::new_v4().simple());
        let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
        claim(&gw, &["x-mux".to_string()], a, "x").unwrap();
        let wanted = vec!["x-video".to_string(), "x-mux".to_string()];
        assert!(claim(&gw, &wanted, b, "x").is_err());
        release_flow(&a);
        // b took nothing in the failed claim, so x-video is free for a third flow.
        let c = Uuid::new_v4();
        claim(&gw, &["x-video".to_string()], c, "x").unwrap();
        release_flow(&c);
    }
}
