//! Property transforms between exposed block properties and underlying element values.
//!
//! Block definitions declare `PropertyMapping.transform` as a string name. This module
//! resolves those names to actual conversion functions — forward (exposed → element) and
//! inverse (element → exposed). Identity is used when no transform is set.

use strom_types::PropertyValue;

/// Forward + inverse pair for a named transform.
pub struct Transform {
    pub forward: fn(PropertyValue) -> Option<PropertyValue>,
    pub inverse: fn(PropertyValue) -> Option<PropertyValue>,
}

/// Look up a transform by its registered name. Returns identity for `None`.
pub fn lookup(name: Option<&str>) -> Transform {
    match name {
        None => IDENTITY,
        Some("bool_to_volume") => BOOL_TO_VOLUME,
        Some("db_to_linear") => DB_TO_LINEAR,
        Some(_) => IDENTITY,
    }
}

const IDENTITY: Transform = Transform {
    forward: |v| Some(v),
    inverse: |v| Some(v),
};

/// Bool → Float: true → 1.0, false → 0.0. Used for gating volume elements with a bool.
const BOOL_TO_VOLUME: Transform = Transform {
    forward: |v| match v {
        PropertyValue::Bool(true) => Some(PropertyValue::Float(1.0)),
        PropertyValue::Bool(false) => Some(PropertyValue::Float(0.0)),
        _ => None,
    },
    inverse: |v| match v {
        PropertyValue::Float(f) => Some(PropertyValue::Bool(f >= 0.5)),
        _ => None,
    },
};

/// Float dB → Float linear and back. -120 dB floor matches `linear_to_db` in mixer/properties.rs.
const DB_TO_LINEAR: Transform = Transform {
    forward: |v| match v {
        PropertyValue::Float(db) => Some(PropertyValue::Float(10.0_f64.powf(db / 20.0))),
        PropertyValue::Int(db) => Some(PropertyValue::Float(10.0_f64.powf(db as f64 / 20.0))),
        _ => None,
    },
    inverse: |v| match v {
        PropertyValue::Float(linear) => {
            let db = if linear <= 0.0 {
                -120.0
            } else {
                20.0 * linear.log10()
            };
            Some(PropertyValue::Float(db))
        }
        _ => None,
    },
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bool_to_volume_roundtrip() {
        let t = lookup(Some("bool_to_volume"));
        assert!(matches!(
            (t.forward)(PropertyValue::Bool(true)),
            Some(PropertyValue::Float(f)) if (f - 1.0).abs() < 1e-9
        ));
        assert!(matches!(
            (t.forward)(PropertyValue::Bool(false)),
            Some(PropertyValue::Float(f)) if f.abs() < 1e-9
        ));
        assert!(matches!(
            (t.inverse)(PropertyValue::Float(1.0)),
            Some(PropertyValue::Bool(true))
        ));
        assert!(matches!(
            (t.inverse)(PropertyValue::Float(0.0)),
            Some(PropertyValue::Bool(false))
        ));
    }

    #[test]
    fn db_to_linear_roundtrip() {
        let t = lookup(Some("db_to_linear"));
        let fwd = (t.forward)(PropertyValue::Float(0.0)).unwrap();
        match fwd {
            PropertyValue::Float(f) => assert!((f - 1.0).abs() < 1e-9),
            _ => panic!(),
        }
        let inv = (t.inverse)(PropertyValue::Float(1.0)).unwrap();
        match inv {
            PropertyValue::Float(db) => assert!(db.abs() < 1e-9),
            _ => panic!(),
        }
        // -6 dB ≈ 0.501
        let half = (t.forward)(PropertyValue::Float(-6.0)).unwrap();
        match half {
            PropertyValue::Float(f) => assert!((f - 0.5012).abs() < 1e-3),
            _ => panic!(),
        }
    }

    #[test]
    fn identity_passes_through() {
        let t = lookup(None);
        assert!(matches!(
            (t.forward)(PropertyValue::Int(42)),
            Some(PropertyValue::Int(42))
        ));
    }

    #[test]
    fn unknown_transform_falls_back_to_identity() {
        // Tolerating unknown transform names keeps older block definitions
        // (perhaps loaded from disk, perhaps user-defined) working when the
        // backend doesn't recognise their transform name.
        let t = lookup(Some("definitely_not_a_real_transform"));
        assert!(matches!(
            (t.forward)(PropertyValue::Float(1.5)),
            Some(PropertyValue::Float(f)) if (f - 1.5).abs() < 1e-9
        ));
        assert!(matches!(
            (t.inverse)(PropertyValue::Bool(true)),
            Some(PropertyValue::Bool(true))
        ));
    }

    #[test]
    fn bool_to_volume_rejects_non_bool_forward() {
        // The forward direction expects Bool; sending Float should yield None
        // so state.rs reports a clear "value type does not match transform"
        // error to the API caller.
        let t = lookup(Some("bool_to_volume"));
        assert!((t.forward)(PropertyValue::Float(0.5)).is_none());
        assert!((t.forward)(PropertyValue::Int(1)).is_none());
    }

    #[test]
    fn db_to_linear_rejects_non_numeric_forward() {
        let t = lookup(Some("db_to_linear"));
        assert!((t.forward)(PropertyValue::Bool(true)).is_none());
        assert!((t.forward)(PropertyValue::String("0".to_string())).is_none());
    }
}
