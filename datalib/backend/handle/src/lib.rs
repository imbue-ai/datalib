//! A handle: one identifier for a person in one namespace, normalized so
//! that the same person reached two ways reads the same.
//!
//! A handle is `<kind>:<value>` — `email:riker@enterprise.org`,
//! `tel:+12025550123`, `slack:T01/U02`. Where a native id *is* an email
//! address or a phone number it becomes one of those, not a per-app kind,
//! so one link covers every app that reaches a person by that number.
//! Renders write handles into the markdown as chip links, `[Name](uri)`
//! with the handle as a URI ([`Handle::to_uri`]), and the contacts app
//! links them to contacts; nothing here knows what a contact is.
//! `docs/dev/contacts.md` says how it all fits, and what a new kind
//! touches.
//!
//! A handle is stored: in render stores, in the index, and in the links a
//! person made in the contacts app. So a change to what a constructor
//! returns for some input bumps [`RULES_VERSION`]; the render step
//! re-renders every source when it moves, and the contacts store's test
//! names the rung that brings its links along.

use std::fmt;

use serde::{Deserialize, Serialize};
use strum::{EnumString, IntoStaticStr, VariantArray};

/// Bump whenever any constructor below returns something different for
/// some input — a spelling newly accepted or refused, a value normalized
/// another way. A new kind is not that: no handle already stored reads
/// differently, so it moves nothing.
pub const RULES_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, EnumString, IntoStaticStr, VariantArray)]
#[strum(serialize_all = "snake_case")]
pub enum HandleKind {
    Email,
    Tel,
    Slack,
    /// A Signal account's id (ACI): the one identifier a Signal backup
    /// has for a person whose number it does not know.
    SignalAci,
    /// A person on Facebook, whose export names people and never numbers
    /// them: `name/<the name shown>`, or `deleted/<conversation id>` for
    /// an account deleted since, which only its one-to-one conversation
    /// tells apart from the others.
    Facebook,
}

impl HandleKind {
    pub fn as_str(self) -> &'static str {
        self.into()
    }

    /// `None` for a spelling this build does not know.
    pub fn parse(s: &str) -> Option<Self> {
        s.parse().ok()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(into = "String", try_from = "String")]
pub struct Handle(String);

impl From<Handle> for String {
    fn from(h: Handle) -> String {
        h.0
    }
}

impl TryFrom<String> for Handle {
    type Error = String;

    fn try_from(s: String) -> Result<Self, String> {
        Handle::parse(&s).ok_or_else(|| format!("{s:?} is not a handle"))
    }
}

impl Handle {
    /// An email address, lowercased, or a `mailto:` URI naming one.
    /// `Name <addr>` is not accepted: separating the name is the
    /// caller's parse, not a guess made here.
    pub fn email(addr: &str) -> Option<Self> {
        let addr = addr.trim();
        let addr = match strip_prefix_ignore_case(addr, "mailto:") {
            Some(uri) => uri.split_once('?').map_or(uri, |(a, _query)| a),
            None => addr,
        };
        let (local, domain) = addr.split_once('@')?;
        let well_formed = !local.is_empty()
            && !domain.is_empty()
            && !domain.contains('@')
            && domain.contains('.')
            && !addr
                .chars()
                .any(|c| c.is_whitespace() || matches!(c, '<' | '>' | ','));
        well_formed.then(|| Self::of(HandleKind::Email, &addr.to_lowercase()))
    }

