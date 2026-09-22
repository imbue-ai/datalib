//! Slack entity ids, and the `ts` parsing they share with the render.

use datalib_id::{composite_key, IdNamespace, Identity};
use datalib_time::{IsoOffsetTimestamp, RecordStampPrecision};

pub const ID_NAMESPACE: IdNamespace = IdNamespace::Slack;
pub const STAMP_PRECISION: RecordStampPrecision = RecordStampPrecision::Seconds;

pub const KIND_THREAD: &str = "thread";
pub const KIND_MESSAGE: &str = "message";
pub const KIND_REACTION: &str = "reaction";

/// Parse a Slack `ts` — unix seconds with a fractional part, always UTC
/// (`"1728499573.123456"`) — into an offsetted instant. `None` on a
/// shape we do not recognize; the message's caller records that.
pub fn parse_slack_ts(ts: &str) -> Option<IsoOffsetTimestamp> {
    let (secs_str, frac_str) = ts.split_once('.').unwrap_or((ts, ""));
    let secs: i64 = secs_str.parse().ok()?;
    let micros: i64 = if frac_str.is_empty() {
        0
    } else {
        let mut frac = frac_str.to_string();
        if frac.len() < 6 {
            frac.push_str(&"0".repeat(6 - frac.len()));
        } else {
            frac.truncate(6);
        }
        frac.parse().ok()?
    };
    let base = IsoOffsetTimestamp::from_unix_millis(secs.checked_mul(1000)?)?;
    Some(base.bump_micros(micros))
}

pub fn ts_to_iso(ts: &str) -> Option<String> {
    parse_slack_ts(ts).map(|t| t.to_rfc3339_micros())
}

pub fn ts_to_ms(ts: &str) -> Option<i64> {
    parse_slack_ts(ts).map(|t| t.to_unix_millis())
}

/// A message's `ts` is its time as well as its key, so the stamp in a
/// message's or reaction's id comes from the `ts` in its key; nothing
/// else has to agree with it. A thread's id carries none: its row's
/// stamp is derived from its items.
fn identity(
    source_id: &str,
    team_id: &str,
    entity_kind: &'static str,
    natural_key: String,
    ts: Option<&str>,
) -> Identity {
    Identity::mint(
        ID_NAMESPACE,
        source_id,
        Some(team_id),
        entity_kind,
        natural_key,
        STAMP_PRECISION.stored_ms(ts.and_then(ts_to_ms)),
    )
}

pub fn thread(source_id: &str, team_id: &str, channel_id: &str, thread_ts: &str) -> Identity {
    identity(
        source_id,
        team_id,
        KIND_THREAD,
        composite_key(&[channel_id, thread_ts]),
        None,
    )
}

pub fn message(source_id: &str, team_id: &str, channel_id: &str, ts: &str) -> Identity {
    identity(
        source_id,
        team_id,
        KIND_MESSAGE,
        composite_key(&[channel_id, ts]),
        Some(ts),
    )
}

pub fn reaction(
    source_id: &str,
    team_id: &str,
    channel_id: &str,
    ts: &str,
    name: &str,
    user: &str,
) -> Identity {
    identity(
        source_id,
        team_id,
        KIND_REACTION,
        composite_key(&[channel_id, ts, name, user]),
        Some(ts),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use datalib_id::{entity_id_str, stamp_of};

    /// The collision the *documented* recipe had: a thread root's
    /// `thread_ts` equals its own `ts`, so a recipe without a
    /// thread/message discriminator gives one id for two entities.
    #[test]
    fn a_thread_root_and_its_own_message_differ() {
        let ts = "1700000000.000100";
        assert_ne!(
            thread("src", "T1", "C1", ts).uuid,
            message("src", "T1", "C1", ts).uuid,
            "thread root and its message must not share an id"
        );
    }

    #[test]
    fn workspaces_are_separated() {
        // `channel_id` is unique per workspace, not globally.
        assert_ne!(
            message("src", "T_A", "C1", "1.1").uuid,
            message("src", "T_B", "C1", "1.1").uuid,
        );
    }

    #[test]
    fn natural_key_regenerates_the_uuid() {
        for (team, got) in [
            ("T1", thread("src", "T1", "C1", "1.1")),
            ("T1", message("src", "T1", "C1", "1.1")),
            ("T1", reaction("src", "T1", "C1", "1.1", "wave", "U1")),
        ] {
            assert_eq!(
                got.uuid,
                entity_id_str(
                    ID_NAMESPACE,
                    "src",
                    Some(team),
                    got.entity_kind,
                    &got.natural_key,
                    got.at,
                ),
            );
        }
    }

    /// The stamp is the `ts` in the key, to the second — what the
    /// row's `created_at` stores — and a thread carries none.
    #[test]
    fn the_stamp_is_the_ts_in_the_key() {
        let ts = "1700000000.123456";
        assert_eq!(
            stamp_of(&message("src", "T1", "C1", ts).uuid),
            Some(1_700_000_000_000)
        );
        assert_eq!(
            stamp_of(&reaction("src", "T1", "C1", ts, "wave", "").uuid),
            Some(1_700_000_000_000)
        );
        assert_eq!(stamp_of(&thread("src", "T1", "C1", ts).uuid), None);
        assert_eq!(stamp_of(&message("src", "T1", "C1", "not-a-ts").uuid), None);
    }

    /// A reaction's per-user row and the aggregate row (empty `user`)
    /// are distinct, and two emoji on one message are distinct.
    #[test]
    fn reactions_separate_by_user_and_emoji() {
        assert_ne!(
            reaction("src", "T1", "C1", "1.1", "wave", "U1").uuid,
            reaction("src", "T1", "C1", "1.1", "wave", "").uuid,
        );
        assert_ne!(
            reaction("src", "T1", "C1", "1.1", "wave", "U1").uuid,
            reaction("src", "T1", "C1", "1.1", "tada", "U1").uuid,
        );
    }
}
