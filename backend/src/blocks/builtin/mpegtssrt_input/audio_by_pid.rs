//! Audio outputs in PID order, for the MPEG-TS/SRT input's `map_audio_by_pid`.
//!
//! `audio_out_k` carries the k-th audio stream of the feed, counting from 0 in
//! ascending PID order, whatever order the demuxer adds its pads in. An output
//! with no k-th stream carries the first one instead (a fallback), and moves
//! to its own stream when a later feed has it.
//!
//! Each audio pad of the demuxer feeds a `tee`, so one stream can feed several
//! outputs. Pads are collected until the demuxer signals `no-more-pads`, and
//! then ranked by PID. A demuxer pad's PID is read from its stream-id, which
//! `tsdemux` ends with the PID; in decode mode the id is found upstream of
//! `decodebin`'s pad, inside its decoder chain.
//!
//! A reconnecting caller may move PIDs (#877). `tsdemux` adds the new pads
//! before removing the old ones, so an output fed by a pad of an earlier batch
//! keeps it until that pad is removed, then takes its new stream. `decodebin`
//! removes first, so its outputs are free when the new batch is ranked.

use gstreamer as gst;
use gstreamer::prelude::*;
use tracing::{debug, info, warn};

/// Name of the element message the demuxer posts each time it assigns the
/// audio outputs. Fields, one entry per output in output order:
/// - `pids` (array of `i32`): the PID of the stream the output carries, or -1
///   if the stream-id did not give one.
/// - `fallback` (array of `bool`): the feed has no stream for this output, so
///   it carries the first stream.
pub const AUDIO_MAP_MESSAGE: &str = "strom-mpegtssrt-audio-map";

/// The audio outputs of one block in PID-ordered mode.
///
/// Only weak references to GStreamer objects: this lives in the demuxer's
/// signal closures, which the demuxer owns.
pub(super) struct PidOrderedAudio {
    instance_id: String,
    outputs: Vec<Output>,
    /// Every live demuxer audio pad, with the `tee` it feeds.
    streams: Vec<Stream>,
    /// Streams added since the demuxer last signalled `no-more-pads`.
    open_batch: Vec<u64>,
    /// The stream each output was given at the last `no-more-pads`, by id.
    /// Empty until the first batch is ranked.
    assignment: Vec<u64>,
    next_id: u64,
}

struct Output {
    /// The `audio_out_k` name, for logs.
    name: String,
    /// Where a tee links: the identity's sink pad in passthrough, the head of
    /// the convert chain in decode mode.
    target: gst::glib::WeakRef<gst::Pad>,
}

struct Stream {
    id: u64,
    pad: gst::glib::WeakRef<gst::Pad>,
    pid: Option<u32>,
    tee: gst::glib::WeakRef<gst::Element>,
}

impl PidOrderedAudio {
    /// `targets` holds, per output in order, its name and the pad a stream
    /// links into.
    pub(super) fn new(instance_id: &str, targets: Vec<(String, gst::Pad)>) -> Self {
        Self {
            instance_id: instance_id.to_string(),
            outputs: targets
                .into_iter()
                .map(|(name, pad)| Output {
                    name,
                    target: pad.downgrade(),
                })
                .collect(),
            streams: Vec::new(),
            open_batch: Vec::new(),
            assignment: Vec::new(),
            next_id: 0,
        }
    }

    /// A demuxer audio pad was added: give it a `tee` and hold it until the
    /// batch is complete.
    pub(super) fn pad_added(&mut self, demux: &gst::Element, pad: &gst::Pad) {
        let id = self.next_id;
        self.next_id += 1;
        let pid = stream_pid(pad);
        if pid.is_none() {
            warn!(
                "MPEGTSSRT Input {}: No PID found for audio pad {}; it ranks after the others",
                self.instance_id,
                pad.name()
            );
        }

        let tee = match self.attach_tee(demux, pad, id) {
            Ok(tee) => tee,
            Err(e) => {
                warn!(
                    "MPEGTSSRT Input {}: Could not attach audio pad {}: {}",
                    self.instance_id,
                    pad.name(),
                    e
                );
                return;
            }
        };

        debug!(
            "MPEGTSSRT Input {}: Audio pad {} (PID {}) waits for the stream list to complete",
            self.instance_id,
            pad.name(),
            format_pid(pid)
        );
        self.streams.push(Stream {
            id,
            pad: pad.downgrade(),
            pid,
            tee: tee.downgrade(),
        });
        self.open_batch.push(id);
    }