    /// A phone number already in international form: a leading `+`, then
    /// the country code and number, bare or as a `tel:` URI. Separators —
    /// spaces of any width, dashes of any length, dots, parentheses — are
    /// dropped, as is a trunk `(0)` written after the country code
    /// (`+44 (0)20 …`) and an extension or other `;` parameter. A number
    /// without its country code is refused rather than given one by guess.
    pub fn tel(number: &str) -> Option<Self> {
        let number = number.trim();
        let number = strip_prefix_ignore_case(number, "tel:").unwrap_or(number);
        let number = number.split_once(';').map_or(number, |(n, _params)| n);
        let folded: String = number
            .chars()
            .map(|c| if is_separator(c) { ' ' } else { c })
            .collect();
        let rest = folded.trim_start().strip_prefix('+')?;
        let rest = drop_trunk_zero(rest);
        let mut digits = String::with_capacity(rest.len());
        for c in rest.chars() {
            match c {
                '0'..='9' => digits.push(c),
                ' ' | '(' | ')' => {}
                _ => return None,
            }
        }
        // E.164 allows at most 15 digits; fewer than 7 is a short code.
        // Under country code 1 (NANP) every number has ten digits after
        // the 1, so anything shorter is a short code someone put `+1` on.
        let nanp = digits.starts_with('1');
        let plausible = if nanp {
            digits.len() == 11
        } else {
            (7..=15).contains(&digits.len()) && !digits.starts_with('0')
        };
        plausible.then(|| Self::of(HandleKind::Tel, &format!("+{digits}")))
    }

    /// A WhatsApp JID. A person's JID is their phone number at
    /// `s.whatsapp.net`, so it becomes a `tel:` handle; a group
    /// (`@g.us`), a linked-device id (`@lid`) or anything else is not a
    /// phone number and has no handle yet.
    pub fn whatsapp_jid(jid: &str) -> Option<Self> {
        let number = jid.strip_suffix("@s.whatsapp.net")?;
        let number = number.split_once(':').map_or(number, |(n, _device)| n);
        Self::tel(&format!("+{number}"))
    }

    /// A Slack user, which is only unique within its workspace.
    pub fn slack(team_id: &str, user_id: &str) -> Option<Self> {
        let ok =
            |s: &str| !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
        (ok(team_id) && ok(user_id))
            .then(|| Self::of(HandleKind::Slack, &format!("{team_id}/{user_id}")))
    }

