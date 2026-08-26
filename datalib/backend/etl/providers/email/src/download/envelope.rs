//! Turning one RFC 5322 message into a JMAP-shaped `Email/get` envelope.
//!
//! The raw schema stores every email's envelope in JMAP's shape
//! regardless of where it came from (see [`super::schema_raw`]), so that
//! `EmailRow::from_jmap_envelope` — and therefore every promoted column,
//! the stored payload, and the `mailboxIds` / `keywords` join inputs — is
//! written by exactly one code path.
//!
//! The JMAP downloader gets that shape from the server. Every other mode
//! has to build it: mbox parses a Google Takeout export, and the Gmail
//! API mode parses `messages.get?format=RAW`. Both end up holding the
//! same two things — the message bytes, and some per-message facts the
//! *transport* supplied (which labels, which thread, which keywords) that
//! are not derivable from the bytes. This module is where those meet.
//!
//! Extracting it is what makes "the same mailbox ingested two ways
//! dedupes rather than doubles" a property of the code rather than of
//! two implementations happening to agree.

use anyhow::{anyhow, Result};
use mail_parser::{Address, HeaderValue, Message, MessageParser, MimeHeaders, PartType};
use serde_json::{json, Value};

/// The per-message facts a transport knows and the bytes do not.
///
/// A Takeout mbox carries labels in an `X-Gmail-Labels` header; the Gmail
/// API returns them as `labelIds`. By the time we get here they have both
/// been resolved to the same mailbox ids and JMAP keywords.
#[derive(Debug, Clone)]
pub struct TransportFacts {
    /// Stable email id — the `Message-ID` header, or the content hash
    /// when the message has none. Identical across modes, which is what
    /// makes a Takeout-then-live-sync migration dedupe.
    pub email_id: String,
    /// CAS key for the `.eml` (its blake3).
    pub blob_id: String,
    pub thread_id: String,
    pub mailbox_ids: Vec<String>,
    pub keywords: Vec<String>,
}

/// Build the JMAP-shaped envelope for `raw`.
///
/// `msg` is the already-parsed form of `raw`; callers generally need the
/// parse for other reasons (attachment detection, date extraction) and
/// parsing a large message twice is not free.
pub fn synthesize(raw: &[u8], msg: &Message<'_>, facts: &TransportFacts) -> Value {
    let mailbox_ids_obj: serde_json::Map<String, Value> = facts
        .mailbox_ids
        .iter()
        .map(|m| (m.clone(), Value::Bool(true)))
        .collect();
    let keywords_obj: serde_json::Map<String, Value> = facts
        .keywords
        .iter()
        .map(|k| (k.clone(), Value::Bool(true)))
        .collect();
    let mut envelope = json!({
        "id": facts.email_id.clone(),
        "blobId": facts.blob_id.clone(),
        "threadId": facts.thread_id.clone(),
        "mailboxIds": Value::Object(mailbox_ids_obj),
        "keywords": Value::Object(keywords_obj),
        "size": raw.len(),
        "hasAttachment": iter_attachments(msg).next().is_some(),
    });
    let obj = envelope.as_object_mut().expect("envelope is an object");
    if let Some(r) = received_at(msg) {
        obj.insert("receivedAt".into(), Value::String(r.clone()));
        obj.insert("sentAt".into(), Value::String(r));
    }
    if let Some(s) = msg.subject() {
        obj.insert("subject".into(), Value::String(s.to_string()));
    }
    if let Some(from) = addresses_to_jmap(msg.from()) {
        obj.insert("from".into(), Value::Array(from));
    }
    if let Some(to) = addresses_to_jmap(msg.to()) {
        obj.insert("to".into(), Value::Array(to));
    }
    if let Some(cc) = addresses_to_jmap(msg.cc()) {
        obj.insert("cc".into(), Value::Array(cc));
    }
    if let Some(mid) = msg.message_id() {
        obj.insert(
            "messageId".into(),
            Value::Array(vec![Value::String(strip_angle(mid).to_string())]),
        );
    }
    if let Some(irt) = msg.header("In-Reply-To").and_then(header_text) {
        obj.insert(
            "inReplyTo".into(),
            Value::Array(vec![Value::String(strip_angle(&irt).to_string())]),
        );
    }
    let refs: Vec<Value> = msg
        .header("References")
        .and_then(header_text)
        .map(|s| {
            s.split_whitespace()
                .map(|tok| Value::String(strip_angle(tok).to_string()))
                .collect()
        })
        .unwrap_or_default();
    if !refs.is_empty() {
        obj.insert("references".into(), Value::Array(refs));
    }
    envelope
}

