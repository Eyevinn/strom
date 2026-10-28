//! Just enough ISO BMFF box reading to make each split file start at zero,
//! and to index each file when it closes.
//!
//! The muxer numbers time through the whole recording, so the second file's
//! first fragment says it starts at, say, 38 s, and players show a 46 s file
//! that holds 8 s. Each fragment's `tfdt` (track fragment decode time) carries
//! that time per track, in the track's own timescale. The sink subtracts the
//! file's start from every `tfdt` it writes, the same instant for every track
//! so they stay in sync. Values change in place; no box changes size.
//!
//! A fragmented file with no `sidx` (segment index) makes FFmpeg's demuxer,
//! and so Chrome's `<video>`, read every `moof` in the file before it reports
//! metadata: over HTTP that is the whole file. One `sidx` ahead of the first
//! fragment that covers every fragment to the end of the file stops that. An
//! `mfra` at the end does not; FFmpeg skips it by default.

use std::collections::HashMap;

/// Iterate the boxes in `data`: (type, body start, end) with offsets into `data`.
fn boxes(data: &[u8]) -> impl Iterator<Item = ([u8; 4], usize, usize)> + '_ {
    let mut pos = 0usize;
    std::iter::from_fn(move || {
        if pos + 8 > data.len() {
            return None;
        }
        let size32 = u32::from_be_bytes(data[pos..pos + 4].try_into().ok()?) as usize;
        let kind: [u8; 4] = data[pos + 4..pos + 8].try_into().ok()?;
        let (header, size) = match size32 {
            1 => {
                if pos + 16 > data.len() {
                    return None;
                }
                let s = u64::from_be_bytes(data[pos + 8..pos + 16].try_into().ok()?) as usize;
                (16, s)
            }
            0 => (8, data.len() - pos),
            s => (8, s),
        };
        if size < header || pos + size > data.len() {
            return None;
        }
        let item = (kind, pos + header, pos + size);
        pos += size;
        Some(item)
    })
}

fn child<'a>(data: &'a [u8], kind: &[u8; 4]) -> Option<&'a [u8]> {
    boxes(data)
        .find(|(k, _, _)| k == kind)
        .map(|(_, start, end)| &data[start..end])
}

fn read_u32(data: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes(data.get(at..at + 4)?.try_into().ok()?))
}

/// Track id → timescale, from an init segment (`ftyp` + `moov`).
pub fn track_timescales(init: &[u8]) -> HashMap<u32, u32> {
    tracks(init).into_iter().collect()
}

/// The first track in an init segment, as (track id, timescale): the track a
/// `sidx` refers to.
pub fn first_track(init: &[u8]) -> Option<(u32, u32)> {
    tracks(init).into_iter().next()
}

/// (track id, timescale) of each track, in `moov` order.
fn tracks(init: &[u8]) -> Vec<(u32, u32)> {
    let mut result = Vec::new();
    let Some(moov) = child(init, b"moov") else {
        return result;
    };
    for (kind, start, end) in boxes(moov) {
        if &kind != b"trak" {
            continue;
        }
        let trak = &moov[start..end];
        let track_id = child(trak, b"tkhd").and_then(|tkhd| {
            // version 1: creation/modification are 64-bit.
            let at = if tkhd.first() == Some(&1) { 20 } else { 12 };
            read_u32(tkhd, at)
        });
        let timescale = child(trak, b"mdia")
            .and_then(|mdia| child(mdia, b"mdhd"))
            .and_then(|mdhd| {
                let at = if mdhd.first() == Some(&1) { 20 } else { 12 };
                read_u32(mdhd, at)
            });
        if let (Some(id), Some(ts)) = (track_id, timescale) {
            if ts > 0 {
                result.push((id, ts));
            }
        }
    }
    result
}

