//! ChatGPT (openai) entity ids.

use datalib_id::{entity_id_str, Scope};

pub const PROVIDER: &str = "openai";

pub const KIND_CONVERSATION: &str = "conversation";
pub const KIND_MESSAGE: &str = "message";

/// An entity's identity: the id we mint, and the upstream natural key
/// it was minted from. See `claude::render::ids::Identity` — the
/// pairing exists so `upstream_id` and `uuid` cannot drift apart.
#[derive(Debug, Clone)]
pub struct Identity {
    pub uuid: String,
    pub natural_key: String,
    pub entity_kind: &'static str,
}

fn identity(entity_kind: &'static str, natural_key: String) -> Identity {
    Identity {
        uuid: entity_id_str(PROVIDER, Scope::ProviderGlobal, entity_kind, &natural_key),
        natural_key,
        entity_kind,
    }
}

pub fn conversation(conversation_id: &str) -> Identity {
    identity(KIND_CONVERSATION, conversation_id.to_string())
}

pub fn message(message_id: &str) -> Identity {
    identity(KIND_MESSAGE, message_id.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upstream_ids_no_longer_leak_into_our_keyspace() {
        // The fixture's ids are the shape that put `openai` on
        // NON_UUID_PK_PROVIDERS in the first place.
        for got in [
            conversation("68fa0001-fake-7000-8000-positronic0001"),
            message("msg-fake-poly-0001"),
        ] {
            assert_eq!(got.uuid.len(), 36, "{}", got.uuid);
            assert!(
                got.uuid.chars().all(|c| c.is_ascii_hexdigit() || c == '-'),
                "{} must be hex+dashes",
                got.uuid,
            );
        }
    }

    /// `upstream_id` must regenerate `uuid`; see the equivalent
    /// test in the claude ids module.
    #[test]
    fn natural_key_regenerates_the_uuid() {
        for got in [conversation("c1"), message("m1")] {
            assert_eq!(
                got.uuid,
                entity_id_str(
                    PROVIDER,
                    Scope::ProviderGlobal,
                    got.entity_kind,
                    &got.natural_key
                ),
            );
        }
    }

    #[test]
    fn kinds_separate_ids_over_the_same_key() {
        // OpenAI does not promise its conversation and message id
        // spaces are disjoint, and we no longer depend on it.
        assert_ne!(conversation("x").uuid, message("x").uuid);
    }
}
