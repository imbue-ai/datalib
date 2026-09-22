//! Facebook entity ids. Every export record is keyed by the id the
//! ingest minted for its row — Facebook's own `fbid` where the record
//! carries one, else a hash of the record — and those are unique across
//! the export, so the scope is provider-global. The feeds datalib
//! composes (the comments and reactions timelines, the friends list)
//! are keyed on their names.

use datalib_id::{composite_key, IdNamespace, Identity, Scope};
use datalib_time::RecordStampPrecision;

pub const ID_NAMESPACE: IdNamespace = IdNamespace::Facebook;
pub const STAMP_PRECISION: RecordStampPrecision = RecordStampPrecision::Seconds;

pub const KIND_POST: &str = "post";
pub const KIND_POST_TEXT: &str = "post_text";
pub const KIND_ALBUM: &str = "album";
pub const KIND_ALBUM_DESCRIPTION: &str = "album_description";
pub const KIND_PHOTO: &str = "photo";
pub const KIND_FEED: &str = "feed";
pub const KIND_FEED_MONTH: &str = "feed_month";
pub const KIND_COMMENT: &str = "comment";
pub const KIND_REACTION: &str = "reaction";
pub const KIND_FRIEND: &str = "friend";
pub const KIND_FRIENDS_GROUP: &str = "friends_group";

fn identity(entity_kind: &'static str, natural_key: String, date_ms: Option<i64>) -> Identity {
    Identity::mint(
        ID_NAMESPACE,
        Scope::ProviderGlobal,
        entity_kind,
        natural_key,
        STAMP_PRECISION.stored_ms(date_ms),
    )
}

pub fn post(row_id: &str) -> Identity {
    identity(KIND_POST, row_id.to_string(), None)
}

/// The one item a post's document holds.
pub fn post_text(row_id: &str, date_ms: Option<i64>) -> Identity {
    identity(KIND_POST_TEXT, row_id.to_string(), date_ms)
}

pub fn album(row_id: &str) -> Identity {
    identity(KIND_ALBUM, row_id.to_string(), None)
}

pub fn album_description(row_id: &str, date_ms: Option<i64>) -> Identity {
    identity(KIND_ALBUM_DESCRIPTION, row_id.to_string(), date_ms)
}

pub fn photo(album_row_id: &str, uri: &str, date_ms: Option<i64>) -> Identity {
    identity(KIND_PHOTO, composite_key(&[album_row_id, uri]), date_ms)
}

/// A timeline datalib composes — `comments`, `reactions`.
pub fn feed(name: &str) -> Identity {
    identity(KIND_FEED, name.to_string(), None)
}

pub fn feed_month(name: &str, period_key: &str) -> Identity {
    identity(KIND_FEED_MONTH, composite_key(&[name, period_key]), None)
}

pub fn comment(row_id: &str, date_ms: Option<i64>) -> Identity {
    identity(KIND_COMMENT, row_id.to_string(), date_ms)
}

/// One reaction may arrive as two export rows; the item is keyed on
/// every row it folds together.
pub fn reaction(row_ids: &[&str], date_ms: Option<i64>) -> Identity {
    identity(KIND_REACTION, composite_key(row_ids), date_ms)
}

/// No stamp: "friends since" is the row's stamp, but it is the day of
/// the friendship at second precision as the export gives it — kept
/// out so a friend row does not move when the export re-states it.
pub fn friend(row_id: &str) -> Identity {
    identity(KIND_FRIEND, row_id.to_string(), None)
}

pub fn friends_group() -> Identity {
    identity(KIND_FRIENDS_GROUP, "friends".to_string(), None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use datalib_id::{entity_id_str, stamp_of};

    const MS: Option<i64> = Some(1_700_000_000_999);

    #[test]
    fn natural_key_regenerates_the_uuid() {
        for got in [
            post("r1"),
            post_text("r1", MS),
            album("r2"),
            album_description("r2", MS),
            photo("r2", "photos/a.jpg", MS),
            feed("comments"),
            feed_month("comments", "2024-03"),
            comment("r3", MS),
            reaction(&["r4", "r5"], MS),
            friend("r6"),
            friends_group(),
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
    fn a_post_and_its_text_differ_over_one_row() {
        assert_ne!(post("r").uuid, post_text("r", None).uuid);
        assert_eq!(stamp_of(&post_text("r", MS).uuid), Some(1_700_000_000_000));
        assert_eq!(stamp_of(&post("r").uuid), None);
    }
}