/// The stable email id for a message: its `Message-ID` header, falling
/// back to the content hash when it has none.
///
/// Every mode must derive this the same way. Using a transport-native id
/// instead (Gmail's hex `id`, JMAP's `Email.id`) would fork the id space
/// per transport, so the same mailbox ingested from a Takeout export and
/// then from a live sync would double rather than dedupe.
pub fn email_id(msg: &Message<'_>, content_hash: &str) -> String {
    match msg.message_id() {
        Some(mid) => strip_angle(mid).to_string(),
        None => content_hash.to_string(),
    }
}

/// `Date:` as an offset-preserving ISO-8601 string, per the repo-wide
/// timestamp convention.
pub fn received_at(msg: &Message<'_>) -> Option<String> {
    msg.date()
        .and_then(|d| datalib_time::parse_strict(&d.to_rfc3339()).ok())
        .map(|t| t.to_rfc3339())
        .or_else(|| header_text(msg.header("Date")?))
}

/// Parse `raw`, or fail with a message naming what could not be parsed.
pub fn parse(raw: &[u8]) -> Result<Message<'_>> {
    MessageParser::default()
        .parse(raw)
        .ok_or_else(|| anyhow!("mail-parser could not parse a {}-byte message", raw.len()))
}

pub fn strip_angle(s: &str) -> &str {
    s.trim().trim_start_matches('<').trim_end_matches('>')
}

pub fn header_text(hv: &HeaderValue) -> Option<String> {
    match hv {
        HeaderValue::Text(t) => Some(t.to_string()),
        HeaderValue::TextList(v) => Some(v.join(" ")),
        _ => None,
    }
}

/// JMAP `EmailAddress[]` from a parsed address header.
pub fn addresses_to_jmap(addr: Option<&Address>) -> Option<Vec<Value>> {
    let list = addr?;
    let mut out = Vec::new();
    for a in list.iter() {
        let email = a.address()?.to_string();
        let mut o = serde_json::Map::new();
        o.insert("email".into(), Value::String(email));
        if let Some(name) = a.name() {
            o.insert("name".into(), Value::String(name.to_string()));
        }
        out.push(Value::Object(o));
    }
    (!out.is_empty()).then_some(out)
}

