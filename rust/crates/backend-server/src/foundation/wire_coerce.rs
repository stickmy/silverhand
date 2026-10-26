//! An optional integer body field whose malformed value is a route-level
//! `invalid input` rather than a serde rejection.

use serde::{Deserialize, Deserializer};
use serde_json::Value;

use silverhand_backend_core::viewer_read_model::presence::MAX_SAFE_INTEGER;

#[derive(Clone, Debug, Default)]
pub enum CoercedNumber {
    #[default]
    Absent,
    Value(i64),
    /// Present but not a JSON integer within the JS-safe domain.
    Invalid,
}

impl CoercedNumber {
    /// The integer, or `None` when absent. Check [`CoercedNumber::is_invalid`] first.
    pub fn value(&self) -> Option<i64> {
        match self {
            Self::Value(v) => Some(*v),
            _ => None,
        }
    }

    pub fn is_invalid(&self) -> bool {
        matches!(self, Self::Invalid)
    }
}

impl<'de> Deserialize<'de> for CoercedNumber {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = Value::deserialize(deserializer)?;
        Ok(match value.as_i64() {
            Some(v) if (-MAX_SAFE_INTEGER..=MAX_SAFE_INTEGER).contains(&v) => Self::Value(v),
            _ => Self::Invalid,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::CoercedNumber;

    fn decode(json: &str) -> CoercedNumber {
        #[derive(serde::Deserialize)]
        struct Body {
            #[serde(default)]
            n: CoercedNumber,
        }
        serde_json::from_str::<Body>(json).unwrap().n
    }

    #[test]
    fn only_safe_json_integers_are_values() {
        assert!(matches!(decode("{}"), CoercedNumber::Absent));
        assert!(matches!(decode(r#"{"n":42}"#), CoercedNumber::Value(42)));
        assert!(matches!(decode(r#"{"n":-1}"#), CoercedNumber::Value(-1)));
        for invalid in [
            r#"{"n":"5"}"#,
            r#"{"n":""}"#,
            r#"{"n":null}"#,
            r#"{"n":true}"#,
            r#"{"n":[5]}"#,
            r#"{"n":1.5}"#,
            r#"{"n":9007199254740992}"#,
        ] {
            assert!(decode(invalid).is_invalid(), "{invalid}");
        }
    }
}
