//! Raw-store schema for the Facebook export provider: how an export file
//! becomes a table name, the one CAS edge table, and the uuid namespace.
//! Every JSON file in the export lands as its own table (one row per
//! record), so the table names are mechanical rather than a manifest —
//! except Messenger's, whose conversations all land in two tables
//! (`messenger.rs`).

use datalib_etl_macros::CasEdgeRow;
use datalib_store_meta::Migration;
use uuid::Uuid;

/// The tables the render side reads, each pinned by a test to the
/// [`canonical_table`] of the path Facebook ships it at.
pub const POSTS_TABLE: &str = "your_facebook_activity_posts_your_posts_check_ins_photos_and_videos";
pub const OTHER_POSTS_TABLE: &str =
    "your_facebook_activity_posts_posts_on_other_pages_and_profiles";
pub const ALBUMS_TABLE: &str = "your_facebook_activity_posts_album";
pub const COMMENTS_TABLE: &str = "your_facebook_activity_comments_and_reactions_comments";
pub const REACTIONS_TABLE: &str =
    "your_facebook_activity_comments_and_reactions_likes_and_reactions";
pub const FRIENDS_TABLE: &str = "connections_friends_your_friends";
pub const PROFILE_TABLE: &str = "personal_information_profile_information_profile_information";

/// One row per Messenger conversation, keyed by its id; and one per
/// message, keyed `<thread id>:<timestamp_ms>:<n>`. No export path names
/// these, so no `canonical_table` can collide with them.
pub const MESSENGER_THREADS_TABLE: &str = "messenger_threads";
pub const MESSENGER_MESSAGES_TABLE: &str = "messenger_messages";

/// Each rung runs once on a store from before it; the README beside the
/// etl crate (§"The migration ladder") has the rules.
pub const LADDER: &[Migration] = &[Migration {
    version: 1,
    name: "Messenger conversations move to messenger_threads and messenger_messages",
    apply: |conn| Box::pin(super::migrate::messenger_tables(conn)),
}];

/// `media_blobs` — the CAS edge from a record to each media file it
/// references by `uri`. Owning entity: the record's row id. Ref: the
/// export-relative `uri` exactly as the JSON spells it.
#[derive(Debug, Clone, CasEdgeRow)]
#[cas_edge_row(table = "media_blobs")]
pub struct MediaBlobRow {
    pub id: String,
    pub owner_id: String,
    pub uri: String,
    pub blake3: Option<String>,
}

/// The tables every store has: the media edge, and `ingested_files`,
/// which names the chunk files each record table was last read from.
pub fn store_ddl() -> Vec<String> {
    use datalib_etl::blob_cas::CasEdgeRow as _;
    use datalib_etl::bulk::BulkUpsertable as _;
    let mut out = MediaBlobRow::all_ddl();
    out.push(datalib_etl::doltlite_raw::bookkeeping_ddl_for(
        MediaBlobRow::TABLE,
    ));
    out.push(datalib_etl_files::file_checkpoint::INGESTED_FILES_DDL.to_string());
    out
}

/// The raw table for an export file, from its path relative to the
/// export root. The trailing `_<digits>` it drops is the chunk index
/// Facebook splits a long file on
/// (`your_posts__check_ins__photos_and_videos_1.json`, `album/0.json`),
/// and every chunk belongs in the one table.
pub use datalib_etl_files::export_files::table_name as canonical_table;

pub fn facebook_ns() -> Uuid {
    Uuid::new_v5(&Uuid::NAMESPACE_DNS, b"facebook.datalib")
}

pub fn ns_id(recipe: &str) -> String {
    Uuid::new_v5(&facebook_ns(), recipe.as_bytes())
        .as_hyphenated()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_render_tables_are_the_export_paths() {
        assert_eq!(
            canonical_table(
                "your_facebook_activity/posts/your_posts__check_ins__photos_and_videos_1.json"
            ),
            POSTS_TABLE
        );
        assert_eq!(
            canonical_table("your_facebook_activity/posts/posts_on_other_pages_and_profiles.json"),
            OTHER_POSTS_TABLE
        );
        assert_eq!(
            canonical_table("your_facebook_activity/posts/album/0.json"),
            ALBUMS_TABLE
        );
        assert_eq!(
            canonical_table("your_facebook_activity/posts/album/1.json"),
            ALBUMS_TABLE
        );
        assert_eq!(
            canonical_table("your_facebook_activity/comments_and_reactions/comments.json"),
            COMMENTS_TABLE
        );
        assert_eq!(
            canonical_table(
                "your_facebook_activity/comments_and_reactions/likes_and_reactions_1.json"
            ),
            REACTIONS_TABLE
        );
        assert_eq!(
            canonical_table(
                "your_facebook_activity/comments_and_reactions/likes_and_reactions.json"
            ),
            REACTIONS_TABLE
        );
        assert_eq!(
            canonical_table("connections/friends/your_friends.json"),
            FRIENDS_TABLE
        );
        assert_eq!(
            canonical_table("personal_information/profile_information/profile_information.json"),
            PROFILE_TABLE
        );
    }

    #[test]
    fn a_trailing_word_that_is_not_a_chunk_index_stays() {
        assert_eq!(
            canonical_table("ads_information/story_views_in_past_7_days.json"),
            "ads_information_story_views_in_past_7_days"
        );
        assert_eq!(
            canonical_table("security_and_login_information/where_you're_logged_in.json"),
            "security_and_login_information_where_you_re_logged_in"
        );
        assert_eq!(canonical_table("7.json"), "t_7");
    }

    #[test]
    fn namespace_is_stable() {
        assert_eq!(ns_id("post:a"), ns_id("post:a"));
        assert_ne!(ns_id("post:a"), ns_id("post:b"));
        assert_eq!(ns_id("x").len(), 36);
    }
}
