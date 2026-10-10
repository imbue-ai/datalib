//! Messenger's files: `messages/<folder>/<thread dir>/message_<n>.json`,
//! one conversation per directory, split into a thread row and one row
//! per message. Facebook gives a message no id, so its row is keyed by
//! the conversation, its time and its place among messages sent in the
//! same millisecond.

use std::collections::HashMap;

use serde_json::{json, Value};

use super::schema_raw::{MESSENGER_MESSAGES_TABLE, MESSENGER_THREADS_TABLE};
use super::Record;

/// Where one Messenger file sits: the folder Facebook filed the
/// conversation under (`inbox`, `filtered_threads`, `message_requests`,
/// `e2ee_cutover`, …) and the conversation's directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadFile {
    pub folder: String,
    pub dir: String,
}

impl ThreadFile {
    /// `Some` for a conversation file, wherever the export puts the
    /// `messages/` directory (under `your_facebook_activity/` today, at
    /// the root in older exports).
    pub fn of(rel: &str) -> Option<Self> {
        let parts: Vec<&str> = rel.split('/').collect();
        let [.., messages, folder, dir, file] = parts.as_slice() else {
            return None;
        };
        let chunk = file.strip_prefix("message_")?.strip_suffix(".json")?;
        if *messages != "messages" || chunk.is_empty() || !chunk.bytes().all(|b| b.is_ascii_digit())
        {
            return None;
        }
        Some(Self {
            folder: (*folder).to_string(),
            dir: (*dir).to_string(),
        })
    }

    /// The conversation's id: the digits Facebook ends its directory
    /// name with (`jeanlucpicard_1234567890`, `facebookuser_…`, or the
    /// bare id when the other side has no name). The name before them
    /// follows a rename; the digits do not. A directory with no digits
    /// is its own id.
    pub fn thread_id(&self) -> String {
        let digits = self
            .dir
            .rsplit('_')
            .next()
            .filter(|tail| !tail.is_empty() && tail.bytes().all(|b| b.is_ascii_digit()));
        digits.unwrap_or(&self.dir).to_string()
    }

    /// The thread row and one row per message.
    /// The thread's payload is the file without its `messages`; each
    /// message's is the message as Facebook wrote it, beside the id of
    /// the thread it belongs to.
    pub fn rows(&self, file: Value) -> anyhow::Result<Vec<Record>> {
        let thread_id = self.thread_id();
        let Value::Object(mut thread) = file else {
            anyhow::bail!("a Messenger file that is not an object");
        };
        let messages = match thread.remove("messages") {
            Some(Value::Array(items)) => items,
            Some(other) => anyhow::bail!(
                "a Messenger file whose messages are not a list: {}",
                kind_of(&other)
            ),
            None => anyhow::bail!("a Messenger file with no messages"),
        };
        let mut out = Vec::with_capacity(messages.len() + 1);
        out.push(Record {
            table: MESSENGER_THREADS_TABLE.to_string(),
            id: thread_id.clone(),
            payload: json!({"thread_id": thread_id, "folder": self.folder, "thread": thread}),
        });
        // Facebook lists a conversation newest first; counting from the
        // oldest keeps a message's key when newer ones arrive.
        let mut seen: HashMap<i64, usize> = HashMap::new();
        for message in messages.into_iter().rev() {
            let ms = message
                .get("timestamp_ms")
                .and_then(Value::as_i64)
                .unwrap_or(0);
            let n = seen.entry(ms).or_default();
            out.push(Record {
                table: MESSENGER_MESSAGES_TABLE.to_string(),
                id: message_id(&thread_id, ms, *n),
                payload: json!({"thread_id": thread_id, "message": message}),
            });
            *n += 1;
        }
        Ok(out)
    }
}

fn kind_of(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "a bool",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "a list",
        Value::Object(_) => "an object",
    }
}

