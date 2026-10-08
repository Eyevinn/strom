//! ProRes 4444 as a stinger clip, in a binary of its own: on macOS it
//! decodes through VideoToolbox, which shares GL with the process, and a GPU
//! flow being torn down in a neighbouring test can stall it.

pub mod common;
#[path = "common/stinger.rs"]
pub mod rig;

use gstreamer::prelude::*;
use rig::*;

/// A ProRes 4444 graphic keeps its alpha on the way to the GPU mixer: the
/// program shows through its transparent half, and every frame arrives.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial_test::serial(stinger)]
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

/// The stinger source moves between formats from clip to clip: a PNG clip
/// (RGBA), then ProRes 4444 (10- or 16-bit 4:4:4 with alpha), then the PNG
/// clip again. The source also feeds an ordinary mixer input, whose GL upload
/// does not take ProRes's format as it is; that input adapts, and each clip's
/// graphic airs with every frame, whichever came before it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial_test::serial(stinger)]
async fn a_prores_clip_after_another_format_still_airs() {
    if !common::gl_available(GL_ELEMENTS)
        || !common::plugins_available(CODEC_ELEMENTS)
        || !common::plugins_available(&["avenc_prores_ks", "avdec_prores"])
    {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let png = dir.path().join("classic.mov");
    let prores = dir.path().join("prores.mov");
    classic_clip(&png);
    prores_classic_clip(&prores);
    let r = start_edited(
        "switch",
        "gpu",
        dir,
        vec![png, prores],
        stinger_also_on_an_input,
    )
    .await;

    classic_take_of(&r, 0, Colour::Red, Colour::Blue).await;
    r.wait_until_parked().await;
    classic_take_of(&r, 1, Colour::Blue, Colour::Red).await;
    r.wait_until_parked().await;
    classic_take_of(&r, 0, Colour::Red, Colour::Blue).await;
    r.state.stop_flow(&r.flow_id).await.unwrap();
}

/// [`a_prores_clip_after_another_format_still_airs`] the other way round:
/// ProRes first, then the PNG clip, then ProRes again.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial_test::serial(stinger)]
async fn another_format_after_a_prores_clip_still_airs() {
    if !common::gl_available(GL_ELEMENTS)
        || !common::plugins_available(CODEC_ELEMENTS)
        || !common::plugins_available(&["avenc_prores_ks", "avdec_prores"])
    {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let prores = dir.path().join("prores.mov");
    let png = dir.path().join("classic.mov");
    prores_classic_clip(&prores);
    classic_clip(&png);
    let r = start_edited(
        "switchback",
        "gpu",
        dir,
        vec![prores, png],
        stinger_also_on_an_input,
    )
    .await;

    classic_take_of(&r, 0, Colour::Red, Colour::Blue).await;
    r.wait_until_parked().await;
    classic_take_of(&r, 1, Colour::Blue, Colour::Red).await;
    r.wait_until_parked().await;
    classic_take_of(&r, 0, Colour::Red, Colour::Blue).await;
    r.state.stop_flow(&r.flow_id).await.unwrap();
}

/// A consumer that cannot take a clip's format stops the stinger source's
/// output. The next cue of a clip it can take starts the output again: that
/// clip airs with every frame, not only after a flow restart.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial_test::serial(stinger)]
async fn the_stinger_source_recovers_after_a_clip_a_consumer_refused() {
    if !common::gl_available(GL_ELEMENTS)
        || !common::plugins_available(CODEC_ELEMENTS)
        || !common::plugins_available(&["avenc_prores_ks", "avdec_prores"])
    {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let png = dir.path().join("classic.mov");
    let prores = dir.path().join("prores.mov");
    classic_clip(&png);
    prores_classic_clip(&prores);
    // Beside the mixer, a consumer that takes RGBA only and cannot adapt:
    // the PNG clip passes, ProRes is refused.
    let r = start_edited("recover", "gpu", dir, vec![png, prores], |flow| {
        let elem = |id: &str, ty: &str, props: Vec<(&str, strom_types::PropertyValue)>| {
            strom_types::Element {
                id: id.to_string(),
                element_type: ty.to_string(),
                properties: props.into_iter().map(|(k, v)| (k.to_string(), v)).collect(),
                position: [0.0, 0.0].into(),
                pad_properties: Default::default(),
            }
        };
        use strom_types::PropertyValue as PV;
        flow.elements.push(elem(
            "only_rgba",
            "capsfilter",
            vec![("caps", PV::String("video/x-raw,format=RGBA".into()))],
        ));
        flow.elements.push(elem(
            "rgba_sink",
            "fakesink",
            vec![("sync", PV::Bool(false)), ("async", PV::Bool(false))],
        ));
        for (from, to) in [
            ("sting-recover:video_out", "only_rgba:sink"),
            ("only_rgba:src", "rgba_sink:sink"),
        ] {
            flow.links.push(strom_types::element::Link {
                from: from.into(),
                to: to.into(),
            });
        }
    })
    .await;

    classic_take_of(&r, 0, Colour::Red, Colour::Blue).await;
    r.wait_until_parked().await;
    // Refused by the RGBA-only consumer, which stops the source's output.
    r.state
        .stinger_cue(&r.flow_id, &r.mixer(), 1, None)
        .await
        .expect("cue the ProRes clip");
    let output = {
        let pipelines = r.state.pipelines_read().await;
        pipelines
            .get(&r.flow_id)
            .and_then(|m| m.pipeline().by_name("sting-recover:appsrc_video"))
            .and_then(|e| e.static_pad("src"))
            .expect("the stinger source's video appsrc")
    };
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    // A source that stops on an error sends EOS on its way out.
    while output.last_flow_result() != Err(gstreamer::FlowError::Eos) {
        assert!(
            std::time::Instant::now() < deadline,
            "the RGBA-only consumer never refused the ProRes clip: {:?}",
            output.last_flow_result()
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    // The PNG clip airs again, every frame.
    classic_take_of(&r, 0, Colour::Blue, Colour::Red).await;
    r.state.stop_flow(&r.flow_id).await.unwrap();
}
