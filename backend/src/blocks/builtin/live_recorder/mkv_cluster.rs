//! Just enough Matroska reading to make each split file start at zero.
//!
//! Every cluster carries an absolute `Timestamp` (in `TimestampScale` units,
//! 1 ms by default); the blocks inside are relative to it. `matroskamux`
//! emits each cluster's header as a buffer of its own, starting with the
//! Cluster element ID. The sink moves every cluster timestamp in a file back by
//! the file's first one. A smaller value always fits the bytes the larger one
//! had, so nothing changes size.

const CLUSTER_ID: [u8; 4] = [0x1F, 0x43, 0xB6, 0x75];
const TIMESTAMP_ID: u8 = 0xE7;

/// An EBML variable-length size at `data[pos..]`: (value, bytes it takes).
fn vint(data: &[u8], pos: usize) -> Option<(u64, usize)> {
    let first = *data.get(pos)?;
    if first == 0 {
        return None;
    }
    let len = first.leading_zeros() as usize + 1;
    let mut value = (first as u64) & (0xFF >> len);
    for i in 1..len {
        value = (value << 8) | *data.get(pos + i)? as u64;
    }
    Some((value, len))
}

/// Where a cluster header's timestamp sits: (offset, length, value).
fn timestamp(data: &[u8]) -> Option<(usize, usize, u64)> {
    if data.get(..4)? != CLUSTER_ID {
        return None;
    }
    let (_, size_len) = vint(data, 4)?;
    let pos = 4 + size_len;
    if *data.get(pos)? != TIMESTAMP_ID {
        return None;
    }
    let (len, len_len) = vint(data, pos + 1)?;
    let start = pos + 1 + len_len;
    let len = len as usize;
    if len == 0 || len > 8 {
        return None;
    }
    let bytes = data.get(start..start + len)?;
    let value = bytes.iter().fold(0u64, |acc, b| (acc << 8) | *b as u64);
    Some((start, len, value))
}

/// The timestamp of a cluster header, if `data` starts with one.
pub fn cluster_timestamp(data: &[u8]) -> Option<u64> {
    timestamp(data).map(|(_, _, v)| v)
}

/// Move a cluster header's timestamp back by `base`, in place.
pub fn shift_cluster_timestamp(data: &mut [u8], base: u64) {
    let Some((start, len, value)) = timestamp(data) else {
        return;
    };
    let shifted = value.saturating_sub(base).to_be_bytes();
    data[start..start + len].copy_from_slice(&shifted[8 - len..]);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A cluster header as matroskamux writes it: unknown size, then Timestamp.
    fn cluster(ts: &[u8]) -> Vec<u8> {
        let mut v = CLUSTER_ID.to_vec();
        v.extend_from_slice(&[0x01, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF]);
        v.push(TIMESTAMP_ID);
        v.push(0x80 | ts.len() as u8);
        v.extend_from_slice(ts);
        v
    }

    #[test]
    fn reads_a_cluster_timestamp() {
        assert_eq!(cluster_timestamp(&cluster(&[0x94, 0x70])), Some(38_000));
        assert_eq!(cluster_timestamp(b"not a cluster"), None);
    }

    #[test]
    fn shifting_keeps_the_length_and_moves_the_value() {
        let mut c = cluster(&[0x94, 0x70]);
        let len = c.len();
        shift_cluster_timestamp(&mut c, 37_990);
        assert_eq!(c.len(), len);
        assert_eq!(cluster_timestamp(&c), Some(10));
    }
}