/// Track id → default sample duration, from the `trex` boxes of an init
/// segment. A fragment whose `tfhd` and `trun` give no duration uses it.
pub fn default_sample_durations(init: &[u8]) -> HashMap<u32, u32> {
    let mut result = HashMap::new();
    let Some(mvex) = child(init, b"moov").and_then(|moov| child(moov, b"mvex")) else {
        return result;
    };
    for (kind, start, end) in boxes(mvex) {
        if &kind != b"trex" {
            continue;
        }
        let trex = &mvex[start..end];
        if let (Some(id), Some(duration)) = (read_u32(trex, 4), read_u32(trex, 12)) {
            result.insert(id, duration);
        }
    }
    result
}

/// Where each `tfdt` value sits in a fragment header, with its track and width.
struct DecodeTime {
    track_id: u32,
    offset: usize,
    wide: bool,
}

fn decode_times(fragment: &[u8]) -> Vec<DecodeTime> {
    let mut result = Vec::new();
    for (kind, moof_start, moof_end) in boxes(fragment) {
        if &kind != b"moof" {
            continue;
        }
        let moof = &fragment[moof_start..moof_end];
        for (kind, traf_start, traf_end) in boxes(moof) {
            if &kind != b"traf" {
                continue;
            }
            let traf = &moof[traf_start..traf_end];
            let Some(track_id) = child(traf, b"tfhd").and_then(|tfhd| read_u32(tfhd, 4)) else {
                continue;
            };
            for (kind, tfdt_start, _) in boxes(traf) {
                if &kind == b"tfdt" {
                    result.push(DecodeTime {
                        track_id,
                        offset: moof_start + traf_start + tfdt_start + 4,
                        wide: traf.get(tfdt_start) == Some(&1),
                    });
                }
            }
        }
    }
    result
}

/// The earliest decode time in a fragment header, in nanoseconds.
pub fn fragment_start_ns(fragment: &[u8], timescales: &HashMap<u32, u32>) -> Option<u64> {
    decode_times(fragment)
        .iter()
        .filter_map(|dt| {
            let ts = *timescales.get(&dt.track_id)? as u128;
            let value = read_decode_time(fragment, dt)? as u128;
            Some((value * 1_000_000_000 / ts) as u64)
        })
        .min()
}

/// Where a fragment ends, in nanoseconds: the latest track's decode time plus
/// the duration of its samples.
pub fn fragment_end_ns(
    fragment: &[u8],
    timescales: &HashMap<u32, u32>,
    default_durations: &HashMap<u32, u32>,
) -> Option<u64> {
    let mut end = None;
    for (kind, moof_start, moof_end) in boxes(fragment) {
        if &kind != b"moof" {
            continue;
        }
        let moof = &fragment[moof_start..moof_end];
        for (kind, traf_start, traf_end) in boxes(moof) {
            if &kind != b"traf" {
                continue;
            }
            let traf = &moof[traf_start..traf_end];
            let tfhd = child(traf, b"tfhd")?;
            let track_id = read_u32(tfhd, 4)?;
            let ts = *timescales.get(&track_id)? as u128;
            let tfdt = child(traf, b"tfdt")?;
            let decode_time = if tfdt.first() == Some(&1) {
                u64::from_be_bytes(tfdt.get(4..12)?.try_into().ok()?)
            } else {
                read_u32(tfdt, 4)? as u64
            };
            let default = tfhd_default_duration(tfhd)
                .or_else(|| default_durations.get(&track_id).copied())
                .unwrap_or(0);
            let mut duration = 0u64;
            for (kind, start, run_end) in boxes(traf) {
                if &kind == b"trun" {
                    duration += trun_duration(&traf[start..run_end], default)?;
                }
            }
            let ns = ((decode_time + duration) as u128 * 1_000_000_000 / ts) as u64;
            end = end.max(Some(ns));
        }
    }
    end
}

/// `default_sample_duration` of a `tfhd`, when its flags say it is there.
fn tfhd_default_duration(tfhd: &[u8]) -> Option<u32> {
    let flags = read_u32(tfhd, 0)? & 0x00ff_ffff;
    if flags & 0x08 == 0 {
        return None;
    }
    // After version/flags and track_ID: base_data_offset, then
    // sample_description_index, each only when flagged.
    let mut at = 8;
    if flags & 0x01 != 0 {
        at += 8;
    }
    if flags & 0x02 != 0 {
        at += 4;
    }
    read_u32(tfhd, at)
}

