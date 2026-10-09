//! The period a feed's document covers. A feed of things a person did —
//! comments, reactions, videos watched, prompts — is one document per
//! year; a conversation stays one per month.

use std::collections::BTreeMap;

use crate::types::NormalizedChatItem;

/// The bucket of an item with no date.
pub const UNDATED: &str = "undated";

/// The UTC year of a stamp (`"2026"`), or [`UNDATED`].
pub fn year_of(ms: Option<i64>) -> String {
    ms.and_then(datalib_time::IsoOffsetTimestamp::from_unix_millis)
        .and_then(|t| {
            let stamp = t.to_rfc3339();
            let date = stamp.split('T').next()?;
            date.rsplitn(3, '-').last().map(str::to_string)
        })
        .unwrap_or_else(|| UNDATED.to_string())
}

/// `items` by [`year_of`], oldest first within each year.
pub fn by_year(mut items: Vec<NormalizedChatItem>) -> BTreeMap<String, Vec<NormalizedChatItem>> {
    items.sort_by_key(|i| i.date_ms);
    let mut years: BTreeMap<String, Vec<NormalizedChatItem>> = BTreeMap::new();
    for item in items {
        years.entry(year_of(item.date_ms)).or_default().push(item);
    }
    years
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_stamp_files_under_its_utc_year() {
        // 2025-12-31T23:30:00Z
        assert_eq!(year_of(Some(1_767_223_800_000)), "2025");
        // 2026-01-01T00:00:00Z
        assert_eq!(year_of(Some(1_767_225_600_000)), "2026");
        assert_eq!(year_of(Some(0)), "1970");
        assert_eq!(year_of(None), UNDATED);
    }
}
