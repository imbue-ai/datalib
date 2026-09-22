//! LinkedIn entity ids. A connection is its profile URL, unique across
//! LinkedIn; a message, share or comment row is the id the ingest
//! minted from its export row; a post thread is the post's link. All
//! provider-global.

use datalib_id::{composite_key, IdNamespace, Identity, Scope};
use datalib_time::RecordStampPrecision;

pub const ID_NAMESPACE: IdNamespace = IdNamespace::Linkedin;
pub const STAMP_PRECISION: RecordStampPrecision = RecordStampPrecision::Seconds;

pub const KIND_CONNECTION: &str = "connection";
pub const KIND_CONNECTIONS_GROUP: &str = "connections_group";
pub const KIND_CONVERSATION: &str = "conversation";
pub const KIND_MESSAGE: &str = "message";
pub const KIND_POST: &str = "post";
pub const KIND_SHARE: &str = "share";
pub const KIND_COMMENT: &str = "comment";
pub const KIND_POST_ORIGIN: &str = "post_origin";

fn identity(
    source_id: &str,
    entity_kind: &'static str,
    natural_key: String,
    date_ms: Option<i64>,
) -> Identity {
    Identity::mint(
        ID_NAMESPACE,
        source_id,
        Scope::ProviderGlobal,
        entity_kind,
        natural_key,
        STAMP_PRECISION.stored_ms(date_ms),
    )
}

/// No stamp: a connection's `created_at` is the day it was made, but a
/// "Connected On" of `16 Jun 2026` is a date the export gives without
/// a time, so the row's stamp is a fabricated midnight and not the
/// record's own.
pub fn connection(source_id: &str, url: &str) -> Identity {
    identity(source_id, KIND_CONNECTION, url.to_string(), None)
}

/// A connection row with no profile URL: keyed on name and company so
/// distinct people do not collapse onto one empty-URL id.
pub fn connection_without_url(source_id: &str, name: &str, company: &str) -> Identity {
    identity(
        source_id,
        KIND_CONNECTION,
        composite_key(&[name, company]),
        None,
    )
}

pub fn connections_group(source_id: &str) -> Identity {
    identity(
        source_id,
        KIND_CONNECTIONS_GROUP,
        "connections".to_string(),
        None,
    )
}

/// `table` is the export feed (`messages`, `inmail`, …), whose
/// conversation ids are their own sequences.
pub fn conversation(source_id: &str, table: &str, conversation_id: &str) -> Identity {
    identity(
        source_id,
        KIND_CONVERSATION,
        composite_key(&[table, conversation_id]),
        None,
    )
}

pub fn message(source_id: &str, table: &str, row_id: &str, date_ms: Option<i64>) -> Identity {
    identity(
        source_id,
        KIND_MESSAGE,
        composite_key(&[table, row_id]),
        date_ms,
    )
}

pub fn post(source_id: &str, key: &str) -> Identity {
    identity(source_id, KIND_POST, key.to_string(), None)
}

pub fn share(source_id: &str, row_id: &str, date_ms: Option<i64>) -> Identity {
    identity(source_id, KIND_SHARE, row_id.to_string(), date_ms)
}

pub fn comment(source_id: &str, row_id: &str, date_ms: Option<i64>) -> Identity {
    identity(source_id, KIND_COMMENT, row_id.to_string(), date_ms)
}

/// The placeholder for a post the export left out but commented on.
pub fn post_origin(source_id: &str, key: &str, date_ms: Option<i64>) -> Identity {
    identity(source_id, KIND_POST_ORIGIN, key.to_string(), date_ms)
}

#[cfg(test)]
mod tests {
    use super::*;
    use datalib_id::{entity_id_str, stamp_of};

    const MS: Option<i64> = Some(1_700_000_000_999);

    #[test]
    fn natural_key_regenerates_the_uuid() {
        for got in [
            connection("src", "https://www.linkedin.com/in/x"),
            connection_without_url("src", "Ann", "Acme"),
            connections_group("src"),
            conversation("src", "messages", "c1"),
            message("src", "messages", "r1", MS),
            post("src", "https://www.linkedin.com/posts/x"),
            share("src", "r2", MS),
            comment("src", "r3", MS),
            post_origin("src", "k", MS),
        ] {
            assert_eq!(
                got.uuid,
                entity_id_str(
                    ID_NAMESPACE,
                    "src",
                    Scope::ProviderGlobal,
                    got.entity_kind,
                    &got.natural_key,
                    got.at,
                ),
            );
        }
    }

    #[test]
    fn items_carry_their_stamp_and_documents_none() {
        assert_eq!(
            stamp_of(&message("src", "m", "r", MS).uuid),
            Some(1_700_000_000_000)
        );
        assert_eq!(stamp_of(&connection("src", "u").uuid), None);
        assert_eq!(stamp_of(&post("src", "k").uuid), None);
    }
}
