//! Regression test for the `EXC_GUARD` crash in the macOS per-thread CPU sampler.
//!
//! Bug: the thread registry stored a streaming thread's mach port *name*, taken
//! with `pthread_mach_thread_np()`, which does not take a user reference on the
//! port. Once the thread exited, the name was freed and the kernel was free to
//! hand it to any other port. `ThreadCpuSampler` then called `thread_info()` on
//! that stale name, and when the name had been recycled to a *guarded* port
//! (libdispatch and XPC guard theirs) the kernel raised `EXC_GUARD` and killed
//! the process. Thread churn — every WHIP session builds and tears down
//! streaming threads — plus an open stats WebSocket was enough to hit it.
//!
//! Fix: the registry holds a `ThreadHandle`, which owns a send right on the
//! port, so the name stays bound to the thread it was captured from until the
//! handle is dropped.
//!
//! This test drives real GStreamer streaming threads through the real bus sync
//! handler and the real sampler, then asserts the invariant the fix provides:
//! a port name held by a snapshot the sampler could still be working from is
//! never freed, even after the thread has exited and the flow has been torn
//! down. Revert the fix and the names are gone by then.

use gstreamer as gst;
use gstreamer::prelude::*;
use std::time::Duration;
use strom::gst::thread_priority;
use strom::system_monitor::ThreadCpuSampler;
use strom::thread_registry::ThreadRegistry;
use strom_types::flow::ThreadPriority;

/// How long glib keeps an idle pooled thread before letting it exit.
const IDLE_REAP: Duration = Duration::from_millis(100);

const REQUIRED_ELEMENTS: &[&str] = &["videotestsrc", "queue", "fakesink"];

/// Skipping on a missing element passes green and guards nothing, so CI sets
/// `STROM_REQUIRE_GST_PLUGINS=1` to turn a skip into a failure.
fn missing_element() -> Option<&'static str> {
    let missing = REQUIRED_ELEMENTS
        .iter()
        .copied()
        .find(|e| gst::ElementFactory::find(e).is_none())?;
    assert!(
        std::env::var("STROM_REQUIRE_GST_PLUGINS").is_err(),
        "STROM_REQUIRE_GST_PLUGINS is set but this element is missing: {missing}"
    );
    Some(missing)
}

/// Each branch runs on its own streaming thread, so the pipeline registers
/// enough of them for one recycled name to be likely rather than incidental.
const BRANCHES: usize = 16;

fn build_pipeline() -> gst::Pipeline {
    let description = (0..BRANCHES)
        .map(|_| "videotestsrc is-live=true ! queue ! fakesink sync=false")
        .collect::<Vec<_>>()
        .join(" ");

    gst::parse::launch(&description)
        .expect("pipeline should parse")
        .downcast::<gst::Pipeline>()
        .expect("parse::launch returns a pipeline")
}

/// Whether `name` is still allocated in this task's IPC space. `false` means
/// the kernel may have handed it to an unrelated port, which is exactly when
/// `thread_info()` becomes unsafe to call.
#[cfg(target_os = "macos")]
fn mach_port_name_is_allocated(name: libc::mach_port_t) -> bool {
    extern "C" {
        fn mach_port_type(
            task: libc::mach_port_t,
            name: libc::mach_port_t,
            ptype: *mut libc::natural_t,
        ) -> libc::kern_return_t;
        static mach_task_self_: libc::mach_port_t;
    }

    let mut ptype: libc::natural_t = 0;
    // SAFETY: `ptype` is a valid out parameter; the call only inspects `name`.
    let kr = unsafe { mach_port_type(mach_task_self_, name, &mut ptype) };
    kr == libc::KERN_SUCCESS
}

#[test]
fn registered_thread_ids_survive_pipeline_teardown() {
    gst::init().expect("GStreamer should initialise");
    if let Some(missing) = missing_element() {
        eprintln!("skipping: missing GStreamer element '{missing}'");
        return;
    }

    // GStreamer hands streaming threads back to a glib thread pool that keeps
    // idle ones alive for 15 seconds by default, so a short teardown/rebuild
    // cycle reuses the same threads and no port name is ever released. A live
    // server sits idle for far longer than that between WHIP sessions; shorten
    // the retention so the same thread churn happens inside a test run.
    extern "C" {
        fn g_thread_pool_set_max_idle_time(interval_ms: u32);
    }
    // SAFETY: a plain global setter in glib, safe to call at any point.
    unsafe { g_thread_pool_set_max_idle_time(IDLE_REAP.as_millis() as u32) };

    let registry = ThreadRegistry::new();
    let mut sampler = ThreadCpuSampler::new();
    let flow_id = uuid::Uuid::new_v4();

    let mut snapshots = Vec::new();
    let mut saw_registered_threads = false;

    // Several start/stop cycles, because the crash needs thread churn: each
    // cycle creates streaming threads, registers them, and joins them again.
    for _ in 0..5 {
        let pipeline = build_pipeline();
        thread_priority::setup_thread_priority_handler(
            &pipeline,
            ThreadPriority::Normal,
            None,
            flow_id,
            Some(registry.clone()),
        );

        pipeline
            .set_state(gst::State::Playing)
            .expect("pipeline should start");
        std::thread::sleep(Duration::from_millis(300));

        // What the stats WebSocket does on every tick, on the real code path.
        let stats = sampler.sample(&registry);
        if !stats.threads.is_empty() {
            saw_registered_threads = true;
        }

        // Hold a snapshot across teardown: this is the window the sampler works
        // in, between reading the registry and calling into mach.
        snapshots.push(registry.get_all());

        thread_priority::remove_thread_priority_handler(&pipeline);
        pipeline
            .set_state(gst::State::Null)
            .expect("pipeline should stop");
        registry.unregister_flow(&flow_id);

        // Long enough for the pool to reap the threads this cycle created.
        std::thread::sleep(IDLE_REAP * 6);

        // Sampling after teardown must also be safe.
        let _ = sampler.sample(&registry);
    }

    assert!(
        saw_registered_threads,
        "no streaming threads were registered, so this test guarded nothing"
    );

    #[cfg(target_os = "macos")]
    for snapshot in &snapshots {
        for info in snapshot {
            let name = info.thread_id() as libc::mach_port_t;
            assert!(
                mach_port_name_is_allocated(name),
                "mach port name {name:#x} for element '{}' was freed while a registry \
                 snapshot still held it; the sampler could hand a recycled name to \
                 thread_info() and take the process down with EXC_GUARD",
                info.element_name
            );
        }
    }

    // The names are only released once the snapshots are dropped.
    drop(snapshots);
}
