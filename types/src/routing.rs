//! The audio routing matrix wire format.
//!
//! Shared because both the backend blocks and the graph editor read and write
//! it: two parsers for one format drift, and a routing matrix that survives a
//! round trip through the editor is the whole contract.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};

/// Ceiling on a crosspoint gain. A router has no business amplifying, and
/// summing several crosspoints onto one output already risks clipping.
pub const MAX_CROSSPOINT_GAIN: f64 = 1.0;

/// Anything at or below this reads as fully closed.
pub const GAIN_FLOOR_DB: f64 = -60.0;

/// One crosspoint of a routing matrix: an input channel feeding an output
/// channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Crosspoint {
    pub in_stream: usize,
    pub in_channel: usize,
    pub out_stream: usize,
    pub out_channel: usize,
}

impl Crosspoint {
    pub fn new(in_stream: usize, in_channel: usize, out_stream: usize, out_channel: usize) -> Self {
        Self {
            in_stream,
            in_channel,
            out_stream,
            out_channel,
        }
    }

    /// The `iXcY` key naming this crosspoint's source.
    pub fn source_key(&self) -> String {
        format!("i{}c{}", self.in_stream, self.in_channel)
    }

    /// The `oXcY` key naming this crosspoint's destination.
    pub fn destination_key(&self) -> String {
        format!("o{}c{}", self.out_stream, self.out_channel)
    }
}

/// Gain per crosspoint. A crosspoint that is absent is closed.
pub type RoutingGains = HashMap<Crosspoint, f64>;

/// Parse a routing matrix, accepting either form.
///
/// * `{"i0c0": ["o0c0", "o1c0"]}` — the listed crosspoints open at unity. This
///   is what `builtin.audiorouter` writes.
/// * `{"i0c0": {"o0c0": 1.0, "o1c0": 0.35}}` — an explicit gain per crosspoint.
///
/// An unusable key is skipped rather than failing the whole matrix: one bad key
/// should not silently mute a live router. Returns the gains and the keys that
/// could not be read, so a caller that can warn does.
pub fn parse_routing_gains(json: &str) -> (RoutingGains, Vec<String>) {
    let mut gains = RoutingGains::new();
    let mut skipped = Vec::new();

    let trimmed = json.trim();
    if trimmed.is_empty() || trimmed == "{}" {
        return (gains, skipped);
    }

    let Ok(entries) = serde_json::from_str::<HashMap<String, serde_json::Value>>(trimmed) else {
        skipped.push(format!("unparsable routing matrix JSON: {json}"));
        return (gains, skipped);
    };

    for (source, destinations) in entries {
        let Some((in_stream, in_channel)) = parse_routing_key(&source, 'i') else {
            skipped.push(source);
            continue;
        };

        // Flatten to (destination key, gain) first so the insert loop below
        // needs only one mutable borrow of `skipped`.
        let pairs: Vec<(String, f64)> = match destinations {
            serde_json::Value::Array(list) => list
                .into_iter()
                .filter_map(|destination| match destination.as_str() {
                    Some(key) => Some((key.to_string(), 1.0)),
                    None => {
                        skipped.push(destination.to_string());
                        None
                    }
                })
                .collect(),
            serde_json::Value::Object(map) => map
                .into_iter()
                .filter_map(|(destination, gain)| match gain.as_f64() {
                    Some(gain) => Some((destination, gain)),
                    None => {
                        skipped.push(destination);
                        None
                    }
                })
                .collect(),
            other => {
                skipped.push(format!("{source}: {other}"));
                Vec::new()
            }
        };

        for (destination, gain) in pairs {
            match parse_routing_key(&destination, 'o') {
                Some((out_stream, out_channel)) => {
                    gains.insert(
                        Crosspoint::new(in_stream, in_channel, out_stream, out_channel),
                        gain.clamp(0.0, MAX_CROSSPOINT_GAIN),
                    );
                }
                None => skipped.push(destination),
            }
        }
    }

    (gains, skipped)
}

/// Serialise a routing matrix.
///
/// Stays on the list form while every gain is unity, so a block that only
/// understands on/off keeps working and its stored routing does not change
/// shape. Output is ordered, so saving the same routing twice gives the same
/// string and the flow file does not churn.
pub fn serialize_routing_gains(gains: &RoutingGains) -> String {
    let all_unity = gains
        .values()
        .all(|gain| (*gain - 1.0).abs() < f64::EPSILON);

    let value = if all_unity {
        let mut plain: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for crosspoint in gains.keys() {
            plain
                .entry(crosspoint.source_key())
                .or_default()
                .push(crosspoint.destination_key());
        }
        for destinations in plain.values_mut() {
            destinations.sort();
        }
        serde_json::to_value(plain)
    } else {
        let mut withgains: BTreeMap<String, BTreeMap<String, f64>> = BTreeMap::new();
        for (crosspoint, gain) in gains {
            withgains
                .entry(crosspoint.source_key())
                .or_default()
                .insert(crosspoint.destination_key(), *gain);
        }
        serde_json::to_value(withgains)
    };

    value
        .and_then(|v| serde_json::to_string(&v))
        .unwrap_or_else(|_| "{}".to_string())
}

