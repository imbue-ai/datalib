//! Zoom entity ids. A meeting UUID is Zoom's and names one meeting
//! instance; each part of its document is keyed by it.

use datalib_id::{IdNamespace, Identity, Minter};
use datalib_time::RecordStampPrecision;

pub const ID_NAMESPACE: IdNamespace = IdNamespace::Zoom;
pub const STAMP_PRECISION: RecordStampPrecision = RecordStampPrecision::Millis;

pub const KIND_MEETING: &str = "meeting";
pub const KIND_DETAILS: &str = "meeting_details";
pub const KIND_SUMMARY: &str = "meeting_summary";

const IDS: Minter = Minter::new(ID_NAMESPACE, STAMP_PRECISION);

/// No stamp: a fetch problem names the meeting by its UUID alone, and
/// has to find the same document.
pub fn meeting(source_id: &str, uuid: &str) -> Identity {
    IDS.mint(source_id, KIND_MEETING, uuid.to_string(), None)
}

pub fn details(source_id: &str, uuid: &str, at: Option<i64>) -> Identity {
    IDS.mint(source_id, KIND_DETAILS, uuid.to_string(), at)
}

pub fn summary(source_id: &str, uuid: &str, at: Option<i64>) -> Identity {
    IDS.mint(source_id, KIND_SUMMARY, uuid.to_string(), at)
}

#[cfg(test)]
mod tests {
    use super::*;
    use datalib_id::entity_id_str;

    #[test]
    fn natural_key_regenerates_the_uuid() {
        let at = Some(1_790_000_000_000);
        for got in [
            meeting("src", "nCC1701+D/briefing=="),
            details("src", "nCC1701+D/briefing==", at),
            summary("src", "nCC1701+D/briefing==", at),
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
            details("src", "m", at).uuid,
            summary("src", "m", at).uuid,
            "the kind separates one key"
        );
    }
}
