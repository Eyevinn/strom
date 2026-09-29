//! Reading a pipeline's bus until a deadline.

use std::time::{Duration, Instant};

use gstreamer as gst;
use gstreamer::prelude::*;

/// An error message from the bus, with the name of the element that posted it.
#[derive(Debug, Clone)]
pub struct BusError {
    pub source: String,
    pub message: String,
    pub debug: Option<String>,
}

impl std::fmt::Display for BusError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {} ({:?})", self.source, self.message, self.debug)
    }
}

fn bus_error(msg: &gst::Message) -> Option<BusError> {
    let gst::MessageView::Error(err) = msg.view() else {
        return None;
    };
    Some(BusError {
        source: msg
            .src()
            .map(|s| s.name().to_string())
            .unwrap_or_else(|| "unknown".to_string()),
        message: err.error().to_string(),
        debug: err.debug().map(|d| d.to_string()),
    })
}

/// The first error posted within `wait`, or `None` if the bus stayed clean.
pub fn first_error(bus: &gst::Bus, wait: Duration) -> Option<BusError> {
    let deadline = Instant::now() + wait;
    while Instant::now() < deadline {
        if let Some(msg) = bus.timed_pop_filtered(
            gst::ClockTime::from_mseconds(50),
            &[gst::MessageType::Error],
        ) {
            return bus_error(&msg);
        }
    }
    None
}

/// Every error posted during `window`. Always waits the whole window.
pub fn collect_errors(bus: &gst::Bus, window: Duration) -> Vec<BusError> {
    let deadline = Instant::now() + window;
    let mut errors = Vec::new();
    while Instant::now() < deadline {
        if let Some(msg) = bus.timed_pop_filtered(
            gst::ClockTime::from_mseconds(50),
            &[gst::MessageType::Error],
        ) {
            errors.extend(bus_error(&msg));
        }
    }
    errors
}