/// Parse a routing key like `i0c1` or `o2c3` into (stream, channel).
pub fn parse_routing_key(key: &str, prefix: char) -> Option<(usize, usize)> {
    let rest = key.strip_prefix(prefix)?;
    let (stream, channel) = rest.split_once('c')?;
    Some((stream.parse().ok()?, channel.parse().ok()?))
}

/// A crosspoint gain as dB, for display. Amplitudes at or below the floor read
/// as fully closed.
pub fn gain_to_db(gain: f64) -> f64 {
    if gain <= 0.0 {
        GAIN_FLOOR_DB
    } else {
        (20.0 * gain.log10()).max(GAIN_FLOOR_DB)
    }
}

/// A dB value back to a crosspoint gain.
pub fn db_to_gain(db: f64) -> f64 {
    if db <= GAIN_FLOOR_DB {
        0.0
    } else {
        10.0_f64.powf(db / 20.0).clamp(0.0, MAX_CROSSPOINT_GAIN)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn xp(i: usize, c: usize, o: usize, d: usize) -> Crosspoint {
        Crosspoint::new(i, c, o, d)
    }

    #[test]
    fn the_list_form_opens_crosspoints_at_unity() {
        let (gains, skipped) = parse_routing_gains(r#"{"i0c0": ["o0c0", "o1c2"]}"#);
        assert!(skipped.is_empty());
        assert_eq!(gains.get(&xp(0, 0, 0, 0)), Some(&1.0));
        assert_eq!(gains.get(&xp(0, 0, 1, 2)), Some(&1.0));
        assert_eq!(gains.len(), 2);
    }

    #[test]
    fn the_object_form_carries_a_gain_per_crosspoint() {
        let (gains, _) = parse_routing_gains(r#"{"i1c1": {"o0c0": 0.35, "o0c1": 1.0}}"#);
        assert_eq!(gains.get(&xp(1, 1, 0, 0)), Some(&0.35));
        assert_eq!(gains.get(&xp(1, 1, 0, 1)), Some(&1.0));
    }

    #[test]
    fn gains_are_capped_at_unity_so_a_router_never_amplifies() {
        let (gains, _) = parse_routing_gains(r#"{"i0c0": {"o0c0": 4.0, "o0c1": -1.0}}"#);
        assert_eq!(gains.get(&xp(0, 0, 0, 0)), Some(&1.0));
        assert_eq!(gains.get(&xp(0, 0, 0, 1)), Some(&0.0));
    }

    #[test]
    fn an_unusable_key_does_not_discard_the_rest_of_the_routing() {
        let (gains, skipped) =
            parse_routing_gains(r#"{"i0c0": ["o0c0"], "nonsense": ["o0c1"], "i0c1": ["zz"]}"#);
        assert_eq!(gains.get(&xp(0, 0, 0, 0)), Some(&1.0));
        assert_eq!(
            gains.len(),
            1,
            "only the valid crosspoint survives: {gains:?}"
        );
        assert_eq!(
            skipped.len(),
            2,
            "and both bad keys are reported: {skipped:?}"
        );
    }

    #[test]
    fn empty_or_unusable_json_closes_everything() {
        for json in ["{}", "", "   ", "not json", r#"{"i0c0": 42}"#] {
            assert!(
                parse_routing_gains(json).0.is_empty(),
                "{json:?} should yield no crosspoints"
            );
        }
    }

    /// `builtin.audiorouter` only understands the list form, and the editor is
    /// shared with the gain-capable router — so a plain routing must come back
    /// unchanged, byte for byte.
    #[test]
    fn a_plain_routing_round_trips_as_the_list_form() {
        let json = r#"{"i0c0":["o0c0","o1c0"],"i0c1":["o0c1"]}"#;
        let (gains, _) = parse_routing_gains(json);
        assert_eq!(serialize_routing_gains(&gains), json);
    }

    #[test]
    fn a_gain_below_unity_switches_the_output_to_the_object_form() {
        let json = r#"{"i0c0":{"o0c0":1.0,"o0c1":0.25}}"#;
        let (gains, _) = parse_routing_gains(json);
        assert_eq!(serialize_routing_gains(&gains), json);
        assert_eq!(
            parse_routing_gains(&serialize_routing_gains(&gains))
                .0
                .get(&xp(0, 0, 0, 1)),
            Some(&0.25),
            "the gain must survive a round trip, not drop to unity"
        );
    }

    #[test]
    fn serialising_the_same_routing_twice_gives_the_same_string() {
        // Otherwise every save rewrites the flow file with a reshuffled matrix.
        let (gains, _) = parse_routing_gains(
            r#"{"i2c3":["o1c6","o0c0"],"i0c0":["o1c0","o0c0"],"i1c1":["o0c1"]}"#,
        );
        let once = serialize_routing_gains(&gains);
        assert_eq!(once, serialize_routing_gains(&gains));
        assert_eq!(once, serialize_routing_gains(&parse_routing_gains(&once).0));
    }

    #[test]
    fn db_and_gain_round_trip_across_the_control_range() {
        for gain in [1.0, 0.5, 0.25, 0.1, 0.01] {
            let back = db_to_gain(gain_to_db(gain));
            assert!(
                (back - gain).abs() < 1e-6,
                "gain {gain} came back as {back}"
            );
        }
        assert_eq!(db_to_gain(GAIN_FLOOR_DB), 0.0);
        assert_eq!(gain_to_db(0.0), GAIN_FLOOR_DB);
    }
}