    /// A Signal ACI, a UUID: 32 hex digits, with or without the dashes,
    /// in any case. Spelled the way Signal does, lowercase with dashes.
    /// A PNI is not accepted here: it names a number, not a person, and
    /// the number is the handle.
    pub fn signal_aci(aci: &str) -> Option<Self> {
        let hex: String = aci
            .trim()
            .chars()
            .filter(|c| *c != '-')
            .map(|c| c.to_ascii_lowercase())
            .collect();
        if hex.len() != 32 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
            return None;
        }
        let dashed = format!(
            "{}-{}-{}-{}-{}",
            &hex[..8],
            &hex[8..12],
            &hex[12..16],
            &hex[16..20],
            &hex[20..]
        );
        Some(Self::of(HandleKind::SignalAci, &dashed))
    }

    /// A person on Facebook by the name the export shows, its whitespace
    /// trimmed and collapsed. Two people of one name are one handle: the
    /// export has nothing else to tell them apart by. Whether a name is a
    /// person at all — not Facebook's `Facebook user` for an account
    /// deleted since — is the caller's to decide.
    pub fn facebook_name(name: &str) -> Option<Self> {
        let name = name.split_whitespace().collect::<Vec<_>>().join(" ");
        (!name.is_empty() && !name.chars().any(char::is_control))
            .then(|| Self::of(HandleKind::Facebook, &format!("name/{name}")))
    }

    /// The deleted account on the other side of the one-to-one Messenger
    /// conversation `conversation_id` (the digits its directory ends in).
    pub fn facebook_deleted(conversation_id: &str) -> Option<Self> {
        let id = conversation_id.trim();
        (!id.is_empty() && id.bytes().all(|b| b.is_ascii_digit()))
            .then(|| Self::of(HandleKind::Facebook, &format!("deleted/{id}")))
    }

    /// A handle as written by [`Handle::as_str`]. `None` for an unknown
    /// kind or a value its kind would not have produced.
    pub fn parse(s: &str) -> Option<Self> {
        Self::rebuild(s).filter(|handle| handle.0 == s)
    }

    /// What a stored handle comes to under this build's rules: its value
    /// through its kind's constructor again. Unlike [`Handle::parse`] it
    /// takes a spelling an older build wrote, so a store can bring its
    /// handles along when [`RULES_VERSION`] moves.
    pub fn rebuild(stored: &str) -> Option<Self> {
        let (kind, value) = stored.split_once(':')?;
        match HandleKind::parse(kind)? {
            HandleKind::Email => Self::email(value),
            HandleKind::Tel => Self::tel(value),
            HandleKind::Slack => {
                let (team, user) = value.split_once('/')?;
                Self::slack(team, user)
            }
            HandleKind::SignalAci => Self::signal_aci(value),
            HandleKind::Facebook => match value.split_once('/')? {
                ("name", name) => Self::facebook_name(name),
                ("deleted", id) => Self::facebook_deleted(id),
                _ => None,
            },
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn kind(&self) -> HandleKind {
        let kind = self.0.split_once(':').map_or("", |(k, _)| k);
        HandleKind::parse(kind).expect("a Handle is only built with a known kind")
    }

    pub fn value(&self) -> &str {
        self.0.split_once(':').map_or("", |(_, v)| v)
    }

    /// The handle as a URI another app can follow: `mailto:` and `tel:`
    /// as the standards spell them, a Slack user as Slack's own deep
    /// link, and a kind with no scheme of its own (a Signal account id, a
    /// Facebook person) as `datalib:handle/<kind>/<value>`, the value
    /// percent-encoded but for its `/`s. This is the href of a chip
    /// link; [`Handle::from_uri`] reads it back, and
    /// `ui/src/cards/chipLinks.js` mirrors both.
    pub fn to_uri(&self) -> String {
        match self.kind() {
            HandleKind::Email => format!("mailto:{}", self.value()),
            HandleKind::Tel => format!("tel:{}", self.value()),
            HandleKind::Slack => {
                let (team, user) = self.value().split_once('/').unwrap_or((self.value(), ""));
                format!("slack://user?team={team}&id={user}")
            }
            // No app follows a link to a Signal account id, so it takes
            // the spelling for a kind with no scheme of its own.
            HandleKind::SignalAci | HandleKind::Facebook => {
                format!(
                    "datalib:handle/{}/{}",
                    self.kind().as_str(),
                    percent_encode(self.value())
                )
            }
        }
    }

    /// The handle a URI names, or `None` for one that names no handle
    /// this build knows: `mailto:` (its query dropped), `tel:`,
    /// `slack://user?team=…&id=…` with the parameters in either order,
    /// and `datalib:handle/<kind>/<value>`, the spelling for a kind that
    /// has no standard scheme of its own.
    pub fn from_uri(uri: &str) -> Option<Self> {
        let uri = uri.trim();
        if strip_prefix_ignore_case(uri, "mailto:").is_some() {
            return Self::email(uri);
        }
        if strip_prefix_ignore_case(uri, "tel:").is_some() {
            return Self::tel(uri);
        }
        if let Some(rest) = strip_prefix_ignore_case(uri, "datalib:handle/") {
            let (kind, value) = rest.split_once('/')?;
            return Self::rebuild(&format!("{kind}:{}", percent_decode(value)?));
        }
        let query = strip_prefix_ignore_case(uri, "slack://user?")?;
        let (mut team, mut user) = (None, None);
        for pair in query.split('&') {
            match pair.split_once('=') {
                Some(("team", t)) => team = Some(t),
                Some(("id", u)) => user = Some(u),
                _ => {}
            }
        }
        Self::slack(team?, user?)
    }

    /// The handle beside the name a source showed, as a person writes
    /// one: `Will Riker <riker@enterprise.org>`, `Will Riker
    /// (+15550123456)`, `Data (slack:T01/U02)`. The identifier alone when
    /// the name is empty or is the identifier. A chip link's title and
    /// its copied text; `copyText` in `ui/src/cards/contacts.ts` mirrors it.
    pub fn describe(&self, shown: &str) -> String {
        let shown = shown.trim();
        if shown.is_empty() || shown == self.value() || shown == self.as_str() {
            return self.value().to_string();
        }
        match self.kind() {
            HandleKind::Email => format!("{shown} <{}>", self.value()),
            HandleKind::Tel => format!("{shown} ({})", self.value()),
            HandleKind::Slack | HandleKind::SignalAci | HandleKind::Facebook => {
                format!("{shown} ({})", self.as_str())
            }
        }
    }

    fn of(kind: HandleKind, value: &str) -> Self {
        Self(format!("{}:{value}", kind.as_str()))
    }
}

/// Every byte but an unreserved one (RFC 3986) and `/` as `%XX`, so a
/// name's spaces and accents survive a markdown link's href.
fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~' | b'/') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// [`percent_encode`] undone; `None` for a malformed escape or bytes that
/// are not UTF-8.
fn percent_decode(s: &str) -> Option<String> {
    let mut bytes = Vec::with_capacity(s.len());
    let mut rest = s.as_bytes();
    while let Some((&b, tail)) = rest.split_first() {
        if b == b'%' {
            let hex = std::str::from_utf8(tail.get(..2)?).ok()?;
            bytes.push(u8::from_str_radix(hex, 16).ok()?);
            rest = &tail[2..];
        } else {
            bytes.push(b);
            rest = tail;
        }
    }
    String::from_utf8(bytes).ok()
}

fn strip_prefix_ignore_case<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    let head = s.get(..prefix.len())?;
    head.eq_ignore_ascii_case(prefix)
        .then(|| &s[prefix.len()..])
}

