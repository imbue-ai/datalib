//! ChatGPT (openai) entity ids.
//!
//! Every id this provider mints goes through
//! [`datalib_id::entity_id_str`]; see `docs/dev/entity_ids.md` for the
//! rule.
//!
//! ## Scope
//!
//! [`Scope::ProviderGlobal`]. OpenAI's `conversation_id` and
//! `message_id` are unique across the service, so no further scoping
//! is needed — and unlike anthropic's `org_uuid`, there is no optional
//! account field here that could tempt a scope which sometimes exists
//! and sometimes doesn't (`conv.account_id` is itself `Option`, so
//! using it would re-key rows the first time an ingest saw it).
//!
//! ## What this replaces
//!
//! Nothing was minted at all: `conversation_id` and `message_id` were
//! used verbatim as our primary keys. They are not UUIDs — the TNG
//! fixture's are `msg-fake-poly-0001`-shaped, and real ones are
//! `aaa1bbb2-...`-ish but not guaranteed — so `grid_rows.uuid` held a
//! foreign namespace inside ours, where a single upstream id reuse
//! becomes our collision. This provider was one of the two on
//! `NON_UUID_PK_PROVIDERS`.

use datalib_id::{entity_id_str, Scope};

pub const PROVIDER: &str = "openai";

pub const KIND_CONVERSATION: &str = "conversation";
pub const KIND_MESSAGE: &str = "message";

/// An entity's identity: the id we mint, and the upstream natural key
/// it was minted from. See `anthropic::render::ids::Identity` — the
/// pairing exists so `source_native_id` and `uuid` cannot drift apart.
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

/// One conversation — its grid row, its `markdown_uuid`, and the
/// `conversation_uuid` every message row carries.
pub fn conversation(conversation_id: &str) -> Identity {
    identity(KIND_CONVERSATION, conversation_id.to_string())
}

/// One message.
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

    /// `source_native_id` must regenerate `uuid`; see the equivalent
    /// test in the anthropic ids module.
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
