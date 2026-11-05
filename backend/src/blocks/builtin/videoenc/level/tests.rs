use super::super::tests::require_element;
use super::super::{videoenc_definition, Codec, VideoEncBuilder};
use super::*;
use crate::blocks::{BlockBuildContext, BlockBuilder};
use gstreamer_app as gst_app;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use strom_types::{block::PropertyType, PropertyValue};

fn format(width: u32, height: u32, fps: u32) -> InputFormat {
    InputFormat {
        width,
        height,
        fps: Some((fps, 1)),
    }
}

#[test]
fn auto_never_goes_below_4_2() {
    assert_eq!(auto_level(format(640, 360, 25)), Some(H264Level::L4_2));
    assert_eq!(auto_level(format(1920, 1080, 50)), Some(H264Level::L4_2));
    assert_eq!(auto_level(format(1920, 1080, 60)), Some(H264Level::L4_2));
}

#[test]
fn auto_picks_5_1_for_4k30_and_5_2_for_4k60() {
    assert_eq!(auto_level(format(3840, 2160, 30)), Some(H264Level::L5_1));
    assert_eq!(auto_level(format(3840, 2160, 60)), Some(H264Level::L5_2));
    // 8K is above every level auto offers: left unpinned.
    assert_eq!(auto_level(format(7680, 4320, 30)), None);
}

#[test]
fn fractional_rates_count_macroblocks_per_second() {
    // 1080p59.94 still fits 4.2 (8160 MB * 59.94 < 522240).
    let f = InputFormat {
        width: 1920,
        height: 1080,
        fps: Some((60000, 1001)),
    };
    assert_eq!(auto_level(f), Some(H264Level::L4_2));
    // 1080p30 fits 4.1, 1080p50 does not.
    assert!(covers(H264Level::L4_1, format(1920, 1080, 30)));
    assert!(!covers(H264Level::L4_1, format(1920, 1080, 50)));
}

#[test]
fn unknown_rate_is_checked_on_frame_size_only() {
    let f = InputFormat {
        width: 1920,
        height: 1080,
        fps: None,
    };
    assert!(covers(H264Level::L4_1, f));
    assert!(!covers(H264Level::L3_1, f));
}

#[test]
fn level_too_low_names_what_arrived_and_the_level_it_needs() {
    let err = check(H264Level::L3_1, format(1920, 1080, 50), None).unwrap_err();
    assert!(err.contains("1920x1080 at 50 fps"), "{err}");
    assert!(
        err.contains("8160 macroblocks per frame and 408000 per second"),
        "{err}"
    );
    assert!(err.contains("level 3.1 allows 3600 and 108000"), "{err}");
    assert!(err.contains("needs level 4.2 or higher"), "{err}");
    assert!(err.contains("Level property"), "{err}");
    assert!(check(H264Level::L4_2, format(1920, 1080, 50), None).is_ok());
}

#[test]
fn auto_level_exceeded_later_says_where_it_came_from() {
    let first = format(640, 360, 25);
    let err = check(H264Level::L4_2, format(3840, 2160, 30), Some(first)).unwrap_err();
    assert!(
        err.contains("auto chose 4.2 from the first input (640x360 at 25 fps)"),
        "{err}"
    );
    assert!(err.contains("needs level 5.1"), "{err}");
    let err = check(H264Level::L5_2, format(7680, 4320, 30), None).unwrap_err();
    assert!(err.contains("above 5.2"), "{err}");
}

#[test]
fn level_applies_only_to_h264_on_encoders_that_take_it_from_downstream() {
    for enc in ["vtenc_h264", "vtenc_h264_hw", "x264enc"] {
        assert_eq!(
            applied_h264_level(Codec::H264, H264Level::Auto, enc),
            H264Level::Auto
        );
        assert_eq!(
            applied_h264_level(Codec::H264, H264Level::L5_1, enc),
            H264Level::L5_1
        );
        assert_eq!(
            applied_h264_level(Codec::H264, H264Level::None, enc),
            H264Level::None
        );
    }
    for enc in [
        "nvh264enc",
        "qsvh264enc",
        "vah264enc",
        "amfh264enc",
        "mfh264enc",
        "openh264enc",
    ] {
        for setting in H264Level::ALL {
            assert_eq!(
                applied_h264_level(Codec::H264, *setting, enc),
                H264Level::None,
                "{enc}"
            );
        }
    }
    assert_eq!(
        applied_h264_level(Codec::H265, H264Level::L5_1, "vtenc_h265"),
        H264Level::None
    );
}

