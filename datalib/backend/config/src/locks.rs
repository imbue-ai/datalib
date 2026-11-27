//! What a config says about locks: how a step holds one, and the two every
//! config has without declaring them. The runner sizes them and does the
//! accounting (`datalib_dag::supervisor::locks`).

use serde::{Deserialize, Serialize};
use strum::{EnumString, IntoStaticStr, VariantArray};

/// How a step holds a lock.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Serialize,
    Deserialize,
    EnumString,
    IntoStaticStr,
    VariantArray,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum Hold {
    /// One slot.
    Shared,
    /// Every slot: nobody else holds it at the same time.
    Exclusive,
}

impl Hold {
    pub fn as_str(self) -> &'static str {
        self.into()
    }

    /// `None` for a spelling this build does not know.
    pub fn parse(s: &str) -> Option<Self> {
        s.parse().ok()
    }
}

/// The budget a step that declares no inputs holds when it names no lock:
/// in practice a download, so a rate limit on one keeps no render from
/// starting.
pub const NETWORK: &str = "network";
/// The budget a step that reads something holds when it names no lock.
pub const CPU: &str = "cpu";

/// The locks every config has, whether or not it declares them. A
/// `[[locks]]` entry of the same name resizes one; any other lock a step
/// holds, the config declares.
pub const DEFAULT_LOCKS: [&str; 2] = [NETWORK, CPU];

#[cfg(test)]
mod tests {
    use super::*;

    /// strum and serde spell these independently; the config is read with
    /// serde and the record's detail with strum.
    #[test]
    fn hold_as_str_matches_the_serde_spelling() {
        for &v in Hold::VARIANTS {
            let json = serde_json::to_string(&v).unwrap();
            assert_eq!(json, format!("\"{}\"", v.as_str()), "{v:?}");
            assert_eq!(Hold::parse(v.as_str()), Some(v));
        }
    }
}
