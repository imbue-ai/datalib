//! LinkedIn entity ids. A connection is its profile URL, unique across
//! LinkedIn; a message, share or comment row is the id the ingest
//! minted from its export row; a post thread is the post's link. All
//! provider-global. On the ingest side because a connection's id is
//! also its raw row's key, which the photo fetch joins on.

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

fn identity(entity_kind: &'static str, natural_key: String, date_ms: Option<i64>) -> Identity {
    Identity::mint(
        ID_NAMESPACE,
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
pub fn connection(url: &str) -> Identity {
    identity(KIND_CONNECTION, url.to_string(), None)
}

/// A connection row with no profile URL: keyed on name and company so
/// distinct people do not collapse onto one empty-URL id.
pub fn connection_without_url(name: &str, company: &str) -> Identity {
    identity(KIND_CONNECTION, composite_key(&[name, company]), None)
}

pub fn connections_group() -> Identity {
    identity(KIND_CONNECTIONS_GROUP, "connections".to_string(), None)
}

/// `table` is the export feed (`messages`, `inmail`, …), whose
/// conversation ids are their own sequences.
pub fn conversation(table: &str, conversation_id: &str) -> Identity {
    identity(
        KIND_CONVERSATION,
        composite_key(&[table, conversation_id]),
        None,
    )
}

pub fn message(table: &str, row_id: &str, date_ms: Option<i64>) -> Identity {
    identity(KIND_MESSAGE, composite_key(&[table, row_id]), date_ms)
}

pub fn post(key: &str) -> Identity {
    identity(KIND_POST, key.to_string(), None)
}

pub fn share(row_id: &str, date_ms: Option<i64>) -> Identity {
    identity(KIND_SHARE, row_id.to_string(), date_ms)
}

pub fn comment(row_id: &str, date_ms: Option<i64>) -> Identity {
    identity(KIND_COMMENT, row_id.to_string(), date_ms)
}

/// The placeholder for a post the export left out but commented on.
pub fn post_origin(key: &str, date_ms: Option<i64>) -> Identity {
    identity(KIND_POST_ORIGIN, key.to_string(), date_ms)
}

#[cfg(test)]
mod tests {
    use super::*;
    use datalib_id::{entity_id_str, stamp_of};

    const MS: Option<i64> = Some(1_700_000_000_999);

    #[test]
    fn natural_key_regenerates_the_uuid() {
        for got in [
            connection("https://www.linkedin.com/in/x"),
            connection_without_url("Ann", "Acme"),
            connections_group(),
            conversation("messages", "c1"),
            message("messages", "r1", MS),
            post("https://www.linkedin.com/posts/x"),
            share("r2", MS),
            comment("r3", MS),
            post_origin("k", MS),
        ] {
            assert_eq!(
                got.uuid,
                entity_id_str(
                    ID_NAMESPACE,
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
            stamp_of(&message("m", "r", MS).uuid),
            Some(1_700_000_000_000)
        );
        assert_eq!(stamp_of(&connection("u").uuid), None);
        assert_eq!(stamp_of(&post("k").uuid), None);
    }
}
