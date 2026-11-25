//! Discord's ids. A snowflake is a 64-bit number whose top 42 bits are
//! the milliseconds since 2015-01-01 UTC, so ids sort as their records'
//! times do, and an id says when its record was made.

/// 2015-01-01T00:00:00Z, the instant snowflakes count from.
pub const EPOCH_MS: i64 = 1_420_070_400_000;

pub fn parse(id: &str) -> Option<u64> {
    (!id.is_empty() && id.bytes().all(|b| b.is_ascii_digit()))
        .then(|| id.parse().ok())
        .flatten()
}

/// `id` at the one width every snowflake fits, so that keys sort as the
/// numbers do: ids have been 17, 18 and 19 digits long.
pub fn key(id: u64) -> String {
    format!("{id:020}")
}

pub fn from_key(key: &str) -> Option<u64> {
    parse(key)
}

/// When the record with this id was made, as Unix milliseconds.
pub fn unix_ms(id: u64) -> i64 {
    (id >> 22) as i64 + EPOCH_MS
}

/// The least id a record made at `ms` or later can have.
pub fn floor_at(ms: i64) -> u64 {
    ((ms - EPOCH_MS).max(0) as u64) << 22
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Discord's own worked example (its API reference, "Snowflake IDs").
    #[test]
    fn an_id_says_when_its_record_was_made() {
        assert_eq!(unix_ms(175_928_847_299_117_063), 1_462_015_105_796);
        let floor = floor_at(1_462_015_105_796);
        assert!(floor <= 175_928_847_299_117_063);
        assert_eq!(unix_ms(floor), 1_462_015_105_796);
        assert!(floor_at(1_462_015_105_797) > 175_928_847_299_117_063);
    }

    #[test]
    fn a_time_before_discord_is_the_bottom_of_every_history() {
        assert_eq!(floor_at(0), 0);
    }

    #[test]
    fn keys_sort_as_the_numbers_do() {
        let (short, long) = (99_999_999_999_999_999, 1_000_000_000_000_000_000);
        assert!(short.to_string() > long.to_string());
        assert!(key(short) < key(long));
        assert_eq!(from_key(&key(long)), Some(long));
    }

    #[test]
    fn only_digits_are_an_id() {
        assert_eq!(parse("1701"), Some(1701));
        for no in ["", "17-01", "bridge", "99999999999999999999999"] {
            assert_eq!(parse(no), None, "{no}");
        }
    }
}
