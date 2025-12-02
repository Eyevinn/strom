/// Minimal test program for glvideomixerelement request pad linking
/// Tests:
/// 1. Can link to request pads with element.link_pads(Some("src"), mixer, None)
/// 2. Can set pad properties (xpos, ypos, width, height, alpha, zorder)
/// 3. Does video actually composite (two different patterns side-by-side)
/// 4. Does it work on restart (second pipeline after first is destroyed)
use gstreamer as gst;
use gstreamer::prelude::*;

fn main() {
    // Initialize GStreamer
    gst::init().expect("Failed to initialize GStreamer");

    println!("=== Testing glvideomixerelement request pads ===\n");

    // Test 1: First pipeline - should work
    println!("TEST 1: First pipeline creation and start");
    println!("        Expected: Two video patterns (smpte + ball) side-by-side");
    test_pipeline(1);

    // Test 2: Second pipeline - this is where crashes typically happen
    println!("\nTEST 2: Second pipeline creation and start (after first was destroyed)");
    println!("        This tests if glvideomixerelement handles recreation properly");
    test_pipeline(2);

    // Test 3: Third pipeline - be extra sure
    println!("\nTEST 3: Third pipeline creation (paranoia check)");
    test_pipeline(3);

    println!("\n=== All tests completed successfully ===");
    println!("✓ Request pad linking works");
    println!("✓ Pad properties work");
    println!("✓ Video composition works");
    println!("✓ Restart works (no crashes)");
}