/// Walk every MIME part the parser surfaces as an attachment or inline
/// non-body part, yielding `(dotted_part_id, &MessagePart)`. Mirrors the
/// JMAP server's `partId` convention (1-based dotted paths).
pub fn iter_attachments<'a>(
    msg: &'a Message<'a>,
) -> impl Iterator<Item = (String, &'a mail_parser::MessagePart<'a>)> {
    msg.parts.iter().enumerate().filter_map(move |(i, part)| {
        let is_attachment = part.attachment_name().is_some()
            || matches!(part.body, PartType::Binary(_) | PartType::InlineBinary(_));
        is_attachment.then(|| ((i + 1).to_string(), part))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const EML: &[u8] = b"From: Jean-Luc Picard <picard@enterprise.ufp>\r\n\
To: Beverly Crusher <crusher@enterprise.ufp>\r\n\
Cc: Data <data@enterprise.ufp>\r\n\
Subject: Tea, Earl Grey, hot\r\n\
Message-ID: <abc.123@enterprise.ufp>\r\n\
In-Reply-To: <prior.1@enterprise.ufp>\r\n\
References: <root.0@enterprise.ufp> <prior.1@enterprise.ufp>\r\n\
Date: Tue, 4 May 2027 03:42:05 -0700\r\n\
\r\n\
Make it so.\r\n";

    fn facts() -> TransportFacts {
        TransportFacts {
            email_id: "abc.123@enterprise.ufp".into(),
            blob_id: "b3hash".into(),
            thread_id: "thr-1".into(),
            mailbox_ids: vec!["mbox-inbox".into(), "mbox-work".into()],
            keywords: vec!["$seen".into()],
        }
    }

    #[test]
    fn builds_a_jmap_shaped_envelope() {
        let msg = parse(EML).unwrap();
        let env = synthesize(EML, &msg, &facts());
        assert_eq!(env["id"], "abc.123@enterprise.ufp");
        assert_eq!(env["blobId"], "b3hash");
        assert_eq!(env["threadId"], "thr-1");
        assert_eq!(env["subject"], "Tea, Earl Grey, hot");
        assert_eq!(env["size"], EML.len());
        assert_eq!(env["from"][0]["email"], "picard@enterprise.ufp");
        assert_eq!(env["from"][0]["name"], "Jean-Luc Picard");
        assert_eq!(env["to"][0]["email"], "crusher@enterprise.ufp");
        assert_eq!(env["cc"][0]["email"], "data@enterprise.ufp");
        assert_eq!(env["messageId"][0], "abc.123@enterprise.ufp");
        assert_eq!(env["inReplyTo"][0], "prior.1@enterprise.ufp");
        assert_eq!(env["references"][1], "prior.1@enterprise.ufp");
        assert_eq!(env["hasAttachment"], false);
    }

    /// The join tables are refreshed by reading these back out of the
    /// stored payload, so they have to be objects keyed by id, not arrays.
    #[test]
    fn writes_mailboxes_and_keywords_as_id_keyed_objects() {
        let msg = parse(EML).unwrap();
        let env = synthesize(EML, &msg, &facts());
        assert_eq!(env["mailboxIds"]["mbox-inbox"], true);
        assert_eq!(env["mailboxIds"]["mbox-work"], true);
        assert_eq!(env["keywords"]["$seen"], true);
        assert!(env["mailboxIds"].is_object());
    }

    /// Repo-wide convention: preserve the source offset, never normalize
    /// to UTC. `-0700` has to survive.
    #[test]
    fn preserves_the_source_timezone_offset() {
        let msg = parse(EML).unwrap();
        let env = synthesize(EML, &msg, &facts());
        let received = env["receivedAt"].as_str().unwrap();
        assert!(received.contains("-07:00"), "normalized away: {received}");
        assert_eq!(env["sentAt"], env["receivedAt"]);
    }

    #[test]
    fn uses_the_message_id_as_the_stable_id() {
        let msg = parse(EML).unwrap();
        assert_eq!(email_id(&msg, "fallback"), "abc.123@enterprise.ufp");
    }

    /// A message with no `Message-ID` still needs a stable id, and the
    /// content hash is the only thing available that two transports
    /// looking at the same bytes will agree on.
    #[test]
    fn falls_back_to_the_content_hash_without_a_message_id() {
        let raw = b"From: a@b\r\nSubject: no id\r\n\r\nbody\r\n";
        let msg = parse(raw).unwrap();
        assert_eq!(email_id(&msg, "blake3-of-bytes"), "blake3-of-bytes");
    }

    /// Angle brackets are part of the header syntax, not the identifier.
    /// A mode that kept them would not dedupe against one that didn't.
    #[test]
    fn strips_angle_brackets_from_identifiers() {
        assert_eq!(strip_angle("<abc@d>"), "abc@d");
        assert_eq!(strip_angle("  <abc@d>  "), "abc@d");
        assert_eq!(strip_angle("abc@d"), "abc@d");
    }

    /// Two transports handing the same bytes and the same facts to this
    /// function must produce byte-identical envelopes — that is the whole
    /// reason it exists.
    #[test]
    fn is_deterministic_across_callers() {
        let msg = parse(EML).unwrap();
        let a = synthesize(EML, &msg, &facts());
        let b = synthesize(EML, &parse(EML).unwrap(), &facts());
        assert_eq!(a, b);
    }
}
