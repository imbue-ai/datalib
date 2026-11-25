//! Which channels of a server the account can read. Discord lists every
//! channel of a server to every member, and answers 403 for the ones the
//! member's roles do not reach, so what each is worth asking for is
//! worked out from the roles first (Discord's API reference,
//! "Permissions", § "Permission Overwrites").

use std::collections::HashMap;

use serde_json::Value;

pub const ADMINISTRATOR: u64 = 1 << 3;
pub const VIEW_CHANNEL: u64 = 1 << 10;
pub const READ_MESSAGE_HISTORY: u64 = 1 << 16;

/// The account in one server: who it is, the roles it holds, and what
/// they come to before any channel's overwrites.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Member {
    pub user_id: String,
    pub guild_id: String,
    pub roles: Vec<String>,
    pub base: u64,
}

fn bits(v: Option<&Value>) -> u64 {
    match v {
        Some(Value::String(s)) => s.parse().unwrap_or(0),
        Some(Value::Number(n)) => n.as_u64().unwrap_or(0),
        _ => 0,
    }
}

impl Member {
    /// From `/users/@me/guilds/{id}/member` and the server's roles. The
    /// role every member holds has the server's own id.
    pub fn new(user_id: &str, guild_id: &str, member: &Value, roles: &[Value]) -> Self {
        let held: Vec<String> = member
            .get("roles")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(String::from)
            .collect();
        let of_role: HashMap<&str, u64> = roles
            .iter()
            .filter_map(|r| Some((r.get("id")?.as_str()?, bits(r.get("permissions")))))
            .collect();
        let base = std::iter::once(guild_id)
            .chain(held.iter().map(String::as_str))
            .filter_map(|id| of_role.get(id))
            .fold(0, |all, p| all | p);
        Self {
            user_id: user_id.to_string(),
            guild_id: guild_id.to_string(),
            roles: held,
            base,
        }
    }

    pub fn can_read(&self, channel: &Value, is_owner: bool) -> bool {
        let wanted = VIEW_CHANNEL | READ_MESSAGE_HISTORY;
        is_owner || self.in_channel(channel) & wanted == wanted
    }

    fn in_channel(&self, channel: &Value) -> u64 {
        if self.base & ADMINISTRATOR != 0 {
            return u64::MAX;
        }
        let overwrites: HashMap<&str, (u64, u64)> = channel
            .get("permission_overwrites")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|o| {
                let id = o.get("id")?.as_str()?;
                Some((id, (bits(o.get("allow")), bits(o.get("deny")))))
            })
            .collect();
        let apply = |p: u64, (allow, deny): (u64, u64)| (p & !deny) | allow;
        let mut p = self.base;
        if let Some(o) = overwrites.get(self.guild_id.as_str()) {
            p = apply(p, *o);
        }
        let of_roles = self
            .roles
            .iter()
            .filter_map(|r| overwrites.get(r.as_str()))
            .fold((0, 0), |(a, d), (allow, deny)| (a | allow, d | deny));
        p = apply(p, of_roles);
        match overwrites.get(self.user_id.as_str()) {
            Some(o) => apply(p, *o),
            None => p,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const ME: &str = "1701";
    const GUILD: &str = "1000";

    fn read() -> String {
        (VIEW_CHANNEL | READ_MESSAGE_HISTORY).to_string()
    }

    fn member(roles: &[&str], everyone: u64, officers: u64) -> Member {
        Member::new(
            ME,
            GUILD,
            &json!({"roles": roles}),
            &[
                json!({"id": GUILD, "permissions": everyone.to_string()}),
                json!({"id": "2000", "permissions": officers.to_string()}),
            ],
        )
    }

    fn channel(overwrites: Value) -> Value {
        json!({"id": "3000", "permission_overwrites": overwrites})
    }

    #[test]
    fn what_everyone_holds_reads_a_channel_with_no_overwrites() {
        let all = VIEW_CHANNEL | READ_MESSAGE_HISTORY;
        assert!(member(&[], all, 0).can_read(&channel(json!([])), false));
        assert!(!member(&[], VIEW_CHANNEL, 0).can_read(&channel(json!([])), false));
    }

    /// The private channel: everyone denied, one role let in.
    #[test]
    fn a_role_let_into_a_private_channel_reads_it_and_nobody_else_does() {
        let all = VIEW_CHANNEL | READ_MESSAGE_HISTORY;
        let private = channel(json!([
            {"id": GUILD, "type": 0, "allow": "0", "deny": read()},
            {"id": "2000", "type": 0, "allow": read(), "deny": "0"},
        ]));
        assert!(!member(&[], all, 0).can_read(&private, false));
        assert!(member(&["2000"], all, 0).can_read(&private, false));
    }

    #[test]
    fn an_overwrite_for_the_member_has_the_last_word() {
        let all = VIEW_CHANNEL | READ_MESSAGE_HISTORY;
        let banned = channel(json!([{"id": ME, "type": 1, "allow": "0", "deny": read()}]));
        assert!(!member(&["2000"], all, all).can_read(&banned, false));
    }

    #[test]
    fn an_administrator_and_an_owner_read_everything() {
        let closed = channel(json!([{"id": GUILD, "type": 0, "allow": "0", "deny": read()}]));
        assert!(member(&["2000"], 0, ADMINISTRATOR).can_read(&closed, false));
        assert!(member(&[], 0, 0).can_read(&closed, true));
        assert!(!member(&[], 0, 0).can_read(&closed, false));
    }
}
