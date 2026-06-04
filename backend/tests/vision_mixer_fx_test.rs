//! End-to-end test for the vision mixer shader FX engine.
//!
//! Builds a real vision mixer flow on the GPU (OpenGL) backend, starts it,
//! and exercises the FX surface: FX slot presence, applying looks (input +
//! master), shader wipe takes and master-FX takes. Runs on software GL
//! (llvmpipe), so it works in CI — skips when GL is unavailable.

use std::collections::HashMap;
use strom::blocks::BlockRegistry;
use strom::events::EventBroadcaster;
use strom::gst::pipeline::PipelineManager;
use strom_types::effects::{EffectTarget, VideoEffect};
use strom_types::Flow;
use tempfile::NamedTempFile;

const BLOCK_ID: &str = "vmfx";

/// A flow with a single vision mixer block forced onto the GPU backend.
/// Inputs are left unlinked — force-live compositors output regardless.
fn build_vm_flow() -> Flow {
    let mut flow = Flow::new("vm_fx_test");
    flow.blocks.push(strom_types::BlockInstance {
        id: BLOCK_ID.to_string(),
        block_definition_id: "builtin.vision_mixer".to_string(),
        name: None,
        properties: {
            let mut p = HashMap::new();
            p.insert(
                "compositor_preference".to_string(),
                strom_types::PropertyValue::String("gpu".to_string()),
            );
            p.insert(
                "num_inputs".to_string(),
                strom_types::PropertyValue::UInt(2),
            );
            p
        },
        position: strom_types::block::Position { x: 100.0, y: 100.0 },
        runtime_data: None,
        computed_external_pads: None,
    });
    flow
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn vision_mixer_fx_engine_end_to_end() {
    gstreamer::init().unwrap();

    if gstreamer::ElementFactory::find("glvideomixerelement").is_none()
        || gstreamer::ElementFactory::find("glshader").is_none()
    {
        eprintln!("SKIP: GStreamer GL elements not available");
        return;
    }

    let temp_file = NamedTempFile::new().unwrap();
    let registry = BlockRegistry::new(temp_file.path());
    let events = EventBroadcaster::new(10);
    let media_path = std::env::temp_dir();

    let flow = build_vm_flow();

    let mut manager = match PipelineManager::new(
        &flow,
        events,
        &registry,
        vec![],
        "all".to_string(),
        None,
        media_path,
        std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
    ) {
        Ok(m) => m,
        Err(e) => {
            // GL context creation can fail in truly headless environments.
            eprintln!("SKIP: could not build GPU pipeline ({})", e);
            return;
        }
    };

    if let Err(e) = manager.start() {
        eprintln!("SKIP: could not start GPU pipeline ({})", e);
        return;
    }

    // Let the pipeline roll a few frames.
    tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;

    // FX engine must be detected on the GPU path with default enable_fx.
    assert!(
        manager.vision_mixer_fx_available(BLOCK_ID),
        "FX slots missing from GPU pipeline"
    );

    // Apply a look to input 0 and a master look — both must succeed and
    // report the clamped effect back.
    let applied = manager
        .set_vision_mixer_effect(
            BLOCK_ID,
            EffectTarget::Input(0),
            &VideoEffect::Pixelate { block_size: 9999.0 },
        )
        .expect("input look failed");
    assert_eq!(applied, VideoEffect::Pixelate { block_size: 200.0 });

    manager
        .set_vision_mixer_effect(
            BLOCK_ID,
            EffectTarget::Master,
            &VideoEffect::Vignette {
                amount: 0.5,
                softness: 0.5,
            },
        )
        .expect("master look failed");

    // Param-only change on the same kind (uniform swap path).
    manager
        .set_vision_mixer_effect(
            BLOCK_ID,
            EffectTarget::Input(0),
            &VideoEffect::Pixelate { block_size: 32.0 },
        )
        .expect("param-only update failed");

    // Invalid color must be rejected.
    assert!(manager
        .set_vision_mixer_effect(
            BLOCK_ID,
            EffectTarget::Input(1),
            &VideoEffect::ChromaKey {
                key_color: "green".to_string(),
                similarity: 0.3,
                smoothness: 0.1,
                spill: 0.5,
            },
        )
        .is_err());

    // Out-of-range input must be rejected (no such FX slot).
    assert!(manager
        .set_vision_mixer_effect(BLOCK_ID, EffectTarget::Input(7), &VideoEffect::None)
        .is_err());

    // Shader wipe take and master-FX take must run without error.
    manager
        .trigger_transition(BLOCK_ID, 0, 1, "wipe_left", 200)
        .expect("wipe take failed");
    tokio::time::sleep(tokio::time::Duration::from_millis(300)).await;

    manager
        .trigger_transition(BLOCK_ID, 1, 0, "glitch_cut", 200)
        .expect("glitch take failed");
    tokio::time::sleep(tokio::time::Duration::from_millis(300)).await;

    // The pipeline must still be alive and rolling after the FX work —
    // a shader compile failure would have posted an error and torn it down.
    use gstreamer::prelude::ElementExtManual;
    assert_eq!(
        manager.pipeline().current_state(),
        gstreamer::State::Playing,
        "pipeline died during FX"
    );

    manager.stop().expect("stop failed");
    drop(manager);
}
