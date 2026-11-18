//! Turning one RFC 5322 message into a JMAP-shaped `Email/get` envelope.

use anyhow::{anyhow, Result};
use mail_parser::{AddressList, HeaderValue, Message, MessageParser, PartKind};
use serde_json::{json, Value};

/// The per-message facts a transport knows and the bytes do not.
#[derive(Debug, Clone)]
pub struct TransportFacts {
    /// Stable email id, from [`email_id`].
    pub email_id: String,
    /// CAS key for the `.eml` (its blake3).
    pub blob_id: String,
    pub thread_id: String,
    pub mailbox_ids: Vec<String>,
    pub keywords: Vec<String>,
    /// Upstream's line of the body, as plain text. `None` makes one from
    /// the bytes ([`preview`]).
    pub preview: Option<String>,
}

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
        "hasAttachment": has_attachment(msg),
        "preview": facts.preview.clone().unwrap_or_else(|| preview(msg)),
    });
    let obj = envelope.as_object_mut().expect("envelope is an object");
    if let Some(r) = received_at(msg) {
        obj.insert("receivedAt".into(), Value::String(r.clone()));
        obj.insert("sentAt".into(), Value::String(r));
    }
    // An empty Subject stores none, as every row already stored has it.
    if let Some(s) = msg.subject().filter(|s| !s.is_empty()) {
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
    if let Some(irt) = msg.headers().value("In-Reply-To").and_then(header_text) {
        obj.insert(
            "inReplyTo".into(),
            Value::Array(vec![Value::String(strip_angle(&irt).to_string())]),
        );
    }
    let refs: Vec<Value> = msg
        .headers()
        .value("References")
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

/// The most a preview holds: what RFC 8621 allows a JMAP server's.
const PREVIEW_CHARS: usize = 256;
/// Asked of the parser in bytes, and before white space collapses, so
/// enough to fill [`PREVIEW_CHARS`] of text in any script.
const PREVIEW_BYTES: usize = 4 * 1024;

/// A line of the message's body as plain text: its first text part, else
/// its first HTML part with the markup taken out. Empty when it has no text.
pub fn preview(msg: &Message<'_>) -> String {
    let body = msg.body_preview(PREVIEW_BYTES).unwrap_or_default();
    let words: Vec<&str> = body.split_whitespace().collect();
    words.join(" ").chars().take(PREVIEW_CHARS).collect()
}

/// Gmail's own id for a message or a thread. Its top bits are the time
/// Gmail received the message (a thread's, its first message), so ids
/// sort in arrival order. The API spells it in hex; a Takeout mbox spells
/// it in decimal, on the message's `From ` line and in `X-GM-THRID`.
/// A store spells both [`GmailId::key`]'s way.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GmailId(u64);

impl GmailId {
    pub fn from_api(hex: &str) -> Option<Self> {
        u64::from_str_radix(hex, 16).ok().map(Self)
    }

    pub fn from_takeout(decimal: &str) -> Option<Self> {
        decimal.parse().ok().map(Self)
    }

    /// Lowercase hex zero-padded to sixteen, so the text sorts as the
    /// number does. The API drops the leading zero, which only an id from
    /// Gmail's first months has.
    pub(crate) fn key(self) -> String {
        format!("{:016x}", self.0)
    }

    /// The key of an id as the API spells it; one that is not hex stays
    /// as it is.
    pub(crate) fn key_of_api(api: &str) -> String {
        Self::from_api(api).map_or_else(|| api.to_string(), Self::key)
    }
}

/// The stable email id for a message: Gmail's own id when the transport
/// has one, else the `Message-ID` header, else the content hash.
///
/// Gmail's id comes first because the rows a sync writes should sit
/// together in the store (etl/README.md § "What a write costs"), and
/// because a `Message-ID` is not unique: two Gmail messages can share one,
/// and keyed by it they would collapse into a single row.
pub fn email_id(gmail_id: Option<GmailId>, msg: &Message<'_>, content_hash: &str) -> String {
    if let Some(g) = gmail_id {
        return g.key();
    }
    match msg.message_id() {
        Some(mid) => strip_angle(mid).to_string(),
        None => content_hash.to_string(),
    }
}

pub fn received_at(msg: &Message<'_>) -> Option<String> {
    msg.date()
        .and_then(|d| datalib_time::parse_strict(&d.to_rfc3339()).ok())
        .map(|t| t.to_rfc3339())
        .or_else(|| header_text(msg.headers().value("Date")?))
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

pub fn header_text(hv: HeaderValue<'_>) -> Option<String> {
    match hv {
        HeaderValue::Text(t) => Some(t.to_string()),
        HeaderValue::TextList(v) => Some(v.iter().collect::<Vec<_>>().join(" ")),
        _ => None,
    }
}

pub fn addresses_to_jmap(addr: Option<AddressList<'_>>) -> Option<Vec<Value>> {
    let list = addr?;
    let mut out = Vec::new();
    for a in list.mailboxes() {
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

/// Any part of the message itself that is named or not text: the rule
/// every stored `hasAttachment` was made with. RFC 8621's is narrower; it
/// leaves out an inline image.
pub fn has_attachment(msg: &Message<'_>) -> bool {
    msg.root().parts().any(|part| {
        part.attachment_name().is_some()
            || matches!(part.kind(), PartKind::Binary | PartKind::InlineBinary)
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
            preview: None,
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

    /// A Takeout import followed by a live sync must land on the same
    /// rows: the two transports spell one Gmail id differently.
    #[test]
    fn the_api_and_takeout_spellings_of_one_gmail_id_agree() {
        let msg = parse(EML).unwrap();
        let api = GmailId::from_api("19b8d627a801a2b0");
        let takeout = GmailId::from_takeout("1853466712473707184");
        assert_eq!(api, takeout);
        assert_eq!(email_id(api, &msg, "hash"), "19b8d627a801a2b0");
    }

    /// A thread's id is keyed as a message's is, so a Takeout's
    /// `X-GM-THRID` and the API's `threadId` land on one `threads` row.
    #[test]
    fn a_thread_id_is_keyed_like_a_message_id() {
        let takeout = GmailId::from_takeout("1853466712473707184").map(GmailId::key);
        assert_eq!(takeout.as_deref(), Some("19b8d627a801a2b0"));
        assert_eq!(GmailId::key_of_api("19b8d627a801a2b0"), "19b8d627a801a2b0");
        assert_eq!(GmailId::key_of_api("fffffffffffffff"), "0fffffffffffffff");
        assert_eq!(GmailId::key_of_api("thread-xyz"), "thread-xyz");
    }

    /// Keyed by `Message-ID`, two Gmail messages that share one were
    /// merged into a single row.
    #[test]
    fn two_gmail_messages_sharing_a_message_id_stay_two_rows() {
        let msg = parse(EML).unwrap();
        assert_ne!(
            email_id(GmailId::from_api("19b8d627a801a2b0"), &msg, "hash"),
            email_id(GmailId::from_api("19b8d627a801a2b1"), &msg, "hash"),
        );
    }

    /// The text has to sort as the number does, or the store's order is
    /// not arrival order.
    #[test]
    fn a_gmail_id_sorts_as_its_number() {
        let msg = parse(EML).unwrap();
        let short = email_id(GmailId::from_api("fffffffffffffff"), &msg, "h");
        let long = email_id(GmailId::from_api("1000000000000000"), &msg, "h");
        assert_eq!(short, "0fffffffffffffff");
        assert!(short < long);
    }

    #[test]
    fn uses_the_message_id_without_a_gmail_id() {
        let msg = parse(EML).unwrap();
        assert_eq!(email_id(None, &msg, "fallback"), "abc.123@enterprise.ufp");
    }

    /// A message with no `Message-ID` still needs a stable id, and the
    /// content hash is the only thing available that two transports
    /// looking at the same bytes will agree on.
    #[test]
    fn falls_back_to_the_content_hash_without_a_message_id() {
        let raw = b"From: a@b\r\nSubject: no id\r\n\r\nbody\r\n";
        let msg = parse(raw).unwrap();
        assert_eq!(email_id(None, &msg, "blake3-of-bytes"), "blake3-of-bytes");
    }

    /// Angle brackets are part of the header syntax, not the identifier.
    /// A mode that kept them would not dedupe against one that didn't.
    #[test]
    fn strips_angle_brackets_from_identifiers() {
        assert_eq!(strip_angle("<abc@d>"), "abc@d");
        assert_eq!(strip_angle("  <abc@d>  "), "abc@d");
        assert_eq!(strip_angle("abc@d"), "abc@d");
    }

    #[test]
    fn a_preview_is_the_text_body_on_one_line() {
        let msg = parse(EML).unwrap();
        assert_eq!(preview(&msg), "Make it so.");
        let raw = b"From: a@b\r\nSubject: s\r\n\r\nTea,\r\n\r\n   Earl Grey,\thot.\r\n";
        assert_eq!(preview(&parse(raw).unwrap()), "Tea, Earl Grey, hot.");
    }

    /// Most mail with no text part is marketing HTML. Its markup, styles and
    /// entities are not what a person reads.
    #[test]
    fn a_preview_of_html_mail_is_its_text() {
        let raw = b"From: a@b\r\nSubject: s\r\nContent-Type: text/html\r\n\r\n\
<html><head><style>p { color: red }</style></head>\
<body><p>Tea &amp; biscuits</p><p>in Ten Forward</p></body></html>\r\n";
        assert_eq!(
            preview(&parse(raw).unwrap()),
            "Tea & biscuits in Ten Forward"
        );
    }

    /// No more than a JMAP server may send, cut on a character.
    #[test]
    fn a_long_preview_stops_at_the_limit() {
        let raw = format!("From: a@b\r\nSubject: s\r\n\r\n{}\r\n", "é".repeat(9000));
        let got = preview(&parse(raw.as_bytes()).unwrap());
        assert_eq!(got.chars().count(), PREVIEW_CHARS);
        assert!(got.chars().all(|c| c == 'é'), "{got}");
    }

    /// Empty, not absent: "the bytes were read and hold no text" is what
    /// stops a store asking again.
    #[test]
    fn a_message_with_no_body_has_an_empty_preview() {
        let raw = b"From: a@b\r\nSubject: nothing\r\n\r\n";
        let msg = parse(raw).unwrap();
        assert_eq!(preview(&msg), "");
        assert_eq!(synthesize(raw, &msg, &facts())["preview"], "");
    }

    #[test]
    fn upstreams_preview_is_kept_over_one_made_from_the_bytes() {
        let msg = parse(EML).unwrap();
        let facts = TransportFacts {
            preview: Some("As Gmail put it".into()),
            ..facts()
        };
        assert_eq!(synthesize(EML, &msg, &facts)["preview"], "As Gmail put it");
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

/// What a download keeps from the parse, pinned on made-up messages
/// whose handling moved between mail-parser releases. A parser change
/// that moves one of these changes rows already stored: it needs a
/// migration rung, or a reason here why the stored row can move.
#[cfg(test)]
mod stored_facts {
    use super::*;

    /// The fields of a stored envelope that come from the bytes, and the
    /// Message-ID-keyed `email_id` an mbox without Gmail ids stores.
    fn stored(raw: &[u8]) -> Value {
        let Ok(msg) = parse(raw) else {
            return json!("unparsed");
        };
        let facts = TransportFacts {
            email_id: String::new(),
            blob_id: String::new(),
            thread_id: String::new(),
            mailbox_ids: vec![],
            keywords: vec![],
            preview: None,
        };
        let mut env = synthesize(raw, &msg, &facts);
        let obj = env.as_object_mut().unwrap();
        // The row's preview column, not its payload; pinned by the tests
        // of `preview` above.
        obj.remove("preview");
        for transport in ["id", "blobId", "threadId", "mailboxIds", "keywords", "size"] {
            obj.remove(transport);
        }
        obj.insert(
            "email_id".into(),
            email_id(None, &msg, "content-hash").into(),
        );
        env
    }

    #[test]
    fn an_empty_subject_stores_no_subject() {
        let raw = b"From: Data <data@enterprise.ufp>\r\nSubject: \r\n\
Message-ID: <blank.subject@enterprise.ufp>\r\n\r\nNo subject.\r\n";
        assert_eq!(
            stored(raw),
            json!({
                "email_id": "blank.subject@enterprise.ufp",
                "messageId": ["blank.subject@enterprise.ufp"],
                "from": [{"email": "data@enterprise.ufp", "name": "Data"}],
                "hasAttachment": false,
            })
        );
    }

    /// mail-parser 0.11 and 1.0 alike drop a mailbox that follows a
    /// group in the same field.
    #[test]
    fn a_group_stores_its_members_in_order() {
        let raw = b"From: William Riker <riker@enterprise.ufp>\r\n\
To: Away Team: Worf <worf@enterprise.ufp>, Deanna Troi <troi@enterprise.ufp>;,\r\n\
 Beverly Crusher <crusher@enterprise.ufp>\r\n\
Cc: undisclosed-recipients:;\r\n\
Subject: Away team roster\r\n\
Message-ID: <roster@enterprise.ufp>\r\n\r\nBeam down at 0800.\r\n";
        assert_eq!(
            stored(raw),
            json!({
                "email_id": "roster@enterprise.ufp",
                "messageId": ["roster@enterprise.ufp"],
                "subject": "Away team roster",
                "from": [{"email": "riker@enterprise.ufp", "name": "William Riker"}],
                "to": [
                    {"email": "worf@enterprise.ufp", "name": "Worf"},
                    {"email": "troi@enterprise.ufp", "name": "Deanna Troi"},
                ],
                "hasAttachment": false,
            })
        );
    }

    /// One name with no address drops the whole field, not just that
    /// name.
    #[test]
    fn a_name_without_an_address_drops_the_field() {
        let raw = b"From: Guinan\r\nTo: Guinan, Geordi La Forge <laforge@enterprise.ufp>\r\n\
Subject: Ten Forward\r\nMessage-ID: <ten.forward@enterprise.ufp>\r\n\r\nA drink?\r\n";
        assert_eq!(
            stored(raw),
            json!({
                "email_id": "ten.forward@enterprise.ufp",
                "messageId": ["ten.forward@enterprise.ufp"],
                "subject": "Ten Forward",
                "hasAttachment": false,
            })
        );
    }

    #[test]
    fn encoded_words_in_a_subject_and_a_name() {
        let raw = b"From: =?utf-8?q?Jean-Luc?= Picard <picard@enterprise.ufp>\r\n\
To: =?utf-8?b?QmV2ZXJseQ==?= =?utf-8?b?IENydXNoZXI=?= <crusher@enterprise.ufp>\r\n\
Subject: Re: =?utf-8?q?Tea,?= =?utf-8?q?_Earl_Grey?= hot\r\n\
Message-ID: <encoded@enterprise.ufp>\r\n\r\nHot.\r\n";
        assert_eq!(
            stored(raw),
            json!({
                "email_id": "encoded@enterprise.ufp",
                "messageId": ["encoded@enterprise.ufp"],
                "subject": "Re: Tea, Earl Grey hot",
                "from": [{"email": "picard@enterprise.ufp", "name": "Jean-Luc Picard"}],
                "to": [{"email": "crusher@enterprise.ufp", "name": "Beverly Crusher"}],
                "hasAttachment": false,
            })
        );
    }

    /// The `email_id` of every Message-ID spelling below is a row key in
    /// a non-Gmail mbox's store; a change re-keys the row.
    #[test]
    fn odd_message_id_spellings_keep_their_email_id() {
        let cases: &[(&str, &str)] = &[
            (
                "Message-ID: tea.earl.grey@enterprise.ufp",
                "tea.earl.grey@enterprise.ufp",
            ),
            (
                "Message-ID: <first@enterprise.ufp> <second@enterprise.ufp>",
                "second@enterprise.ufp",
            ),
            (
                "Message-ID: <relayed@enterprise.ufp> (Starfleet relay)",
                "relayed@enterprise.ufp",
            ),
            (
                "Message-ID:\r\n <folded@enterprise.ufp>",
                "folded@enterprise.ufp",
            ),
            (
                "Message-Id: <lower.case@enterprise.ufp>",
                "lower.case@enterprise.ufp",
            ),
            (
                "Message-ID: <with space@enterprise.ufp>",
                "with space@enterprise.ufp",
            ),
            ("Message-ID: <>", "content-hash"),
            ("Message-ID: ", "content-hash"),
            (
                "Message-ID: <one@enterprise.ufp>\r\nMessage-ID: <two@enterprise.ufp>",
                "two@enterprise.ufp",
            ),
        ];
        for (header, want) in cases {
            let raw = format!("From: Data <data@enterprise.ufp>\r\n{header}\r\n\r\nbody\r\n");
            assert_eq!(stored(raw.as_bytes())["email_id"], *want, "{header:?}");
        }
    }

    /// Which inputs parse at all: one that does not is counted as a
    /// parse error and stores no row. mail-parser 0.11 also took one
    /// line with no line break, as a message with no header and an empty
    /// body; 1.0 refuses it, as both refuse that line with a line break.
    #[test]
    fn which_headerless_inputs_parse() {
        let cases: &[(&[u8], bool)] = &[
            (b"", false),
            (b"Captain's log, stardate 41153.7.\r\n", false),
            (b"Captain's log", false),
            (b"Captain's log\r\nSecond entry\r\n", false),
            (b"\r\nA body with no header.\r\n", true),
            (
                b"Captain's log\r\n\r\nA body after a line that is no header.\r\n",
                true,
            ),
            (b"Note: a header and nothing else", true),
            (b"Note: a header\r\n\r\n", true),
        ];
        let got: Vec<_> = cases
            .iter()
            .map(|(raw, _)| {
                (
                    String::from_utf8_lossy(raw),
                    stored(raw) != json!("unparsed"),
                )
            })
            .collect();
        let want: Vec<_> = cases
            .iter()
            .map(|(raw, parses)| (String::from_utf8_lossy(raw), *parses))
            .collect();
        assert_eq!(got, want);
    }

    #[test]
    fn a_date_that_does_not_parse_stores_no_received_at() {
        let raw = b"From: Data <data@enterprise.ufp>\r\nDate: stardate 41153.7\r\n\
Message-ID: <stardate@enterprise.ufp>\r\n\r\nbody\r\n";
        assert_eq!(stored(raw).get("receivedAt"), None);
    }

    /// mail-parser 0.11 read `Fr om:` as `From:`, so a sender could put
    /// a second From where a DKIM check would not see it. The rows of an
    /// mbox take the real sender on its next read; a Gmail API row keeps
    /// the one it stored, in `from_json`, which nothing shows.
    #[test]
    fn a_ut_zone_and_a_spaced_header_name() {
        let raw = b"From: Data <data@enterprise.ufp>\r\n\
Date: Tue, 4 May 2027 03:42:05 UT\r\n\
Subject: After the UT zone\r\n\
Fr om: Q <q@continuum.example>\r\n\
Message-ID: <ut.zone@enterprise.ufp>\r\n\r\nbody\r\n";
        assert_eq!(
            stored(raw),
            json!({
                "email_id": "ut.zone@enterprise.ufp",
                "messageId": ["ut.zone@enterprise.ufp"],
                "subject": "After the UT zone",
                "from": [{"email": "data@enterprise.ufp", "name": "Data"}],
                "receivedAt": "2027-05-04T03:42:05+00:00",
                "sentAt": "2027-05-04T03:42:05+00:00",
                "hasAttachment": false,
            })
        );
    }

    /// A Takeout's labels name the mailboxes a message is stored in, read
    /// as written: an encoded word stays encoded, a fold stays folded.
    #[test]
    fn takeout_label_and_thread_headers_are_read_raw() {
        let raw = b"From: Data <data@enterprise.ufp>\r\n\
X-GM-THRID: 1853466712473707184\r\n\
X-Gmail-Labels: Archived,Category Updates,\r\n =?UTF-8?Q?Caf=C3=A9?=,Opened\r\n\
Message-ID: <labels@enterprise.ufp>\r\n\r\nbody\r\n";
        let msg = parse(raw).unwrap();
        let header = |name| msg.headers().value(name).and_then(header_text);
        assert_eq!(header("X-GM-THRID").as_deref(), Some("1853466712473707184"));
        assert_eq!(
            header("X-Gmail-Labels").as_deref(),
            Some("Archived,Category Updates,\r\n =?UTF-8?Q?Caf=C3=A9?=,Opened")
        );
    }

    /// mail-parser 0.11 dropped the line after a one-letter zone, so a
    /// non-Gmail mbox stored this message under its content hash. It
    /// moves to its Message-ID on the mbox's next read, which drops the
    /// old row.
    #[test]
    fn a_military_zone_no_longer_hides_the_message_id() {
        let raw = b"From: Data <data@enterprise.ufp>\r\n\
Date: Tue, 4 May 2027 03:42:05 A\r\n\
Message-ID: <military.zone@enterprise.ufp>\r\n\r\nbody\r\n";
        assert_eq!(stored(raw)["email_id"], "military.zone@enterprise.ufp");
    }

    #[test]
    fn an_unpadded_base64_attachment_is_an_attachment() {
        assert_eq!(stored(UNPADDED_ATTACHMENT)["hasAttachment"], true);
    }

    const UNPADDED_ATTACHMENT: &[u8] = b"From: Data <data@enterprise.ufp>\r\n\
Subject: Log\r\nMessage-ID: <unpadded@enterprise.ufp>\r\nMIME-Version: 1.0\r\n\
Content-Type: multipart/mixed; boundary=\"b1\"\r\n\r\n\
--b1\r\nContent-Type: text/plain\r\n\r\nAttached.\r\n\
--b1\r\nContent-Type: text/plain; name=\"order.txt\"\r\n\
Content-Disposition: attachment; filename=\"order.txt\"\r\n\
Content-Transfer-Encoding: base64\r\n\r\n\
VGVhLCBFYXJsIEdyZXksIGhvdA\r\n\
--b1--\r\n";
}
