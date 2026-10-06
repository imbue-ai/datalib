//! A handle: one identifier for a person in one namespace, normalized so
//! that the same person reached two ways reads the same.
//!
//! A handle is `<kind>:<value>` — `email:riker@enterprise.org`,
//! `tel:+12025550123`, `slack:T01/U02`. Where a native id *is* an email
//! address or a phone number it becomes one of those, not a per-app kind,
//! so one link covers every app that reaches a person by that number.
//! Renders write handles into the markdown (`data-handle`) and the
//! contacts app links them to contacts; nothing here knows what a contact
//! is. `docs/dev/plans/contacts.md` has the design.
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
/// another way.
pub const RULES_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, EnumString, IntoStaticStr, VariantArray)]
#[strum(serialize_all = "snake_case")]
pub enum HandleKind {
    Email,
    Tel,
    Slack,
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

    fn of(kind: HandleKind, value: &str) -> Self {
        Self(format!("{}:{value}", kind.as_str()))
    }
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
    fn parse_round_trips_and_refuses_unnormalized_spellings() {
        for h in [
            Handle::email("riker@enterprise.org").unwrap(),
            Handle::tel("+15550123456").unwrap(),
            Handle::slack("T01", "U02").unwrap(),
        ] {
            assert_eq!(Handle::parse(h.as_str()), Some(h.clone()));
        }
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
}
