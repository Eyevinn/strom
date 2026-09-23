//! Fixtures shared by the session manager's tests.

use super::*;
use strom_types::block::StreamMode;

pub(super) fn dummy_session() -> (gst::Element, gst::Pipeline, Arc<AtomicBool>) {
    let _ = gst::init();
    let element = gst::ElementFactory::make("fakesrc")
        .build()
        .expect("fakesrc is part of gstreamer core");
    let pipeline = gst::Pipeline::new();
    (element, pipeline, Arc::new(AtomicBool::new(false)))
}

/// How long a fixture session has been running before the test looks at it.
/// Comfortably past `DECODE_GRACE`, so a session that has produced nothing
/// usable in that time is genuinely broken rather than still starting up.
pub(super) const RUNNING_FOR: Duration = Duration::from_secs(60);

/// Tick `stamp` every 20 ms until `stop` is set — the rate the session
/// appsink and the slot's output probe stamp a running publisher at.
pub(super) fn tick_until_stopped(stop: Arc<AtomicBool>, stamp: impl Fn() + Send + 'static) {
    std::thread::spawn(move || {
        while !stop.load(Ordering::SeqCst) {
            stamp();
            std::thread::sleep(Duration::from_millis(20));
        }
    });
}

/// A publisher whose transport is gone: nothing has arrived and nothing has
/// come out of its slot for `idle`. `idle` of zero is the case that matters
/// — a client reconnecting the instant its publisher died looks, at that
/// moment, exactly like a healthy one.
pub(super) fn dead_publisher(idle: Duration) -> Arc<SessionActivity> {
    let ran_for = RUNNING_FOR + idle;
    Arc::new(SessionActivity::from_stamps(
        ActivityStamp::backdated(ran_for, idle),
        Arc::new(ActivityStamp::backdated(ran_for, idle)),
    ))
}

/// A publisher that is still sending and whose media still comes out of its
/// slot: both stamps tick, the way the appsink callback and the tee probe do
/// for a running session. The caller sets `stop` to end it.
pub(super) fn live_publisher(stop: Arc<AtomicBool>) -> Arc<SessionActivity> {
    let output = Arc::new(ActivityStamp::backdated(RUNNING_FOR, Duration::ZERO));
    let activity = Arc::new(SessionActivity::from_stamps(
        ActivityStamp::backdated(RUNNING_FOR, Duration::ZERO),
        output.clone(),
    ));
    let ingress = activity.clone();
    tick_until_stopped(stop.clone(), move || ingress.touch_ingress(true));
    tick_until_stopped(stop, move || output.touch());
    activity
}

/// A healthy video-only publisher of static content, mid-gap: its last frame
/// arrived `gap` ago, came out of its slot, and the next one is not due yet.
/// Never carried audio, so nothing about it can be judged on a two-second
/// silence.
pub(super) fn sparse_video_publisher(gap: Duration) -> Arc<SessionActivity> {
    let ran_for = RUNNING_FOR + gap;
    Arc::new(SessionActivity::video_only_from_stamps(
        ActivityStamp::backdated(ran_for, gap),
        Arc::new(ActivityStamp::backdated(ran_for, gap)),
    ))
}

/// A seat that has received RTP for a minute while nothing has come out of
/// its slot's chain — the decoder never got a usable
/// keyframe, or a consumer below the slot's tee is blocking it. Judged on
/// arriving bytes alone this seat looks perfectly healthy.
pub(super) fn receiving_but_never_usable(stop: Arc<AtomicBool>) -> Arc<SessionActivity> {
    let activity = Arc::new(SessionActivity::from_stamps(
        ActivityStamp::backdated(RUNNING_FOR, Duration::ZERO),
        Arc::new(ActivityStamp::new(Instant::now())),
    ));
    let ingress = activity.clone();
    tick_until_stopped(stop, move || ingress.touch_ingress(true));
    activity
}

/// A seat that decoded, then froze `stalled_for` ago, while RTP keeps
/// arriving.
pub(super) fn receiving_but_stalled(
    stop: Arc<AtomicBool>,
    stalled_for: Duration,
) -> Arc<SessionActivity> {
    let activity = Arc::new(SessionActivity::from_stamps(
        ActivityStamp::backdated(RUNNING_FOR, Duration::ZERO),
        Arc::new(ActivityStamp::backdated(RUNNING_FOR, stalled_for)),
    ));
    let ingress = activity.clone();
    tick_until_stopped(stop, move || ingress.touch_ingress(true));
    activity
}