    fn attach_tee(
        &self,
        demux: &gst::Element,
        pad: &gst::Pad,
        id: u64,
    ) -> Result<gst::Element, String> {
        let bin = demux
            .parent()
            .and_then(|p| p.downcast::<gst::Bin>().ok())
            .ok_or("demuxer has no parent bin")?;
        let tee = gst::ElementFactory::make("tee")
            .name(format!("{}:audio_tee_{}", self.instance_id, id))
            .property("allow-not-linked", true)
            .build()
            .map_err(|e| format!("tee: {}", e))?;
        bin.add(&tee).map_err(|e| format!("add tee: {}", e))?;
        let linked = tee
            .sync_state_with_parent()
            .map_err(|e| format!("sync tee: {}", e))
            .and_then(|_| {
                tee.static_pad("sink")
                    .ok_or("tee has no sink pad".to_string())
            })
            .and_then(|sink| {
                pad.link(&sink)
                    .map_err(|e| format!("link pad -> tee: {:?}", e))
            });
        if let Err(e) = linked {
            let _ = tee.set_state(gst::State::Null);
            let _ = bin.remove(&tee);
            return Err(e);
        }
        Ok(tee)
    }

    /// The demuxer has added every pad of the current stream list: rank the
    /// new pads by PID and give each output its stream.
    ///
    /// Returns the [`AUDIO_MAP_MESSAGE`] to post. The caller posts it after
    /// releasing its lock, since a bus sync handler runs on this thread.
    pub(super) fn no_more_pads(&mut self, demux: &gst::Element) -> Option<gst::Message> {
        if self.open_batch.is_empty() {
            return None;
        }
        // The new streams, in the order their pads were added.
        let batch: Vec<(u64, Option<u32>)> = self
            .streams
            .iter()
            .filter(|s| self.open_batch.contains(&s.id))
            .map(|s| (s.id, s.pid))
            .collect();
        self.open_batch.clear();
        let batch_pids: Vec<Option<u32>> = batch.iter().map(|(_, pid)| *pid).collect();
        let assigned = assign_outputs(&batch_pids, self.outputs.len());
        if assigned.is_empty() {
            return None;
        }

        self.assignment = assigned.iter().map(|(i, _)| batch[*i].0).collect();
        for (output, (i, fallback)) in self.outputs.iter().zip(&assigned) {
            let pid = format_pid(batch[*i].1);
            if *fallback {
                warn!(
                    "MPEGTSSRT Input {}: The feed has {} audio stream(s), so {} carries the first (PID {})",
                    self.instance_id,
                    batch.len(),
                    output.name,
                    pid
                );
            } else {
                info!(
                    "MPEGTSSRT Input {}: {} carries the audio stream with PID {}",
                    self.instance_id, output.name, pid
                );
            }
        }

        self.apply();

        let pids: Vec<i32> = assigned
            .iter()
            .map(|(i, _)| batch[*i].1.map_or(-1, |p| p as i32))
            .collect();
        let fallback: Vec<bool> = assigned.iter().map(|(_, f)| *f).collect();
        let structure = gst::Structure::builder(AUDIO_MAP_MESSAGE)
            .field("pids", gst::Array::new(pids))
            .field("fallback", gst::Array::new(fallback))
            .build();
        Some(gst::message::Element::builder(structure).src(demux).build())
    }

