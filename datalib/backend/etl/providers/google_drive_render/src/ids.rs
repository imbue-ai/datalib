//! Google Drive entity ids. A file id is Drive's, unique across Drive; a
//! comment's id is unique within its file, so its natural key carries the
//! file's, as the raw store's keys do.

use datalib_id::{IdNamespace, Identity, Minter};
use datalib_time::{record_stamp_ms, RecordStampPrecision};

pub const ID_NAMESPACE: IdNamespace = IdNamespace::GoogleDrive;

pub const KIND_FILE: &str = "file";
pub const KIND_COMMENT: &str = "comment";
pub const KIND_REPLY: &str = "reply";

const IDS: Minter = Minter::new(ID_NAMESPACE, RecordStampPrecision::Millis);

/// No stamp: a fetch problem names the file by its id alone, and has to
/// find the same document.
pub fn file(source_id: &str, file_id: &str) -> Identity {
    IDS.mint(source_id, KIND_FILE, file_id.to_string(), None)
}

/// `comment_key` is the raw store's, `{file}#{comment}`.
pub fn comment(source_id: &str, comment_key: &str, created_time: Option<&str>) -> Identity {
    IDS.mint(
        source_id,
        KIND_COMMENT,
        comment_key.to_string(),
        created_time.and_then(record_stamp_ms),
    )
}

/// `{file}#{comment}#{reply}`.
pub fn reply(source_id: &str, reply_key: &str, created_time: Option<&str>) -> Identity {
    IDS.mint(
        source_id,
        KIND_REPLY,
        reply_key.to_string(),
        created_time.and_then(record_stamp_ms),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use datalib_id::entity_id_str;

    #[test]
    fn natural_key_regenerates_the_uuid() {
        let at = Some("2364-03-02T09:00:00.000Z");
        for got in [
            file("src", "1Log"),
            comment("src", "1Log#AAA", at),
            reply("src", "1Log#AAA#BBB", at),
        ] {
            assert_eq!(
                got.uuid,
                entity_id_str(
                    ID_NAMESPACE,
                    "src",
                    None,
                    got.entity_kind,
                    &got.natural_key,
                    got.at,
                ),
            );
        }
        assert_ne!(
            comment("src", "x", at).uuid,
            reply("src", "x", at).uuid,
            "the kind separates one key"
        );
    }
}
