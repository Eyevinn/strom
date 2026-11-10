//! Correcting the latency a source reports when it reports less than its
//! data actually has.

use gstreamer as gst;
use gstreamer::prelude::*;

/// Raise the minimum of every live latency answer that passes `pad` to at
/// least `floor`, while `applies()` returns true.
///
/// `applies` is asked on each latency query, not once at build time, so it can
/// look at what the pipeline actually created (an auto-plugging source picks
/// its child on the way to READY).
///
/// The maximum is raised with the minimum when it is lower, so the answer
/// stays consistent. That is only right when the source does hold its data
/// that long without losing it, as a source does whose frames really arrive
/// `floor` after their timestamps. Answers that are not live, and answers
/// whose minimum is already at or above `floor`, pass unchanged.
///
/// A QUERY probe on the answered query: it fires per latency query, never per
/// buffer.
pub fn raise_live_min_latency<F>(pad: &gst::Pad, floor: gst::ClockTime, applies: F)
where
    F: Fn() -> bool + Send + Sync + 'static,
{
    pad.add_probe(
        gst::PadProbeType::QUERY_UPSTREAM | gst::PadProbeType::PULL,
        move |_pad, info| {
            let Some(query) = info.query_mut() else {
                return gst::PadProbeReturn::Ok;
            };
            if let gst::QueryViewMut::Latency(latency) = query.view_mut() {
                let (live, min, max) = latency.result();
                if live && min < floor && applies() {
                    latency.set(live, floor, max.map(|max| max.max(floor)));
                }
            }
            gst::PadProbeReturn::Ok
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    const FLOOR: gst::ClockTime = gst::ClockTime::from_mseconds(100);

    type Answer = (bool, gst::ClockTime, Option<gst::ClockTime>);

    /// Run `videotestsrc` at `fps` into a capsfilter, optionally carrying the
    /// floor (applying when `applies`), and return the latency answer the
    /// sink gets.
    fn answer(is_live: bool, fps: i32, floor: Option<bool>) -> Answer {
        gst::init().unwrap();
        let pipeline = gst::Pipeline::new();
        let src = gst::ElementFactory::make("videotestsrc")
            .property("is-live", is_live)
            .build()
            .unwrap();
        let caps = gst::ElementFactory::make("capsfilter")
            .property(
                "caps",
                gst::Caps::builder("video/x-raw")
                    .field("width", 320)
                    .field("height", 240)
                    .field("framerate", gst::Fraction::new(fps, 1))
                    .build(),
            )
            .build()
            .unwrap();
        let sink = gst::ElementFactory::make("fakesink").build().unwrap();
        pipeline.add_many([&src, &caps, &sink]).unwrap();
        gst::Element::link_many([&src, &caps, &sink]).unwrap();
        if let Some(applies) = floor {
            raise_live_min_latency(&caps.static_pad("src").unwrap(), FLOOR, move || applies);
        }

        pipeline.set_state(gst::State::Playing).unwrap();
        let (res, _, _) = pipeline.state(gst::ClockTime::from_seconds(5));
        res.unwrap();
        let mut query = gst::query::Latency::new();
        assert!(sink.static_pad("sink").unwrap().peer_query(&mut query));
        pipeline.set_state(gst::State::Null).unwrap();
        query.result()
    }

    #[test]
    fn raises_a_live_minimum_below_the_floor() {
        // videotestsrc at 30 fps reports one frame, 33.3 ms, as avfvideosrc
        // does.
        let (live, min, max) = answer(true, 30, Some(true));
        assert!(live);
        assert_eq!(min, FLOOR);
        assert!(
            max.is_none_or(|max| max >= min),
            "max {max:?} below min {min}"
        );
    }

    #[test]
    fn leaves_a_live_minimum_above_the_floor_alone() {
        // 5 fps: one frame is 200 ms, above the floor.
        assert_eq!(answer(true, 5, Some(true)), answer(true, 5, None));
    }

    #[test]
    fn leaves_a_non_live_answer_alone() {
        assert_eq!(answer(false, 30, Some(true)), answer(false, 30, None));
    }

    #[test]
    fn leaves_the_answer_alone_when_it_does_not_apply() {
        assert_eq!(answer(true, 30, Some(false)), answer(true, 30, None));
    }
}
