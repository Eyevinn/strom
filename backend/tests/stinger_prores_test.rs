//! ProRes 4444 as a stinger clip, in a binary of its own: on macOS it
//! decodes through VideoToolbox, which shares GL with the process, and a GPU
//! flow being torn down in a neighbouring test can stall it.

pub mod common;
#[path = "common/stinger.rs"]
pub mod rig;

use rig::*;

/// A ProRes 4444 graphic keeps its alpha on the way to the GPU mixer: the
/// program shows through its transparent half, and every frame arrives.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn prores_4444_keeps_its_alpha() {
    if !common::gl_available(GL_ELEMENTS)
        || !common::plugins_available(&["avenc_prores_ks", "avdec_prores", "qtmux", "qtdemux"])
    {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let clip = dir.path().join("classic.mov");
    prores_classic_clip(&clip);
    let r = start_with("prores", "gpu", dir, vec![clip]).await;
    let s = r.state.stinger_state(&r.flow_id, &r.mixer()).await.unwrap();
    assert!(
        s.clips[0].info.as_ref().unwrap().has_alpha,
        "the clip decodes with its alpha"
    );
    classic_take(&r).await;
    r.state.stop_flow(&r.flow_id).await.unwrap();
}