/// The summed sample durations of a `trun`, in its track's timescale.
fn trun_duration(trun: &[u8], default: u32) -> Option<u64> {
    let flags = read_u32(trun, 0)? & 0x00ff_ffff;
    let count = read_u32(trun, 4)? as u64;
    if flags & 0x100 == 0 {
        return Some(count * default as u64);
    }
    let mut at = 8;
    if flags & 0x01 != 0 {
        at += 4; // data_offset
    }
    if flags & 0x04 != 0 {
        at += 4; // first_sample_flags
    }
    let per_sample = 4 * [0x100, 0x200, 0x400, 0x800]
        .iter()
        .filter(|&&f| flags & f != 0)
        .count();
    let mut total = 0u64;
    for i in 0..count as usize {
        total += read_u32(trun, at + i * per_sample)? as u64;
    }
    Some(total)
}

/// One fragment as a `sidx` lists it: its size in bytes from `styp` to the
/// end of its `mdat`, and its duration in the index's timescale.
pub struct IndexEntry {
    pub size: u32,
    pub duration: u32,
}

/// Size of a `sidx` with `entries` references.
pub fn sidx_len(entries: usize) -> usize {
    40 + 12 * entries
}

/// A `sidx` (version 1) for `track_id`, whose first fragment starts right
/// after it.
pub fn sidx(track_id: u32, timescale: u32, earliest: u64, entries: &[IndexEntry]) -> Vec<u8> {
    let len = sidx_len(entries.len());
    let mut v = Vec::with_capacity(len);
    v.extend_from_slice(&(len as u32).to_be_bytes());
    v.extend_from_slice(b"sidx");
    v.extend_from_slice(&[1, 0, 0, 0]);
    v.extend_from_slice(&track_id.to_be_bytes());
    v.extend_from_slice(&timescale.to_be_bytes());
    v.extend_from_slice(&earliest.to_be_bytes());
    // first_offset: nothing between the index and the first fragment.
    v.extend_from_slice(&0u64.to_be_bytes());
    v.extend_from_slice(&0u16.to_be_bytes());
    v.extend_from_slice(&(entries.len() as u16).to_be_bytes());
    for entry in entries {
        // reference_type 0: the reference is media, not another index.
        v.extend_from_slice(&(entry.size & 0x7fff_ffff).to_be_bytes());
        v.extend_from_slice(&entry.duration.to_be_bytes());
        // starts_with_SAP 0, SAP_type 0: not stated. A fragment cut while the
        // video stalls starts without a keyframe.
        v.extend_from_slice(&0u32.to_be_bytes());
    }
    v
}

/// A `free` box `len` bytes long, at least 8.
pub fn free(len: usize) -> Vec<u8> {
    let mut v = vec![0u8; len];
    v[..4].copy_from_slice(&(len as u32).to_be_bytes());
    v[4..8].copy_from_slice(b"free");
    v
}

fn read_decode_time(data: &[u8], dt: &DecodeTime) -> Option<u64> {
    if dt.wide {
        Some(u64::from_be_bytes(
            data.get(dt.offset..dt.offset + 8)?.try_into().ok()?,
        ))
    } else {
        read_u32(data, dt.offset).map(u64::from)
    }
}