pub fn message_id(thread_id: &str, timestamp_ms: i64, n: usize) -> String {
    format!("{thread_id}:{timestamp_ms:013}:{n}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_conversation_file_is_found_in_every_folder_and_at_the_old_root() {
        let inbox = ThreadFile::of("your_facebook_activity/messages/inbox/worf_123/message_1.json");
        assert_eq!(
            inbox,
            Some(ThreadFile {
                folder: "inbox".into(),
                dir: "worf_123".into()
            })
        );
        assert!(ThreadFile::of("messages/filtered_threads/q_9/message_2.json").is_some());
        assert!(
            ThreadFile::of("your_facebook_activity/messages/messaging_settings.json").is_none()
        );
        assert!(
            ThreadFile::of("your_facebook_activity/messages/inbox/worf_123/photos/1.json")
                .is_none()
        );
        assert!(
            ThreadFile::of("your_facebook_activity/messages/inbox/worf_123/message_.json")
                .is_none()
        );
    }

    #[test]
    fn the_thread_id_is_the_trailing_digits_else_the_directory() {
        let id = |dir: &str| {
            ThreadFile {
                folder: "inbox".into(),
                dir: dir.into(),
            }
            .thread_id()
        };
        assert_eq!(id("jeanlucpicard_1234567890"), "1234567890");
        assert_eq!(id("facebookuser_42"), "42");
        assert_eq!(id("_77"), "77");
        assert_eq!(id("88"), "88");
        assert_eq!(id("tenforward"), "tenforward");
    }

    /// Facebook lists newest first; a message's key must not move when a
    /// newer message, or another in the same millisecond, is added.
    #[test]
    fn a_message_keeps_its_key_when_newer_messages_arrive() {
        let at = ThreadFile {
            folder: "inbox".into(),
            dir: "worf_5".into(),
        };
        let file = |msgs: Value| json!({"participants": [], "messages": msgs});
        let before = at
            .rows(file(json!([
                {"timestamp_ms": 2000, "content": "b"},
                {"timestamp_ms": 1000, "content": "a2"},
                {"timestamp_ms": 1000, "content": "a1"},
            ])))
            .unwrap();
        let after = at
            .rows(file(json!([
                {"timestamp_ms": 3000, "content": "c"},
                {"timestamp_ms": 2000, "content": "b"},
                {"timestamp_ms": 1000, "content": "a2"},
                {"timestamp_ms": 1000, "content": "a1"},
            ])))
            .unwrap();
        let key_of = |rows: &[Record], content: &str| {
            rows.iter()
                .find(|r| r.payload["message"]["content"] == content)
                .map(|r| r.id.clone())
                .unwrap()
        };
        for content in ["a1", "a2", "b"] {
            assert_eq!(key_of(&before, content), key_of(&after, content));
        }
        assert_eq!(key_of(&after, "a1"), "5:0000000001000:0");
        assert_eq!(key_of(&after, "a2"), "5:0000000001000:1");
    }

    #[test]
    fn a_file_that_is_not_a_conversation_is_an_error() {
        let at = ThreadFile {
            folder: "inbox".into(),
            dir: "q_9".into(),
        };
        for bad in [
            json!([]),
            json!({"participants": []}),
            json!({"messages": {}}),
        ] {
            assert!(at.rows(bad.clone()).is_err(), "{bad}");
        }
    }

    #[test]
    fn the_thread_row_keeps_everything_but_the_messages() {
        let at = ThreadFile {
            folder: "message_requests".into(),
            dir: "q_9".into(),
        };
        let rows = at
            .rows(json!({
                "participants": [{"name": "Q"}],
                "title": "Q",
                "is_pending": true,
                "messages": [{"timestamp_ms": 1, "sender_name": "Q"}],
            }))
            .unwrap();
        let thread = &rows[0].payload;
        assert_eq!(rows[0].table, MESSENGER_THREADS_TABLE);
        assert_eq!(rows[0].id, "9");
        assert_eq!(thread["folder"], "message_requests");
        assert_eq!(thread["thread"]["is_pending"], true);
        assert!(thread["thread"].get("messages").is_none());
        assert_eq!(rows[1].payload["thread_id"], "9");
        assert_eq!(rows[1].payload["message"]["sender_name"], "Q");
    }
}
