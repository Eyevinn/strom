//! Just enough ISO BMFF box reading to make each split file start at zero.
//!
//! The muxer numbers time through the whole recording, so the second file's
//! first fragment says it starts at, say, 38 s, and players show a 46 s file
//! that holds 8 s. Each fragment's `tfdt` (track fragment decode time) carries
//! that time per track, in the track's own timescale. The sink subtracts the
//! file's start from every `tfdt` it writes, the same instant for every track
//! so they stay in sync. Values change in place; no box changes size.

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
    let mut result = HashMap::new();
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
                result.insert(id, ts);
            }
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
}