#[test]
fn level_property_definition() {
    let def = videoenc_definition();
    let prop = def
        .exposed_properties
        .iter()
        .find(|p| p.name == "level")
        .expect("level property");
    assert!(
        matches!(&prop.default_value, Some(PropertyValue::String(s)) if s == "none"),
        "{:?}",
        prop.default_value
    );
    let PropertyType::Enum { values } = &prop.property_type else {
        panic!("level is an enum");
    };
    assert!(values.iter().all(|v| v.visible_when.is_none()));
    let values: Vec<&str> = values.iter().map(|v| v.value.as_str()).collect();
    assert_eq!(values, ["auto", "3.1", "4.1", "4.2", "5.1", "5.2", "none"]);
    assert_eq!(
        format!("{:?}", prop.visible_when),
        format!(
            "{:?}",
            Some(strom_types::PropertyCondition::one_of_strings(
                strom_types::videoenc::CODEC_PROPERTY,
                &[strom_types::videoenc::CODEC_H264]
            ))
        )
    );

    assert_eq!(parse_h264_level(&HashMap::new()), H264Level::None);
    let mut props = HashMap::new();
    props.insert("level".to_string(), PropertyValue::String("5.1".into()));
    assert_eq!(parse_h264_level(&props), H264Level::L5_1);
    props.insert("level".to_string(), PropertyValue::String("bogus".into()));
    assert_eq!(parse_h264_level(&props), H264Level::None);
}

/// The real block with `x264enc`, fed by an `appsrc` that changes format
/// mid-stream. Returns the `level` of every caps event on the block's output,
/// or the error the flow failed with.
fn encode_size_change(level: &str, formats: &[(i32, i32, i32)]) -> Result<Vec<String>, String> {
    let _ = gst::init();
    let mut properties = HashMap::new();
    properties.insert("codec".to_string(), PropertyValue::String("h264".into()));
    properties.insert(
        "encoder_preference".to_string(),
        PropertyValue::String("software".into()),
    );
    properties.insert("level".to_string(), PropertyValue::String(level.into()));
    let ctx = BlockBuildContext::new(Vec::new(), "all".to_string());
    let result = VideoEncBuilder
        .build("enc", &properties, &ctx)
        .expect("build the block");

    let pipeline = gst::Pipeline::new();
    let by_id: HashMap<&str, &gst::Element> = result
        .elements
        .iter()
        .map(|(id, element)| (id.as_str(), element))
        .collect();
    assert_eq!(
        by_id["enc:encoder"].factory().unwrap().name(),
        "x264enc",
        "software H.264 is x264enc"
    );
    for element in by_id.values() {
        pipeline.add(*element).expect("add element");
    }
    for (from, to) in &result.internal_links {
        let src = by_id[from.element_id.as_str()]
            .static_pad(from.pad_name.as_deref().unwrap_or("src"))
            .expect("source pad");
        let sink = by_id[to.element_id.as_str()]
            .static_pad(to.pad_name.as_deref().unwrap_or("sink"))
            .expect("sink pad");
        src.link(&sink).expect("internal link");
    }
    let appsrc = gst_app::AppSrc::builder().format(gst::Format::Time).build();
    let sink = gst::ElementFactory::make("fakesink")
        .property("sync", false)
        .build()
        .unwrap();
    pipeline
        .add_many([appsrc.upcast_ref::<gst::Element>(), &sink])
        .unwrap();
    appsrc.link(by_id["enc:input"]).unwrap();
    by_id["enc:capsfilter"].link(&sink).unwrap();

    let levels = Arc::new(Mutex::new(Vec::new()));
    let seen = levels.clone();
    by_id["enc:capsfilter"]
        .static_pad("src")
        .unwrap()
        .add_probe(gst::PadProbeType::EVENT_DOWNSTREAM, move |_, info| {
            if let Some(gst::PadProbeData::Event(event)) = &info.data {
                if let gst::EventView::Caps(c) = event.view() {
                    let s = c.caps().structure(0).unwrap();
                    seen.lock().unwrap().push(format!(
                        "{}x{}:{}",
                        s.get::<i32>("width").unwrap_or(0),
                        s.get::<i32>("height").unwrap_or(0),
                        s.get::<String>("level").unwrap_or_default()
                    ));
                }
            }
            gst::PadProbeReturn::Ok
        });

    pipeline.set_state(gst::State::Playing).unwrap();
    let mut pts = gst::ClockTime::ZERO;
    for &(width, height, fps) in formats {
        let caps = gst::Caps::builder("video/x-raw")
            .field("format", "I420")
            .field("width", width)
            .field("height", height)
            .field("framerate", gst::Fraction::new(fps, 1))
            .build();
        let size = (width * height * 3 / 2) as usize;
        let duration = gst::ClockTime::SECOND / fps as u64;
        for _ in 0..5 {
            let mut buffer = gst::Buffer::from_mut_slice(vec![128u8; size]);
            {
                let b = buffer.get_mut().unwrap();
                b.set_pts(pts);
                b.set_duration(duration);
            }
            pts += duration;
            let sample = gst::Sample::builder().caps(&caps).buffer(&buffer).build();
            if appsrc.push_sample(&sample).is_err() {
                break;
            }
        }
    }
    let _ = appsrc.end_of_stream();

    let bus = pipeline.bus().unwrap();
    let message = bus
        .timed_pop_filtered(
            gst::ClockTime::from_seconds(60),
            &[gst::MessageType::Eos, gst::MessageType::Error],
        )
        .expect("EOS or error within 60 s");
    let outcome = match message.view() {
        gst::MessageView::Error(err) => Err(err.error().to_string()),
        _ => Ok(levels.lock().unwrap().clone()),
    };
    pipeline.set_state(gst::State::Null).unwrap();
    outcome
}