/// Move every `tfdt` in a fragment header back by `base_ns`, in each track's
/// own timescale. A value that would go below zero becomes zero.
pub fn shift_decode_times(fragment: &mut [u8], timescales: &HashMap<u32, u32>, base_ns: u64) {
    for dt in decode_times(fragment) {
        let Some(&ts) = timescales.get(&dt.track_id) else {
            continue;
        };
        let Some(value) = read_decode_time(fragment, &dt) else {
            continue;
        };
        let base = (base_ns as u128 * ts as u128 / 1_000_000_000) as u64;
        let shifted = value.saturating_sub(base);
        if dt.wide {
            fragment[dt.offset..dt.offset + 8].copy_from_slice(&shifted.to_be_bytes());
        } else {
            let v = u32::try_from(shifted).unwrap_or(u32::MAX);
            fragment[dt.offset..dt.offset + 4].copy_from_slice(&v.to_be_bytes());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bx(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut v = ((body.len() + 8) as u32).to_be_bytes().to_vec();
        v.extend_from_slice(kind);
        v.extend_from_slice(body);
        v
    }

    fn full(version: u8, rest: &[u8]) -> Vec<u8> {
        let mut v = vec![version, 0, 0, 0];
        v.extend_from_slice(rest);
        v
    }

    fn trak(id: u32, timescale: u32) -> Vec<u8> {
        // tkhd v0: creation, modification, track_id
        let mut tkhd = vec![0u8; 8];
        tkhd.extend_from_slice(&id.to_be_bytes());
        // mdhd v0: creation, modification, timescale
        let mut mdhd = vec![0u8; 8];
        mdhd.extend_from_slice(&timescale.to_be_bytes());
        let mut body = bx(b"tkhd", &full(0, &tkhd));
        body.extend(bx(b"mdia", &bx(b"mdhd", &full(0, &mdhd))));
        bx(b"trak", &body)
    }

    fn traf(id: u32, tfdt: u64) -> Vec<u8> {
        let mut body = bx(b"tfhd", &full(0, &id.to_be_bytes()));
        body.extend(bx(b"tfdt", &full(1, &tfdt.to_be_bytes())));
        bx(b"traf", &body)
    }

    fn init() -> Vec<u8> {
        let mut moov = trak(1, 90_000);
        moov.extend(trak(2, 48_000));
        let mut v = bx(b"ftyp", b"iso6");
        v.extend(bx(b"moov", &moov));
        v
    }

    fn fragment(video: u64, audio: u64) -> Vec<u8> {
        let mut moof = bx(b"mfhd", &full(0, &7u32.to_be_bytes()));
        moof.extend(traf(1, video));
        moof.extend(traf(2, audio));
        let mut v = bx(b"styp", b"msdh");
        v.extend(bx(b"moof", &moof));
        v
    }

    #[test]
    fn reads_each_tracks_timescale_from_the_init_segment() {
        let ts = track_timescales(&init());
        assert_eq!(ts.get(&1), Some(&90_000));
        assert_eq!(ts.get(&2), Some(&48_000));
    }

    #[test]
    fn a_fragment_starts_at_its_earliest_track() {
        let ts = track_timescales(&init());
        // video at 38.0 s, audio at 37.99 s
        let f = fragment(38 * 90_000, 37 * 48_000 + 47_520);
        assert_eq!(fragment_start_ns(&f, &ts), Some(37_990_000_000));
    }

    #[test]
    fn shifting_moves_every_track_by_the_same_time() {
        let ts = track_timescales(&init());
        let mut f = fragment(38 * 90_000, 37 * 48_000 + 47_520);
        shift_decode_times(&mut f, &ts, 37_990_000_000);
        let times: Vec<u64> = decode_times(&f)
            .iter()
            .map(|dt| read_decode_time(&f, dt).unwrap())
            .collect();
        // video 10 ms after the file start, audio at zero.
        assert_eq!(times, vec![900, 0]);
    }

    /// A `traf` whose `trun` gives each sample's duration.
    fn traf_with_durations(id: u32, tfdt: u64, durations: &[u32]) -> Vec<u8> {
        let mut body = bx(b"tfhd", &full(0, &id.to_be_bytes()));
        body.extend(bx(b"tfdt", &full(1, &tfdt.to_be_bytes())));
        // flags: data_offset, sample_duration, sample_size
        let mut trun = vec![0, 0, 0x03, 0x01];
        trun.extend_from_slice(&(durations.len() as u32).to_be_bytes());
        trun.extend_from_slice(&0u32.to_be_bytes());
        for d in durations {
            trun.extend_from_slice(&d.to_be_bytes());
            trun.extend_from_slice(&100u32.to_be_bytes());
        }
        body.extend(bx(b"trun", &trun));
        bx(b"traf", &body)
    }

    /// A `traf` whose samples take their duration from a default.
    fn traf_with_default(id: u32, tfdt: u64, samples: u32, in_tfhd: Option<u32>) -> Vec<u8> {
        let mut tfhd = match in_tfhd {
            Some(_) => vec![0, 0, 0, 0x08],
            None => vec![0, 0, 0, 0],
        };
        tfhd.extend_from_slice(&id.to_be_bytes());
        if let Some(d) = in_tfhd {
            tfhd.extend_from_slice(&d.to_be_bytes());
        }
        let mut body = bx(b"tfhd", &tfhd);
        body.extend(bx(b"tfdt", &full(1, &tfdt.to_be_bytes())));
        let mut trun = full(0, &samples.to_be_bytes());
        trun[3] = 0;
        body.extend(bx(b"trun", &trun));
        bx(b"traf", &body)
    }

    fn init_with_trex(audio_default: u32) -> Vec<u8> {
        let mut moov = trak(1, 90_000);
        moov.extend(trak(2, 48_000));
        let mut trex = 2u32.to_be_bytes().to_vec();
        trex.extend_from_slice(&1u32.to_be_bytes());
        trex.extend_from_slice(&audio_default.to_be_bytes());
        trex.extend_from_slice(&[0; 8]);
        moov.extend(bx(b"mvex", &bx(b"trex", &full(0, &trex))));
        let mut v = bx(b"ftyp", b"iso6");
        v.extend(bx(b"moov", &moov));
        v
    }

    fn moof(trafs: &[Vec<u8>]) -> Vec<u8> {
        let mut moof = bx(b"mfhd", &full(0, &7u32.to_be_bytes()));
        for traf in trafs {
            moof.extend_from_slice(traf);
        }
        let mut v = bx(b"styp", b"msdh");
        v.extend(bx(b"moof", &moof));
        v
    }

    #[test]
    fn the_first_track_is_the_one_the_index_refers_to() {
        assert_eq!(first_track(&init()), Some((1, 90_000)));
        assert_eq!(first_track(b"INIT;"), None);
    }

    #[test]
    fn a_fragment_ends_where_its_longest_track_ends() {
        let init = init_with_trex(1024);
        let ts = track_timescales(&init);
        let defaults = default_sample_durations(&init);
        // video: 2 s from 10 s; audio: 48 frames of 1024 from 10 s, 1.024 s.
        let f = moof(&[
            traf_with_durations(1, 900_000, &[90_000, 90_000]),
            traf_with_default(2, 480_000, 48, None),
        ]);
        assert_eq!(fragment_end_ns(&f, &ts, &defaults), Some(12_000_000_000));
        // A default in the tfhd wins over the trex one.
        let f = moof(&[traf_with_default(2, 480_000, 48, Some(2048))]);
        assert_eq!(fragment_end_ns(&f, &ts, &defaults), Some(12_048_000_000));
    }

    #[test]
    fn the_index_lists_each_fragment_after_its_header() {
        let entries = [
            IndexEntry {
                size: 1000,
                duration: 90_000,
            },
            IndexEntry {
                size: 2000,
                duration: 45_000,
            },
        ];
        let sidx = sidx(1, 90_000, 0, &entries);
        assert_eq!(sidx.len(), sidx_len(2));
        assert_eq!(&sidx[4..8], b"sidx");
        assert_eq!(read_u32(&sidx, 0), Some(sidx_len(2) as u32));
        assert_eq!(read_u32(&sidx, 12), Some(1));
        assert_eq!(read_u32(&sidx, 16), Some(90_000));
        assert_eq!(&sidx[36..40], &[0, 0, 0, 2]);
        assert_eq!(read_u32(&sidx, 40), Some(1000));
        assert_eq!(read_u32(&sidx, 44), Some(90_000));
        assert_eq!(read_u32(&sidx, 52), Some(2000));
        let free = free(20);
        assert_eq!((read_u32(&free, 0), &free[4..8]), (Some(20), &b"free"[..]));
    }
}