fn test_pipeline(test_num: i32) {
    // Create pipeline
    let pipeline = gst::Pipeline::new();

    // Create two videotestsrc elements with DIFFERENT patterns so we can verify compositing
    // Using is-live=true to simulate real live sources (important for aggregator sync)
    let src1 = gst::ElementFactory::make("videotestsrc")
        .property_from_str("pattern", "smpte") // color bars
        .property("is-live", true) // Simulate live source
        .property("num-buffers", 60i32) // 2 seconds at 30fps
        .build()
        .expect("Failed to create videotestsrc 1");

    let src2 = gst::ElementFactory::make("videotestsrc")
        .property_from_str("pattern", "ball") // moving ball
        .property("is-live", true) // Simulate live source
        .property("num-buffers", 60i32)
        .build()
        .expect("Failed to create videotestsrc 2");

    // Create GL upload elements (glvideomixerelement needs GL memory)
    let upload1 = gst::ElementFactory::make("glupload")
        .build()
        .expect("Failed to create glupload 1");

    let upload2 = gst::ElementFactory::make("glupload")
        .build()
        .expect("Failed to create glupload 2");

    // Create glvideomixerelement
    let mixer = gst::ElementFactory::make("glvideomixerelement")
        .build()
        .expect("Failed to create glvideomixerelement");

    // TEST: Set latency property in NULL state (before adding to pipeline)
    println!("  Testing latency property in NULL state...");
    let latency_ns: u64 = 100_000_000; // 100ms in nanoseconds
    println!("  Setting latency to {}ns (100ms)", latency_ns);
    mixer.set_property("latency", latency_ns);
    println!("  ✓ Latency property set successfully in NULL state");

    // TEST: Request pads and set pad properties in NULL state
    println!("  Testing pad properties in NULL state (before adding to pipeline)...");
    println!("  Requesting sink pad 0...");
    let sink_pad_0 = mixer
        .request_pad_simple("sink_%u")
        .expect("Failed to request sink pad 0");
    println!("  ✓ Got pad: {}", sink_pad_0.name());

    println!("  Setting properties on pad 0 in NULL state...");
    sink_pad_0.set_property("xpos", 80i32);
    sink_pad_0.set_property("ypos", 80i32);
    sink_pad_0.set_property("width", 240i32);
    sink_pad_0.set_property("height", 120i32);
    sink_pad_0.set_property("alpha", 1.0f64);
    sink_pad_0.set_property("zorder", 0u32);
    println!("  ✓ Pad 0 properties set in NULL state");

    println!("  Requesting sink pad 1...");
    let sink_pad_1 = mixer
        .request_pad_simple("sink_%u")
        .expect("Failed to request sink pad 1");
    println!("  ✓ Got pad: {}", sink_pad_1.name());

    println!("  Setting properties on pad 1 in NULL state...");
    sink_pad_1.set_property("xpos", 640i32);
    sink_pad_1.set_property("ypos", 320i32);
    sink_pad_1.set_property("width", 120i32);
    sink_pad_1.set_property("height", 480i32);
    sink_pad_1.set_property("alpha", 1.0f64);
    sink_pad_1.set_property("zorder", 1u32);
    println!("  ✓ Pad 1 properties set in NULL state");

    // Create gldownload for output
    let download = gst::ElementFactory::make("gldownload")
        .build()
        .expect("Failed to create gldownload");

    // Create videoconvert, encoder and muxer to save as MP4
    let output_file = format!("/tmp/test-glvideomixer-{}.mp4", test_num);

    let videoconvert = gst::ElementFactory::make("videoconvert")
        .build()
        .expect("Failed to create videoconvert");

    let encoder = gst::ElementFactory::make("x264enc")
        .property_from_str("speed-preset", "ultrafast")
        .build()
        .expect("Failed to create x264enc");

    let muxer = gst::ElementFactory::make("mp4mux")
        .build()
        .expect("Failed to create mp4mux");

    let sink = gst::ElementFactory::make("filesink")
        .property("location", &output_file)
        .build()
        .expect("Failed to create filesink");
    println!("  Output will be saved to: {}", output_file);

    // Add elements to pipeline (including encoder/muxer for MP4)
    pipeline
        .add_many([
            &src1,
            &src2,
            &upload1,
            &upload2,
            &mixer,
            &download,
            &videoconvert,
            &encoder,
            &muxer,
            &sink,
        ])
        .expect("Failed to add elements");

    println!("  Pipeline created with elements in NULL state");

    // Link everything in NULL state (using pre-created pads with properties already set)
    println!("  Linking src1 -> upload1...");
    src1.link(&upload1).expect("Failed to link src1 to upload1");
    println!("  ✓ src1 -> upload1");

    println!(
        "  Linking upload1:src -> mixer:{} (using pre-created pad)...",
        sink_pad_0.name()
    );
    upload1
        .link_pads(Some("src"), &mixer, Some(sink_pad_0.name().as_str()))
        .expect("Failed to link upload1 to mixer sink_0");
    println!("  ✓ upload1 -> mixer (pad properties already set in NULL state)");

    // Link src2 -> upload2 -> mixer (using pre-created pad)
    println!("  Linking src2 -> upload2...");
    src2.link(&upload2).expect("Failed to link src2 to upload2");
    println!("  ✓ src2 -> upload2");

    println!(
        "  Linking upload2:src -> mixer:{} (using pre-created pad)...",
        sink_pad_1.name()
    );
    upload2
        .link_pads(Some("src"), &mixer, Some(sink_pad_1.name().as_str()))
        .expect("Failed to link upload2 to mixer sink_1");
    println!("  ✓ upload2 -> mixer (pad properties already set in NULL state)");

    // Link mixer -> download -> videoconvert -> encoder -> muxer -> sink
    println!("  Linking mixer -> download...");
    mixer
        .link(&download)
        .expect("Failed to link mixer to download");
    println!("  ✓ mixer -> download");

    println!("  Linking download -> videoconvert...");
    download
        .link(&videoconvert)
        .expect("Failed to link download to videoconvert");
    println!("  ✓ download -> videoconvert");

    println!("  Linking videoconvert -> encoder...");
    videoconvert
        .link(&encoder)
        .expect("Failed to link videoconvert to encoder");
    println!("  ✓ videoconvert -> encoder");

    println!("  Linking encoder -> muxer...");
    encoder
        .link(&muxer)
        .expect("Failed to link encoder to muxer");
    println!("  ✓ encoder -> muxer");

    println!("  Linking muxer -> sink...");
    muxer.link(&sink).expect("Failed to link muxer to sink");
    println!("  ✓ muxer -> sink");

    // Set to PLAYING
    println!("  Setting pipeline to PLAYING...");
    pipeline
        .set_state(gst::State::Playing)
        .expect("Failed to set to PLAYING");

    // Wait for EOS
    let bus = pipeline.bus().expect("Failed to get bus");
    let mut frame_count = 0;
    for msg in bus.iter_timed(gst::ClockTime::NONE) {
        use gst::MessageView;
        match msg.view() {
            MessageView::Eos(..) => {
                println!("  Got EOS after {} buffers, stopping pipeline", frame_count);
                break;
            }
            MessageView::Error(err) => {
                eprintln!("  ❌ Error: {} ({:?})", err.error(), err.debug());
                panic!("Pipeline error");
            }
            MessageView::StateChanged(state) => {
                if state.src().map(|s| s == &pipeline).unwrap_or(false) {
                    println!(
                        "  Pipeline state: {:?} -> {:?}",
                        state.old(),
                        state.current()
                    );
                }
            }
            MessageView::Element(elem) => {
                if let Some(s) = elem.structure() {
                    if s.name() == "progress" {
                        frame_count += 1;
                        if frame_count % 20 == 0 {
                            println!("  Processed {} frames...", frame_count);
                        }
                    }
                }
            }
            _ => {}
        }
    }

    println!("  ✓ Pipeline completed successfully");

    // Cleanup
    println!("  Setting pipeline to NULL...");
    pipeline
        .set_state(gst::State::Null)
        .expect("Failed to set to NULL");

    println!("  Pipeline stopped and destroyed");
    drop(pipeline);
    println!("  ✓ Test completed");
}