/// What people put between the digits of a phone number, from any
/// keyboard: every Unicode space, the dashes and the minus sign, and the
/// dot. Parentheses are kept so a trunk `(0)` can still be seen.
fn is_separator(c: char) -> bool {
    c.is_whitespace()
        || matches!(
            c,
            '-' | '.' | '\u{2010}'..='\u{2015}' | '\u{2212}' | '\u{FE58}' | '\u{FE63}' | '\u{FF0D}'
        )
}

/// `rest` (what follows the `+`) without a parenthesised `0` after the
/// country code. Dialled from abroad the trunk prefix is not dialled, so
/// `+44 (0)20 7946 0958` is `+442079460958`.
fn drop_trunk_zero(rest: &str) -> std::borrow::Cow<'_, str> {
    let squeezed: String = rest.chars().filter(|c| *c != ' ').collect();
    let country_code_len = squeezed
        .find("(0)")
        .filter(|&at| (1..=3).contains(&at) && squeezed[..at].bytes().all(|b| b.is_ascii_digit()));
    match country_code_len {
        Some(at) => format!("{}{}", &squeezed[..at], &squeezed[at + 3..]).into(),
        None => rest.into(),
    }
}

impl fmt::Display for Handle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn email_is_lowercased_and_trimmed() {
        let h = Handle::email("  Riker@Enterprise.ORG ").unwrap();
        assert_eq!(h.as_str(), "email:riker@enterprise.org");
        assert_eq!(h.kind(), HandleKind::Email);
        assert_eq!(h.value(), "riker@enterprise.org");
    }

    #[test]
    fn email_refuses_what_is_not_one_address() {
        for bad in [
            "",
            "riker",
            "@enterprise.org",
            "riker@",
            "riker@localhost",
            "Will Riker <riker@enterprise.org>",
            "riker@enterprise.org, troi@enterprise.org",
            "a@b@c.org",
        ] {
            assert_eq!(Handle::email(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn tel_keeps_digits_and_requires_a_country_code() {
        assert_eq!(
            Handle::tel("+1 (555) 012-3456").unwrap().as_str(),
            "tel:+15550123456"
        );
        assert_eq!(
            Handle::tel("+44 20.7946.0958").unwrap().as_str(),
            "tel:+442079460958"
        );
        for bad in [
            "5550123456",
            "+12345",
            "+1555012345678901",
            "+1 555 CALL NOW",
            "+0555012345",
            "+1123456",
            "+1 555 012 345",
            "+1 202 555 01234",
        ] {
            assert_eq!(Handle::tel(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn email_takes_a_mailto_uri_for_its_address() {
        let riker = Handle::email("riker@enterprise.org").unwrap();
        for spelling in [
            "mailto:riker@enterprise.org",
            "MAILTO:Riker@Enterprise.org",
            " mailto:riker@enterprise.org?subject=Away%20team ",
        ] {
            assert_eq!(Handle::email(spelling), Some(riker.clone()), "{spelling:?}");
        }
        assert_eq!(Handle::email("mailto:"), None);
        assert_eq!(Handle::parse("email:mailto:riker@enterprise.org"), None);
    }

    /// Every way the same number gets written — a vCard 4 `tel:` URI, an
    /// extension, a trunk `(0)`, a no-break space or an en dash from a
    /// phone's address book — is one handle, and that handle reads back
    /// through both `tel` and `parse` as itself.
    #[test]
    fn every_spelling_of_one_number_is_one_handle() {
        let numbers: [(&str, &[&str]); 3] = [
            ("+12025550101", &["1", "202", "555", "0101"]),
            ("+442079460958", &["44", "20", "7946", "0958"]),
            ("+33155501234", &["33", "1", "55", "50", "12", "34"]),
        ];
        let separators = [
            "", " ", "-", ".", "\u{00A0}", "\u{202F}", "\u{2009}", "\u{3000}", "\u{2010}",
            "\u{2011}", "\u{2013}", "\u{2014}", "\u{2212}", "\u{FF0D}",
        ];
        let wrappers: [fn(&str) -> String; 7] = [
            |n| n.to_string(),
            |n| format!("  {n}\t"),
            |n| format!("tel:{n}"),
            |n| format!("TEL:{n}"),
            |n| format!("{n};ext=42"),
            |n| format!("tel:{n};ext=42;phone-context=example.test"),
            |n| format!("\u{00A0}tel:{n};isub=1"),
        ];
        for (canonical, groups) in numbers {
            let want = Handle::tel(canonical).unwrap();
            assert_eq!(want.as_str(), format!("tel:{canonical}"));
            assert_eq!(Handle::tel(want.value()), Some(want.clone()));
            assert_eq!(Handle::parse(want.as_str()), Some(want.clone()));
            let (country, rest) = groups.split_first().unwrap();
            let mut spellings = Vec::new();
            for sep in separators {
                let national = rest.join(sep);
                spellings.push(format!("+{country}{sep}{national}"));
                let (area, subscriber) = rest.split_first().unwrap();
                let subscriber = subscriber.join(sep);
                spellings.push(format!("+{country} ({area}){sep}{subscriber}"));
                if *country != "1" {
                    spellings.push(format!("+{country}{sep}(0){area}{sep}{subscriber}"));
                    spellings.push(format!("+{country} (0) {area}{sep}{subscriber}"));
                }
            }
            let mut tried = 0;
            for spelling in &spellings {
                for wrap in wrappers {
                    let written = wrap(spelling);
                    let got = Handle::tel(&written);
                    assert_eq!(got.as_ref(), Some(&want), "{written:?}");
                    tried += 1;
                }
            }
            assert!(tried > 150, "{tried} spellings of {canonical}");
        }
    }

    #[test]
    fn a_zero_in_parentheses_is_dropped_only_after_the_country_code() {
        assert_eq!(
            Handle::tel("+44 (0)20 7946 0958").unwrap().as_str(),
            "tel:+442079460958"
        );
        assert_eq!(
            Handle::tel("+1 (202) 555-0101").unwrap().as_str(),
            "tel:+12025550101"
        );
        assert_eq!(
            Handle::tel("+49 30 (0)1234567").unwrap().as_str(),
            "tel:+493001234567",
            "a (0) further in is a digit someone bracketed, and kept"
        );
    }

    #[test]
    fn rebuild_reads_an_older_spelling_parse_refuses() {
        assert_eq!(
            Handle::rebuild("email:mailto:riker@enterprise.org"),
            Handle::email("riker@enterprise.org")
        );
        assert_eq!(Handle::rebuild("tel:+1123456"), None);
        assert_eq!(Handle::rebuild("fax:+12025550101"), None);
        let h = Handle::slack("T01", "U02").unwrap();
        assert_eq!(Handle::rebuild(h.as_str()), Some(h));
    }

    /// A WhatsApp sender and the same person's Signal number must be one
    /// handle, or a link made in one app does nothing in the other.
    #[test]
    fn whatsapp_person_jid_is_the_same_tel_handle_as_the_number() {
        let wa = Handle::whatsapp_jid("15550123456@s.whatsapp.net").unwrap();
        assert_eq!(wa, Handle::tel("+1 555 012 3456").unwrap());
        assert_eq!(
            Handle::whatsapp_jid("15550123456:12@s.whatsapp.net"),
            Some(wa)
        );
        assert_eq!(Handle::whatsapp_jid("120363000000000000@g.us"), None);
        assert_eq!(Handle::whatsapp_jid("123456789012345@lid"), None);
    }

    #[test]
    fn slack_is_scoped_to_its_workspace() {
        assert_eq!(
            Handle::slack("T01", "U02").unwrap().as_str(),
            "slack:T01/U02"
        );
        assert_eq!(Handle::slack("", "U02"), None);
        assert_eq!(Handle::slack("T01", "U02/x"), None);
        assert_eq!(
            Handle::slack("T_NCC1701D", "U_PICARD").unwrap().as_str(),
            "slack:T_NCC1701D/U_PICARD"
        );
    }

    #[test]
    fn signal_aci_is_a_uuid_however_it_was_written() {
        let aci = Handle::signal_aci("0195683AD14087F9BDF6234DA6D6880C").unwrap();
        assert_eq!(
            aci.as_str(),
            "signal_aci:0195683a-d140-87f9-bdf6-234da6d6880c"
        );
        assert_eq!(
            Handle::signal_aci(" 0195683a-d140-87f9-bdf6-234da6d6880c "),
            Some(aci)
        );
        for bad in [
            "",
            "0195683a",
            "0195683a-d140-87f9-bdf6-234da6d6880",
            "+15550123456",
            "g195683ad14087f9bdf6234da6d6880c",
        ] {
            assert_eq!(Handle::signal_aci(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn parse_round_trips_and_refuses_unnormalized_spellings() {
        for h in [
            Handle::email("riker@enterprise.org").unwrap(),
            Handle::tel("+15550123456").unwrap(),
            Handle::slack("T01", "U02").unwrap(),
            Handle::signal_aci("0195683ad14087f9bdf6234da6d6880c").unwrap(),
        ] {
            assert_eq!(Handle::parse(h.as_str()), Some(h.clone()));
        }
        assert_eq!(
            Handle::parse("signal_aci:0195683AD14087F9BDF6234DA6D6880C"),
            None
        );
        assert_eq!(Handle::parse("email:Riker@Enterprise.org"), None);
        assert_eq!(Handle::parse("tel:+1 555 012 3456"), None);
        assert_eq!(Handle::parse("fax:+15550123456"), None);
        assert_eq!(Handle::parse("riker@enterprise.org"), None);
    }

    #[test]
    fn every_kind_spells_and_parses_back() {
        for kind in HandleKind::VARIANTS {
            assert_eq!(HandleKind::parse(kind.as_str()), Some(*kind));
        }
    }

    /// A handle read back from a store goes through `parse`, so a row
    /// cannot carry a spelling the normalizers would not have produced.
    #[test]
    fn serde_round_trips_through_parse() {
        let h = Handle::tel("+15550123456").unwrap();
        let json = serde_json::to_string(&h).unwrap();
        assert_eq!(json, "\"tel:+15550123456\"");
        assert_eq!(serde_json::from_str::<Handle>(&json).unwrap(), h);
        assert!(serde_json::from_str::<Handle>("\"tel:+1 555\"").is_err());
    }

    /// The URI is what a chip link's href carries, so every kind has to
    /// come back from its own URI; the TS mirror in `chipLinks.js` is
    /// tested over the same cases.
    #[test]
    fn every_kind_round_trips_through_its_uri() {
        for (h, uri) in [
            (
                Handle::email("riker@enterprise.org").unwrap(),
                "mailto:riker@enterprise.org",
            ),
            (Handle::tel("+15550123456").unwrap(), "tel:+15550123456"),
            (
                Handle::slack("T01", "U02").unwrap(),
                "slack://user?team=T01&id=U02",
            ),
            (
                Handle::signal_aci("0195683ad14087f9bdf6234da6d6880c").unwrap(),
                "datalib:handle/signal_aci/0195683a-d140-87f9-bdf6-234da6d6880c",
            ),
            (
                Handle::facebook_name("Jean-Luc Picard").unwrap(),
                "datalib:handle/facebook/name/Jean-Luc%20Picard",
            ),
            (
                Handle::facebook_name("Beverly Crusher-Howard ☕").unwrap(),
                "datalib:handle/facebook/name/Beverly%20Crusher-Howard%20%E2%98%95",
            ),
            (
                Handle::facebook_deleted("1000000002").unwrap(),
                "datalib:handle/facebook/deleted/1000000002",
            ),
        ] {
            assert_eq!(h.to_uri(), uri);
            assert_eq!(Handle::from_uri(uri), Some(h));
        }
        assert_eq!(
            Handle::from_uri("datalib:handle/signal_aci/0195683AD14087F9BDF6234DA6D6880C"),
            Handle::signal_aci("0195683ad14087f9bdf6234da6d6880c"),
            "an older or looser spelling still reads back"
        );
        assert_eq!(
            Handle::from_uri("mailto:Riker@Enterprise.org?subject=hi"),
            Handle::email("riker@enterprise.org")
        );
        assert_eq!(
            Handle::from_uri("slack://user?id=U02&team=T01"),
            Handle::slack("T01", "U02")
        );
        assert_eq!(Handle::from_uri("https://enterprise.org/riker"), None);
        assert_eq!(Handle::from_uri("slack://channel?team=T01&id=C03"), None);
        assert_eq!(Handle::from_uri("datalib:group/slack"), None);
        // The spelling for a kind with no scheme of its own; every kind
        // this build has one, so a known kind reads back and an unknown
        // one is nothing.
        assert_eq!(
            Handle::from_uri("datalib:handle/tel/+15550123456"),
            Handle::tel("+15550123456")
        );
        assert_eq!(Handle::from_uri("datalib:handle/fax/+15550123456"), None);
    }

    #[test]
    fn describe_puts_the_identifier_beside_the_name_once() {
        let email = Handle::email("riker@enterprise.org").unwrap();
        assert_eq!(
            email.describe("Will Riker"),
            "Will Riker <riker@enterprise.org>"
        );
        assert_eq!(
            email.describe("riker@enterprise.org"),
            "riker@enterprise.org"
        );
        assert_eq!(email.describe("  "), "riker@enterprise.org");
        let tel = Handle::tel("+15550123456").unwrap();
        assert_eq!(tel.describe("Will Riker"), "Will Riker (+15550123456)");
        let slack = Handle::slack("T01", "U02").unwrap();
        assert_eq!(slack.describe("Data"), "Data (slack:T01/U02)");
        let aci = Handle::signal_aci("0195683ad14087f9bdf6234da6d6880f").unwrap();
        assert_eq!(
            aci.describe("Q"),
            "Q (signal_aci:0195683a-d140-87f9-bdf6-234da6d6880f)"
        );
        let deleted = Handle::facebook_deleted("42").unwrap();
        assert_eq!(
            deleted.describe("Facebook user"),
            "Facebook user (facebook:deleted/42)"
        );
    }

    #[test]
    fn a_facebook_name_is_its_words_and_nothing_else() {
        let riker = Handle::facebook_name("William Riker").unwrap();
        assert_eq!(riker.as_str(), "facebook:name/William Riker");
        assert_eq!(
            Handle::facebook_name("  William \t Riker\n"),
            Some(riker.clone())
        );
        assert_eq!(Handle::parse("facebook:name/William Riker"), Some(riker));
        assert_eq!(Handle::parse("facebook:name/William  Riker"), None);
        for bad in ["", "   ", "Q\u{0}"] {
            assert_eq!(Handle::facebook_name(bad), None, "{bad:?}");
        }
        assert_eq!(Handle::facebook_deleted("12a"), None);
        assert_eq!(Handle::facebook_deleted(""), None);
        assert_eq!(Handle::parse("facebook:Q"), None);
        assert_eq!(Handle::parse("facebook:other/Q"), None);
    }

    #[test]
    fn a_malformed_escape_names_no_handle() {
        assert_eq!(Handle::from_uri("datalib:handle/facebook/name/Q%2"), None);
        assert_eq!(Handle::from_uri("datalib:handle/facebook/name/Q%FF"), None);
    }
}
