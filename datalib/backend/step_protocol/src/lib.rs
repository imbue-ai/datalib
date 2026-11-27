//! The step's side of Datalib's step protocol, for a step program written
//! in Rust: what the runner hands a step (its environment and the verb on
//! its command line), and every line a step may print on stdout. The
//! protocol itself is `docs/dev/step_protocol.md`; `README.md` beside this
//! crate has a whole step.
//!
//! Nothing here knows Datalib's stores or its own step program. The
//! runner (`datalib-pipeline`) reads a step's stdout with [`read_line`],
//! and `datalib-step` writes it with [`Emitter`].

mod emit;
pub mod env;
mod event;
mod invocation;
mod outcome;
pub mod problems;
mod read;
mod tree_version;

#[cfg(test)]
mod tests;

pub use emit::Emitter;
pub use event::{Event, LogLevel};
pub use invocation::{Invocation, InvocationError, ParamsError, Read, Verb};
pub use outcome::{FailureKind, Outcome, Output, StoreBytes, CANCELLED_EXIT_CODE};
pub use problems::{Covered, Problem};
pub use read::{read_line, Line, Malformed};
pub use tree_version::tree_version;

/// The version of the protocol this crate speaks: what a package's
/// manifest declares as `step_protocol`.
pub const PROTOCOL_VERSION: u32 = 1;

/// A word that names no value of a closed set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnknownWord {
    /// The set's name, such as `FailureKind`.
    pub set: &'static str,
    pub word: String,
}

impl std::fmt::Display for UnknownWord {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?} is not a {}", self.word, self.set)
    }
}

impl std::error::Error for UnknownWord {}

/// A closed set of words as an enum: each variant's one spelling is the
/// literal beside it, and serde, `as_str`, `parse`, `FromStr` and
/// `strum::VariantArray` all go through it.
macro_rules! closed_set {
    (
        $(#[$meta:meta])*
        pub enum $name:ident {
            $($(#[$vmeta:meta])* $variant:ident = $word:literal),+ $(,)?
        }
    ) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        pub enum $name {
            $($(#[$vmeta])* $variant),+
        }

        impl $name {
            /// Every value, in declaration order.
            pub const ALL: &'static [$name] = &[$($name::$variant),+];

            const WORDS: &'static [&'static str] = &[$($word),+];

            /// The word the protocol spells this value with.
            pub fn as_str(self) -> &'static str {
                match self {
                    $($name::$variant => $word),+
                }
            }

            /// `None` for a word this version of the protocol does not know.
            pub fn parse(word: &str) -> Option<Self> {
                match word {
                    $($word => Some($name::$variant),)+
                    _ => None,
                }
            }
        }

        impl std::str::FromStr for $name {
            type Err = $crate::UnknownWord;

            fn from_str(word: &str) -> Result<Self, Self::Err> {
                $name::parse(word).ok_or_else(|| $crate::UnknownWord {
                    set: stringify!($name),
                    word: word.to_string(),
                })
            }
        }

        impl From<$name> for &'static str {
            fn from(value: $name) -> Self {
                value.as_str()
            }
        }

        impl strum::VariantArray for $name {
            const VARIANTS: &'static [Self] = $name::ALL;
        }

        impl serde::Serialize for $name {
            fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                s.serialize_str(self.as_str())
            }
        }

        impl<'de> serde::Deserialize<'de> for $name {
            fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                struct Word;
                impl serde::de::Visitor<'_> for Word {
                    type Value = $name;
                    fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                        write!(f, "one of {:?}", $name::WORDS)
                    }
                    fn visit_str<E: serde::de::Error>(self, word: &str) -> Result<$name, E> {
                        $name::parse(word)
                            .ok_or_else(|| E::unknown_variant(word, $name::WORDS))
                    }
                }
                d.deserialize_str(Word)
            }
        }
    };
}

pub(crate) use closed_set;