    /// The demuxer removed a pad. If it was an audio pad, take down its tee,
    /// which frees the outputs it fed for the streams assigned to them.
    pub(super) fn pad_removed(&mut self, demux: &gst::Element, pad: &gst::Pad) {
        let Some(index) = self
            .streams
            .iter()
            .position(|s| s.pad.upgrade().is_some_and(|p| &p == pad))
        else {
            return;
        };
        let stream = self.streams.remove(index);
        self.open_batch.retain(|id| *id != stream.id);

        if let Some(tee) = stream.tee.upgrade() {
            for src in tee.src_pads() {
                if let Some(peer) = src.peer() {
                    let _ = src.unlink(&peer);
                }
                tee.release_request_pad(&src);
            }
            if !shutting_down(demux) {
                tee.call_async(|tee| {
                    let _ = tee.set_state(gst::State::Null);
                    if let Some(bin) = tee.parent().and_then(|p| p.downcast::<gst::Bin>().ok()) {
                        let _ = bin.remove(tee);
                    }
                });
            }
        }
        debug!(
            "MPEGTSSRT Input {}: Audio pad {} (PID {}) removed",
            self.instance_id,
            pad.name(),
            format_pid(stream.pid)
        );

        self.apply();
    }

    /// Link each output to its assigned stream, where it is free to take it.
    ///
    /// An output still fed by a stream of an earlier batch keeps it until the
    /// demuxer removes that pad, as in the default mode.
    fn apply(&mut self) {
        for (k, output) in self.outputs.iter().enumerate() {
            let Some(&wanted) = self.assignment.get(k) else {
                continue;
            };
            let Some(stream) = self.streams.iter().find(|s| s.id == wanted) else {
                continue;
            };
            let (Some(tee), Some(target)) = (stream.tee.upgrade(), output.target.upgrade()) else {
                continue;
            };

            if let Some(feeder_pad) = target.peer() {
                let feeder = feeder_pad.parent_element();
                if feeder.as_ref() == Some(&tee) {
                    continue;
                }
                let retiring = feeder.is_some()
                    && self
                        .streams
                        .iter()
                        .any(|s| !self.assignment.contains(&s.id) && s.tee.upgrade() == feeder);
                if retiring {
                    continue;
                }
                let _ = feeder_pad.unlink(&target);
                if let Some(feeder) = feeder {
                    feeder.release_request_pad(&feeder_pad);
                }
            }

            let Some(src) = tee.request_pad_simple("src_%u") else {
                warn!(
                    "MPEGTSSRT Input {}: tee gave no pad for {}",
                    self.instance_id, output.name
                );
                continue;
            };
            match src.link(&target) {
                Ok(_) => info!(
                    "MPEGTSSRT Input {}: Linked audio PID {} -> {}",
                    self.instance_id,
                    format_pid(stream.pid),
                    output.name
                ),
                Err(e) => {
                    warn!(
                        "MPEGTSSRT Input {}: Failed to link audio PID {} -> {}: {:?}",
                        self.instance_id,
                        format_pid(stream.pid),
                        output.name,
                        e
                    );
                    tee.release_request_pad(&src);
                }
            }
        }
    }
}

/// Rank one stream list and give each of `outputs` outputs its stream.
///
/// `pids` lists the streams in the order their pads were added. Returns, per
/// output, the index in `pids` of the stream it carries, and whether that is
/// a fallback: output k carries the k-th stream in ascending PID order, or
/// the first one if there is no k-th. A stream with no known PID ranks after
/// the others, and equal PIDs keep their pad order. Empty if `pids` is.
fn assign_outputs(pids: &[Option<u32>], outputs: usize) -> Vec<(usize, bool)> {
    let mut ranked: Vec<usize> = (0..pids.len()).collect();
    ranked.sort_by_key(|&i| (pids[i].unwrap_or(u32::MAX), i));
    let Some(&first) = ranked.first() else {
        return Vec::new();
    };
    (0..outputs)
        .map(|k| match ranked.get(k) {
            Some(&i) => (i, false),
            None => (first, true),
        })
        .collect()
}

