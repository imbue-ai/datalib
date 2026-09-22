//! Claude entity ids.

use datalib_id::{composite_key, IdNamespace, Identity, Scope};
use datalib_time::RecordStampPrecision;

pub const ID_NAMESPACE: IdNamespace = IdNamespace::Claude;

/// The precision every row's `created_at` is stored at, and so the
/// precision of the stamp in its id. The profile in `render.rs` reads
/// the same constant.
pub const STAMP_PRECISION: RecordStampPrecision = RecordStampPrecision::Seconds;

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

/// `date_ms` is the item's `NormalizedChatItem::date_ms` — what its
/// row's `created_at` is stored from — so the stamp in the id is the
/// row's. A chat-level id passes `None`: its row's stamp is derived
/// from its items.
fn identity(entity_kind: &'static str, natural_key: String, date_ms: Option<i64>) -> Identity {
    Identity::mint(
        ID_NAMESPACE,
        Scope::ProviderGlobal,
        entity_kind,
        natural_key,
        STAMP_PRECISION.stored_ms(date_ms),
    )
}

pub fn conversation(conversation_uuid: &str) -> Identity {
    identity(KIND_CONVERSATION, conversation_uuid.to_string(), None)
}

pub fn message(message_uuid: &str, date_ms: Option<i64>) -> Identity {
    identity(KIND_MESSAGE, message_uuid.to_string(), date_ms)
}

/// A `thinking` block. Keyed on `(message_uuid, block_index)` — a
/// thinking block has no upstream id of its own, and its position
/// within the message is the only thing that distinguishes it from a
/// sibling.
pub fn thinking_block(message_uuid: &str, block_index: usize, date_ms: Option<i64>) -> Identity {
    identity(
        KIND_THINKING,
        composite_key(&[message_uuid, &block_index.to_string()]),
        date_ms,
    )
}

/// A `tool_use` block, keyed on `(message_uuid, tool_use_id)`.
///
/// The message scope is the fix: `tu-{tool_use_id}` was global on an id
/// Anthropic controls and we merely observe.
pub fn tool_use(message_uuid: &str, tool_use_id: &str, date_ms: Option<i64>) -> Identity {
    identity(
        KIND_TOOL_USE,
        composite_key(&[message_uuid, tool_use_id]),
        date_ms,
    )
}

pub fn tool_result(message_uuid: &str, tool_use_id: &str, date_ms: Option<i64>) -> Identity {
    identity(
        KIND_TOOL_RESULT,
        composite_key(&[message_uuid, tool_use_id]),
        date_ms,
    )
}

pub fn block_fallback(message_uuid: &str, block_index: usize, date_ms: Option<i64>) -> Identity {
    identity(
        KIND_BLOCK,
        composite_key(&[message_uuid, &block_index.to_string()]),
        date_ms,
    )
}

pub fn project(project_uuid: &str) -> Identity {
    identity(KIND_PROJECT, project_uuid.to_string(), None)
}

pub fn project_description(project_uuid: &str, date_ms: Option<i64>) -> Identity {
    identity(KIND_PROJECT_DESCRIPTION, project_uuid.to_string(), date_ms)
}

pub fn project_instructions(project_uuid: &str, date_ms: Option<i64>) -> Identity {
    identity(KIND_PROJECT_INSTRUCTIONS, project_uuid.to_string(), date_ms)
}

pub fn project_document(doc_uuid: &str, date_ms: Option<i64>) -> Identity {
    identity(KIND_PROJECT_DOCUMENT, doc_uuid.to_string(), date_ms)
}

#[cfg(test)]
mod tests {
    use super::*;
    use datalib_id::{entity_id_str, stamp_of};

    const MS: Option<i64> = Some(1_700_000_000_000);

    fn every_id() -> Vec<Identity> {
        vec![
            conversation("c1"),
            message("m1", MS),
            thinking_block("m1", 0, MS),
            tool_use("m1", "toolu_1", MS),
            tool_result("m1", "toolu_1", MS),
            block_fallback("m1", 2, MS),
            project("p1"),
            project_description("p1", MS),
            project_instructions("p1", MS),
            project_document("d1", MS),
        ]
    }

    #[test]
    fn every_id_is_uuid_shaped() {
        // The property `ingested_tng_test` asserts across the whole
        // index, pinned here per-recipe so a regression names the
        // recipe rather than just the provider.
        for got in every_id() {
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
        for got in every_id() {
            assert_eq!(
                got.uuid,
                entity_id_str(
                    ID_NAMESPACE,
                    Scope::ProviderGlobal,
                    got.entity_kind,
                    &got.natural_key,
                    got.at,
                ),
                "{} does not regenerate from ({}, {})",
                got.uuid,
                got.entity_kind,
                got.natural_key,
            );
        }
    }

    /// An item's id carries its stamp at the precision its row stores
    /// it; a chat-level id carries none.
    #[test]
    fn items_carry_their_stamp_and_chats_do_not() {
        assert_eq!(stamp_of(&message("m1", Some(1_700_000_000_999)).uuid), MS);
        assert_eq!(stamp_of(&message("m1", None).uuid), None);
        assert_eq!(stamp_of(&conversation("c1").uuid), None);
        assert_eq!(stamp_of(&project("p1").uuid), None);
    }

    #[test]
    fn kinds_separate_ids_over_the_same_key() {
        // `tu-` and `tr-` used to differ only by a two-character
        // prefix glued onto the same id; the kind component is what
        // keeps them apart now.
        assert_ne!(
            tool_use("m1", "toolu_1", MS).uuid,
            tool_result("m1", "toolu_1", MS).uuid
        );
        assert_ne!(conversation("x").uuid, message("x", None).uuid);
        assert_ne!(
            project_description("p", MS).uuid,
            project_instructions("p", MS).uuid
        );
    }

    /// The bug `tu-{tool_use_id}` had: no message scope at all, so the
    /// same tool-use id appearing under two messages — a forked or
    /// regenerated conversation branch, which this renderer emits flat
    /// because `parent_message_uuid` is unused — collided.
    #[test]
    fn tool_blocks_are_scoped_to_their_message() {
        assert_ne!(
            tool_use("msg-a", "toolu_shared", MS).uuid,
            tool_use("msg-b", "toolu_shared", MS).uuid,
        );
    }

    /// `th-{msg}-{idx}` could not be split unambiguously: `-` is both
    /// the separator and a character inside every UUID.
    #[test]
    fn block_keys_are_unambiguous() {
        assert_ne!(
            thinking_block("M", 0, MS).uuid,
            thinking_block("M-0", 0, MS).uuid
        );
        assert_ne!(
            thinking_block("M", 0, MS).uuid,
            block_fallback("M", 0, MS).uuid
        );
    }
}
