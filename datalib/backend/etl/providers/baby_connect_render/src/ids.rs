//! Baby Connect entity ids. Baby Connect's ids are unique across the
//! service for each kind of object, so a child's and an entry's natural
//! key is Baby Connect's id alone, as the raw store keys it.

use datalib_id::{IdNamespace, Identity, Minter};
use datalib_time::{record_stamp_ms, RecordStampPrecision};

pub const ID_NAMESPACE: IdNamespace = IdNamespace::BabyConnect;

pub const KIND_KID: &str = "kid";
pub const KIND_ENTRY: &str = "entry";

const IDS: Minter = Minter::new(ID_NAMESPACE, RecordStampPrecision::Millis);

/// No stamp: a child is a container, and its entries carry the times.
pub fn kid(source_id: &str, kid_id: &str) -> Identity {
    IDS.mint(source_id, KIND_KID, kid_id.to_string(), None)
}

/// An entry, stamped with the time it was logged for.
pub fn entry(source_id: &str, entry_id: &str, at: Option<&str>) -> Identity {
    IDS.mint(
        source_id,
        KIND_ENTRY,
        entry_id.to_string(),
        at.and_then(record_stamp_ms),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use datalib_id::entity_id_str;

    #[test]
    fn natural_key_regenerates_the_uuid() {
        let at = Some("2368-10-12T02:10:00-07:00");
        for got in [
            kid("src", "5629499534213121"),
            entry("src", "4785074604081121", at),
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
    }
}
