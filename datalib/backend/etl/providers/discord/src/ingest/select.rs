//! What the config names, matched against what Discord lists: servers,
//! channels, and direct messages.

use serde_json::Value;

use datalib_etl::download_problems::{resolve_configured, DownloadProblem};

use super::schema_raw::{is_text, is_thread_parent, CATEGORY};

/// The last id in a pasted link (`discord.com/channels/<server>/<channel>`),
/// or the entry itself.
fn named(entry: &str) -> &str {
    let entry = entry.trim();
    match entry.split_once("/channels/") {
        Some((_, path)) => path
            .split(['/', '?', '#'])
            .filter(|p| !p.is_empty())
            .nth(1)
            .unwrap_or(entry),
        None => entry.strip_prefix('#').unwrap_or(entry),
    }
}

fn text<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(Value::as_str).unwrap_or("")
}

fn is_named(v: &Value, entry: &str) -> bool {
    let entry = named(entry);
    text(v, "id") == entry || text(v, "name").to_lowercase() == entry.to_lowercase()
}

/// The servers to copy, and the configured entries Discord lists no
/// server for.
pub fn servers(
    listed: &[Value],
    wanted: Option<&[String]>,
    all: bool,
) -> (Vec<Value>, Vec<DownloadProblem>) {
    let resolution = resolve_configured("servers", wanted.unwrap_or_default(), |entry| {
        let matches: Vec<&Value> = listed.iter().filter(|g| is_named(g, entry)).collect();
        if matches.is_empty() {
            let names: Vec<&str> = listed.iter().map(|g| text(g, "name")).collect();
            return Err(format!(
                "the account is in no such server; it is in: {}",
                names.join(", ")
            ));
        }
        Ok(matches)
    });
    if all {
        return (listed.to_vec(), resolution.problems);
    }
    let chosen: Vec<&str> = resolution
        .resolved
        .iter()
        .flatten()
        .map(|g| text(g, "id"))
        .collect();
    let servers = listed
        .iter()
        .filter(|g| chosen.contains(&text(g, "id")))
        .cloned()
        .collect();
    (servers, resolution.problems)
}

/// Of one server's channels, the ones the config takes: every one, or
/// the ones it names itself or by their category.
pub fn channels<'a>(listed: &'a [Value], wanted: Option<&[String]>) -> Vec<&'a Value> {
    let taken: Vec<&Value> = listed
        .iter()
        .filter(|c| is_text(c) || is_thread_parent(c))
        .collect();
    let Some(wanted) = wanted else {
        return taken;
    };
    let categories: Vec<&str> = listed
        .iter()
        .filter(|c| c.get("type").and_then(Value::as_i64) == Some(CATEGORY))
        .filter(|c| wanted.iter().any(|w| is_named(c, w)))
        .map(|c| text(c, "id"))
        .collect();
    taken
        .into_iter()
        .filter(|c| {
            wanted.iter().any(|w| is_named(c, w)) || categories.contains(&text(c, "parent_id"))
        })
        .collect()
}

/// The configured channels no server copied lists.
pub fn unknown_channels(listed: &[Value], wanted: Option<&[String]>) -> Vec<DownloadProblem> {
    resolve_configured("channels", wanted.unwrap_or_default(), |entry| {
        if listed.iter().any(|c| is_named(c, entry)) {
            Ok(())
        } else {
            Err("no server being copied has a channel by that name or id".to_string())
        }
    })
    .problems
}

/// The direct messages to copy.
pub fn dms(listed: &[Value], wanted: Option<&[String]>) -> (Vec<Value>, Vec<DownloadProblem>) {
    let Some(wanted) = wanted else {
        return (listed.to_vec(), Vec::new());
    };
    let resolution = resolve_configured("dm_conversations", wanted, |entry| {
        let id = named(entry);
        listed
            .iter()
            .find(|c| text(c, "id") == id)
            .cloned()
            .ok_or_else(|| {
                "the account's list of direct messages has no such conversation; one closed in \
                 Discord leaves the list until a message opens it again"
                    .to_string()
            })
    });
    (resolution.resolved, resolution.problems)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn strings(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    fn ids(v: &[&Value]) -> Vec<String> {
        v.iter().map(|c| text(c, "id").to_string()).collect()
    }

    #[test]
    fn a_server_is_named_by_id_or_by_name_whatever_its_case() {
        let listed = [
            json!({"id": "1", "name": "Starfleet Academy"}),
            json!({"id": "2", "name": "Ten Forward"}),
        ];
        let (got, problems) = servers(&listed, Some(&strings(&["starfleet academy", "2"])), false);
        assert_eq!((got.len(), problems.len()), (2, 0));
        let (got, problems) = servers(&listed, Some(&strings(&["Deep Space 9"])), false);
        assert!(got.is_empty());
        assert!(problems[0].detail.contains("Ten Forward"), "{problems:?}");
        assert_eq!(servers(&listed, None, true).0.len(), 2);
        assert!(servers(&listed, None, false).0.is_empty());
    }

    fn server() -> Vec<Value> {
        vec![
            json!({"id": "10", "type": 4, "name": "Bridge"}),
            json!({"id": "11", "type": 0, "name": "log", "parent_id": "10"}),
            json!({"id": "12", "type": 15, "name": "ideas", "parent_id": "10"}),
            json!({"id": "13", "type": 0, "name": "engineering", "parent_id": null}),
            json!({"id": "14", "type": 20, "name": "something new"}),
        ]
    }

    #[test]
    fn with_no_list_every_channel_that_holds_messages_is_taken() {
        assert_eq!(ids(&channels(&server(), None)), ["11", "12", "13"]);
    }

    #[test]
    fn a_channel_is_named_by_name_id_link_or_category() {
        let take = |w: &[&str]| ids(&channels(&server(), Some(&strings(w))));
        assert_eq!(take(&["#Engineering"]), ["13"]);
        assert_eq!(take(&["12"]), ["12"]);
        assert_eq!(take(&["https://discord.com/channels/1/13"]), ["13"]);
        assert_eq!(take(&["https://discord.com/channels/1/13/99?x=1"]), ["13"]);
        assert_eq!(take(&["bridge"]), ["11", "12"]);
        let unknown = unknown_channels(&server(), Some(&strings(&["holodeck", "log"])));
        assert_eq!(unknown.len(), 1);
        assert_eq!(unknown[0].value, "holodeck");
    }

    #[test]
    fn a_dm_is_named_by_its_id_or_its_link() {
        let listed = [json!({"id": "900"}), json!({"id": "901"})];
        let (got, problems) = dms(
            &listed,
            Some(&strings(&["https://discord.com/channels/@me/901", "902"])),
        );
        assert_eq!(got.len(), 1);
        assert_eq!(text(&got[0], "id"), "901");
        assert_eq!(problems[0].value, "902");
        assert_eq!(dms(&listed, None).0.len(), 2);
    }
}