/// The demuxer is on its way down to READY or NULL: the flow is stopping, and
/// the tees go with the pipeline.
fn shutting_down(demux: &gst::Element) -> bool {
    let pending = demux.pending_state();
    demux.current_state() <= gst::State::Ready
        || (pending != gst::State::VoidPending && pending <= gst::State::Ready)
}

fn format_pid(pid: Option<u32>) -> String {
    pid.map_or_else(|| "unknown".to_string(), |p| format!("0x{:x}", p))
}

/// The PID of the elementary stream a demuxer pad carries.
///
/// `tsdemux` ends each stream-id with the PID as 8 hex digits, and names its
/// pads `audio_<program>_<pid>`. A `decodebin` pad is a ghost pad whose
/// stream-start may not have reached it yet, so this walks upstream through
/// the decoder chain until a pad that has one.
pub(super) fn stream_pid(pad: &gst::Pad) -> Option<u32> {
    let mut current = pad.clone();
    // A decoder chain inside decodebin is a handful of elements deep.
    for _ in 0..32 {
        if let Some(pid) = pid_from_pad(&current) {
            return Some(pid);
        }
        let next = if let Some(ghost) = current.downcast_ref::<gst::GhostPad>() {
            ghost.target()
        } else if current.direction() == gst::PadDirection::Src {
            current
                .iterate_internal_links()
                .into_iter()
                .filter_map(Result::ok)
                .next()
        } else {
            current.peer()
        };
        current = next?;
    }
    None
}

fn pid_from_pad(pad: &gst::Pad) -> Option<u32> {
    if let Some(start) = pad.sticky_event::<gst::event::StreamStart>(0) {
        if let Some(pid) = pid_from_stream_id(start.stream_id()) {
            return Some(pid);
        }
    }
    let name = pad.name();
    let is_tsdemux = pad
        .parent_element()
        .and_then(|e| e.factory())
        .is_some_and(|f| f.name() == "tsdemux");
    if is_tsdemux {
        let hex = name.rsplit('_').next()?;
        return u32::from_str_radix(hex, 16).ok();
    }
    None
}

fn pid_from_stream_id(stream_id: &str) -> Option<u32> {
    let last = stream_id.rsplit('/').next()?;
    if last.len() != 8 {
        return None;
    }
    u32::from_str_radix(last, 16).ok()
}

#[cfg(test)]
mod tests {
    use super::{assign_outputs, pid_from_stream_id};

    /// The decode-mode guard: `decodebin` sorts its pads by stream-id, so a
    /// test feed always reaches the block in PID order there.
    #[test]
    fn outputs_follow_pid_order_whatever_order_the_pads_arrive_in() {
        // Pads added as 0x43, 0x41, 0x42.
        let pids = [Some(0x43), Some(0x41), Some(0x42)];
        assert_eq!(
            assign_outputs(&pids, 3),
            vec![(1, false), (2, false), (0, false)],
            "output k must carry the k-th stream in PID order, not in pad order"
        );
    }

    #[test]
    fn an_output_with_no_stream_carries_the_first() {
        // Pads added as 0x42, 0x41; three outputs.
        let pids = [Some(0x42), Some(0x41)];
        assert_eq!(
            assign_outputs(&pids, 3),
            vec![(1, false), (0, false), (1, true)]
        );
        assert_eq!(assign_outputs(&[], 3), vec![]);
    }

    #[test]
    fn a_stream_with_no_pid_ranks_last() {
        let pids = [None, Some(0x51)];
        assert_eq!(assign_outputs(&pids, 2), vec![(1, false), (0, false)]);
    }

    #[test]
    fn reads_the_pid_from_a_tsdemux_stream_id() {
        assert_eq!(
            pid_from_stream_id("2f6c3e1c0a/00000041"),
            Some(0x41),
            "tsdemux appends the PID as 8 hex digits"
        );
        assert_eq!(pid_from_stream_id("2f6c3e1c0a/00001fff"), Some(0x1fff));
        assert_eq!(pid_from_stream_id("2f6c3e1c0a"), None);
        assert_eq!(pid_from_stream_id("abc/xyz"), None);
    }
}