/// Media has only just started arriving and nothing has decoded yet. Normal:
/// H.264 cannot be decoded until a keyframe brings its parameter sets.
pub(super) fn still_prerolling(stop: Arc<AtomicBool>) -> Arc<SessionActivity> {
    let activity = Arc::new(SessionActivity::from_stamps(
        ActivityStamp::backdated(Duration::from_millis(200), Duration::ZERO),
        Arc::new(ActivityStamp::new(Instant::now())),
    ));
    let ingress = activity.clone();
    tick_until_stopped(stop, move || ingress.touch_ingress(true));
    activity
}

/// A session that is still negotiating: connected, but nothing has arrived.
pub(super) fn no_media_yet() -> Arc<SessionActivity> {
    Arc::new(SessionActivity::new(
        Instant::now(),
        Arc::new(ActivityStamp::new(Instant::now())),
    ))
}

pub(super) fn endpoint_config(max_sessions: usize) -> WhipEndpointConfig {
    WhipEndpointConfig {
        instance_id: "whip-input".to_string(),
        endpoint_id: "endpoint".to_string(),
        mode: StreamMode::AudioVideo,
        stun_server: None,
        turn_server: None,
        ice_transport_policy: "all".to_string(),
        pipeline_weak: Default::default(),
        decode: true,
        video_decoding: Arc::new((0..max_sessions).map(|_| AtomicBool::new(false)).collect()),
        jitterbuffer_latency_ms: 200,
        do_retransmission: true,
        drop_on_latency: true,
        dynamic_webrtcbin_store: Arc::new(Mutex::new(HashMap::new())),
        max_video_bitrate_kbps: 4000,
        max_sessions,
        slot_audio_appsrcs: Vec::new(),
        slot_video_appsrcs: Vec::new(),
        slot_decodebins: vec![Vec::new(); max_sessions],
        slot_output: (0..max_sessions)
            .map(|_| Arc::new(ActivityStamp::new(Instant::now())))
            .collect(),
        slot_assignments: Arc::new(RwLock::new(vec![None; max_sessions])),
    }
}

/// A manager with one single-slot endpoint whose only slot is held by a
/// registered session with the given liveness — i.e. a full endpoint.
pub(super) fn full_endpoint(
    resource_id: &str,
    port: u16,
    activity: Arc<SessionActivity>,
) -> (
    Arc<WhipSessionManager>,
    Arc<WhipEndpointConfig>,
    Arc<AtomicBool>,
) {
    let manager = Arc::new(WhipSessionManager::new());
    manager.start_cleanup_task();
    manager.register_endpoint("endpoint".to_string(), endpoint_config(1));
    let config = manager
        .get_endpoint_config("endpoint")
        .expect("endpoint was just registered");

    assert_eq!(
        config.allocate_slot(resource_id),
        Some(0),
        "the endpoint starts with its only slot free"
    );
    let cleanup_sent = register_with_activity(&manager, resource_id, port, activity);
    assert_eq!(
        config.allocate_slot("someone-else"),
        None,
        "the endpoint is now full"
    );
    (manager, config, cleanup_sent)
}

/// A session's `cleanup_sent` flag is the only way to stop its inactivity
/// watchdog thread. Every path that removes a session must set it, or the
/// watchdog outlives the session and asks for cleanup of a port that is gone.
pub(super) fn register(
    manager: &WhipSessionManager,
    resource_id: &str,
    port: u16,
) -> Arc<AtomicBool> {
    register_with_activity(manager, resource_id, port, dead_publisher(Duration::ZERO))
}

pub(super) fn register_with_activity(
    manager: &WhipSessionManager,
    resource_id: &str,
    port: u16,
    activity: Arc<SessionActivity>,
) -> Arc<AtomicBool> {
    register_in_slot(manager, resource_id, port, 0, activity)
}

pub(super) fn register_in_slot(
    manager: &WhipSessionManager,
    resource_id: &str,
    port: u16,
    slot: usize,
    activity: Arc<SessionActivity>,
) -> Arc<AtomicBool> {
    let (element, pipeline, cleanup_sent) = dummy_session();
    let registered = manager.register_session(NewWhipSession {
        resource_id: resource_id.to_string(),
        port,
        element,
        session_pipeline: pipeline,
        endpoint_id: "endpoint".to_string(),
        slot,
        cleanup_sent: cleanup_sent.clone(),
        activity,
    });
    assert!(registered, "session should register");
    assert!(
        !cleanup_sent.load(Ordering::SeqCst),
        "a freshly registered session is not finished"
    );
    cleanup_sent
}