/// An HLS variant switch, 640x360@25 to 1920x1080@50, keeps level 4.2 on the
/// output with level auto. Fails if the pin is removed: x264enc then signals
/// a lower level for 360p and 4.2 for 1080p50.
#[test]
fn auto_level_survives_a_size_change_through_x264enc() {
    if !(require_element("x264enc") && require_element("h264parse")) {
        return;
    }
    let caps =
        encode_size_change("auto", &[(640, 360, 25), (1920, 1080, 50)]).expect("the flow runs");
    assert!(caps.len() >= 2, "expected a caps change, got {caps:?}");
    assert!(caps.iter().any(|c| c.starts_with("640x360:")), "{caps:?}");
    assert!(caps.iter().any(|c| c.starts_with("1920x1080:")), "{caps:?}");
    assert!(caps.iter().all(|c| c.ends_with(":4.2")), "{caps:?}");
}

/// Counterpart: with level none the encoder changes the level with the size,
/// which is what the test above guards against.
#[test]
fn level_none_follows_the_size_through_x264enc() {
    if !(require_element("x264enc") && require_element("h264parse")) {
        return;
    }
    let caps =
        encode_size_change("none", &[(640, 360, 25), (1920, 1080, 50)]).expect("the flow runs");
    let first = caps.first().expect("caps");
    let last = caps.last().expect("caps");
    assert_ne!(
        first.rsplit(':').next(),
        last.rsplit(':').next(),
        "{caps:?}"
    );
}

/// A fixed level too low for the input fails the flow with a message that
/// names the level it needs, not a bare not-negotiated.
#[test]
fn fixed_level_too_low_fails_the_flow_with_the_level_it_needs() {
    if !(require_element("x264enc") && require_element("h264parse")) {
        return;
    }
    let err = encode_size_change("3.1", &[(1920, 1080, 50)]).unwrap_err();
    assert!(err.contains("needs level 4.2"), "{err}");
    assert!(err.contains("Level property"), "{err}");
}
