//! Claude entity ids.

use datalib_id::{composite_key, entity_id_str, Scope};

pub const PROVIDER: &str = "claude";

// Entity kinds — the `entity_kind` recipe component, and the value
// stamped into `grid_rows.upstream_entity_kind`. Distinct from the
// display `kind_label` ("LLM Thinking", "Tool Call"), which may be
// reworded without re-keying anything.
pub const KIND_CONVERSATION: &str = "conversation";
pub const KIND_MESSAGE: &str = "message";
pub const KIND_THINKING: &str = "thinking_block";
pub const KIND_TOOL_USE: &str = "tool_use";
pub const KIND_TOOL_RESULT: &str = "tool_result";
pub const KIND_BLOCK: &str = "content_block";
pub const KIND_PROJECT: &str = "project";
pub const KIND_PROJECT_DESCRIPTION: &str = "project_description";
pub const KIND_PROJECT_INSTRUCTIONS: &str = "project_instructions";
pub const KIND_PROJECT_DOCUMENT: &str = "project_document";

/// An entity's identity: the id we mint, and the upstream natural key
/// it was minted from.
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

pub fn conversation(conversation_uuid: &str) -> Identity {
    identity(KIND_CONVERSATION, conversation_uuid.to_string())
}

pub fn message(message_uuid: &str) -> Identity {
    identity(KIND_MESSAGE, message_uuid.to_string())
}

/// A `thinking` block. Keyed on `(message_uuid, block_index)` — a
/// thinking block has no upstream id of its own, and its position
/// within the message is the only thing that distinguishes it from a
/// sibling.
pub fn thinking_block(message_uuid: &str, block_index: usize) -> Identity {
    identity(
        KIND_THINKING,
        composite_key(&[message_uuid, &block_index.to_string()]),
    )
}

/// A `tool_use` block, keyed on `(message_uuid, tool_use_id)`.
///
/// The message scope is the fix: `tu-{tool_use_id}` was global on an id
/// Anthropic controls and we merely observe.
pub fn tool_use(message_uuid: &str, tool_use_id: &str) -> Identity {
    identity(KIND_TOOL_USE, composite_key(&[message_uuid, tool_use_id]))
}

pub fn tool_result(message_uuid: &str, tool_use_id: &str) -> Identity {
    identity(
        KIND_TOOL_RESULT,
        composite_key(&[message_uuid, tool_use_id]),
    )
}

pub fn block_fallback(message_uuid: &str, block_index: usize) -> Identity {
    identity(
        KIND_BLOCK,
        composite_key(&[message_uuid, &block_index.to_string()]),
    )
}

pub fn project(project_uuid: &str) -> Identity {
    identity(KIND_PROJECT, project_uuid.to_string())
}

pub fn project_description(project_uuid: &str) -> Identity {
    identity(KIND_PROJECT_DESCRIPTION, project_uuid.to_string())
}

pub fn project_instructions(project_uuid: &str) -> Identity {
    identity(KIND_PROJECT_INSTRUCTIONS, project_uuid.to_string())
}

pub fn project_document(doc_uuid: &str) -> Identity {
    identity(KIND_PROJECT_DOCUMENT, doc_uuid.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_id_is_uuid_shaped() {
        // The property `ingested_tng_test` asserts across the whole
        // index, pinned here per-recipe so a regression names the
        // recipe rather than just the provider.
        for got in [
            conversation("c1"),
            message("m1"),
            thinking_block("m1", 0),
            tool_use("m1", "toolu_1"),
            tool_result("m1", "toolu_1"),
            block_fallback("m1", 2),
            project("p1"),
            project_description("p1"),
            project_instructions("p1"),
            project_document("d1"),
        ] {
            assert_eq!(got.uuid.len(), 36, "{}", got.uuid);
            assert!(
                got.uuid.chars().all(|c| c.is_ascii_hexdigit() || c == '-'),
                "{} must be hex+dashes — no passed-through upstream string",
                got.uuid,
            );
        }
    }

    /// The invariant that broke once: the `natural_key` an `Identity`
    /// carries is exactly the string its `uuid` was derived from, so
    /// storing it in `upstream_id` regenerates the row.
    #[test]
    fn natural_key_regenerates_the_uuid() {
        for got in [
            conversation("c1"),
            message("m1"),
            thinking_block("m1", 0),
            tool_use("m1", "toolu_1"),
            tool_result("m1", "toolu_1"),
            block_fallback("m1", 2),
            project_document("d1"),
        ] {
            assert_eq!(
                got.uuid,
                entity_id_str(
                    PROVIDER,
                    Scope::ProviderGlobal,
                    got.entity_kind,
                    &got.natural_key
                ),
                "{} does not regenerate from ({}, {})",
                got.uuid,
                got.entity_kind,
                got.natural_key,
            );
        }
    }

    #[test]
    fn kinds_separate_ids_over_the_same_key() {
        // `tu-` and `tr-` used to differ only by a two-character
        // prefix glued onto the same id; the kind component is what
        // keeps them apart now.
        assert_ne!(
            tool_use("m1", "toolu_1").uuid,
            tool_result("m1", "toolu_1").uuid
        );
        assert_ne!(conversation("x").uuid, message("x").uuid);
        assert_ne!(
            project_description("p").uuid,
            project_instructions("p").uuid
        );
    }

    /// The bug `tu-{tool_use_id}` had: no message scope at all, so the
    /// same tool-use id appearing under two messages — a forked or
    /// regenerated conversation branch, which this renderer emits flat
    /// because `parent_message_uuid` is unused — collided.
    #[test]
    fn tool_blocks_are_scoped_to_their_message() {
        assert_ne!(
            tool_use("msg-a", "toolu_shared").uuid,
            tool_use("msg-b", "toolu_shared").uuid,
        );
    }

    /// `th-{msg}-{idx}` could not be split unambiguously: `-` is both
    /// the separator and a character inside every UUID.
    #[test]
    fn block_keys_are_unambiguous() {
        assert_ne!(thinking_block("M", 0).uuid, thinking_block("M-0", 0).uuid);
        assert_ne!(thinking_block("M", 0).uuid, block_fallback("M", 0).uuid);
    }
}
