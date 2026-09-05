//! Slack entity ids.

use datalib_id::{composite_key, entity_id_str, Scope};

pub const PROVIDER: &str = "slack";

pub const KIND_THREAD: &str = "thread";
pub const KIND_MESSAGE: &str = "message";
pub const KIND_REACTION: &str = "reaction";

/// An entity's identity: the id we mint, and the upstream natural key
/// it was minted from. Paired so `grid_rows.upstream_id` and
/// `uuid` cannot drift — see `claude::render::ids::Identity`.
#[derive(Debug, Clone)]
pub struct Identity {
    pub uuid: String,
    pub natural_key: String,
    pub entity_kind: &'static str,
}

fn identity(team_id: &str, entity_kind: &'static str, natural_key: String) -> Identity {
    Identity {
        uuid: entity_id_str(
            PROVIDER,
            Scope::Upstream(team_id),
            entity_kind,
            &natural_key,
        ),
        natural_key,
        entity_kind,
    }
}

pub fn thread(team_id: &str, channel_id: &str, thread_ts: &str) -> Identity {
    identity(
        team_id,
        KIND_THREAD,
        composite_key(&[channel_id, thread_ts]),
    )
}

pub fn message(team_id: &str, channel_id: &str, ts: &str) -> Identity {
    identity(team_id, KIND_MESSAGE, composite_key(&[channel_id, ts]))
}

pub fn reaction(team_id: &str, channel_id: &str, ts: &str, name: &str, user: &str) -> Identity {
    identity(
        team_id,
        KIND_REACTION,
        composite_key(&[channel_id, ts, name, user]),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The collision the *documented* recipe had: a thread root's
    /// `thread_ts` equals its own `ts`, so a recipe without a
    /// thread/message discriminator gives one id for two entities.
    #[test]
    fn a_thread_root_and_its_own_message_differ() {
        let ts = "1700000000.000100";
        assert_ne!(
            thread("T1", "C1", ts).uuid,
            message("T1", "C1", ts).uuid,
            "thread root and its message must not share an id"
        );
    }

    #[test]
    fn workspaces_are_separated() {
        // `channel_id` is unique per workspace, not globally.
        assert_ne!(
            message("T_A", "C1", "1.1").uuid,
            message("T_B", "C1", "1.1").uuid,
        );
    }

    #[test]
    fn natural_key_regenerates_the_uuid() {
        for (team, got) in [
            ("T1", thread("T1", "C1", "1.1")),
            ("T1", message("T1", "C1", "1.1")),
            ("T1", reaction("T1", "C1", "1.1", "wave", "U1")),
        ] {
            assert_eq!(
                got.uuid,
                entity_id_str(
                    PROVIDER,
                    Scope::Upstream(team),
                    got.entity_kind,
                    &got.natural_key
                ),
            );
        }
    }

    /// A reaction's per-user row and the aggregate row (empty `user`)
    /// are distinct, and two emoji on one message are distinct.
    #[test]
    fn reactions_separate_by_user_and_emoji() {
        assert_ne!(
            reaction("T1", "C1", "1.1", "wave", "U1").uuid,
            reaction("T1", "C1", "1.1", "wave", "").uuid,
        );
        assert_ne!(
            reaction("T1", "C1", "1.1", "wave", "U1").uuid,
            reaction("T1", "C1", "1.1", "tada", "U1").uuid,
        );
    }
}
