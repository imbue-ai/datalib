//! Ternary labels: a row says a label is in or out of a subject, and no
//! row says nobody knows, which is most of them. Who said so is on the
//! row: a person, or an agent whose word waits for one. These are the
//! rules, over values alone; nothing here knows what a subject is, so
//! they can serve any store that labels things. The contacts store is
//! the first (`review.rs`). Each label stands alone: what one says never
//! changes another.

use crate::schema::labels::LabelRow;
use crate::DecidedHow;

/// A label as a person or an agent may write it: trimmed, lowercased,
/// its inner whitespace a `-`. `None` for one that is empty, too long or
/// holds a control character.
pub fn normalize_label(raw: &str) -> Option<String> {
    let words: Vec<String> = raw.split_whitespace().map(str::to_lowercase).collect();
    let label = words.join("-");
    let ok =
        !label.is_empty() && label.chars().count() <= 64 && !label.chars().any(char::is_control);
    ok.then_some(label)
}

/// The label as the subject holds it: in or out, and whether a person
/// has had their say (they made it, or reviewed an agent's).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Held {
    pub applies: bool,
    pub settled: bool,
}

/// What an agent's label comes to, given what the subject holds of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LabelPlan {
    Write,
    Already,
    Refused(String),
}

/// An agent may write over nothing, or over an agent's answer no person
/// has looked at; a person's answer stands.
pub(crate) fn plan_agent_label(held: Option<Held>, applies: bool) -> LabelPlan {
    match held {
        Some(h) if h.settled && h.applies == applies => LabelPlan::Already,
        Some(h) if h.settled => LabelPlan::Refused("a person said otherwise".to_string()),
        _ => LabelPlan::Write,
    }
}

/// A person's in or out over the stored row. Saying what an agent said
/// confirms it, and the row still says the agent made it; saying the
/// opposite overrides it, and the row is the person's but still names
/// the agent, which said the other thing, since a label is in or out.
pub(crate) fn person_label(
    stored: Option<&LabelRow>,
    subject: &str,
    label: &str,
    applies: bool,
    now: &str,
    tz: &str,
) -> LabelRow {
    let agrees = stored.is_some_and(|r| r.decided_how == DecidedHow::Agent && r.applies == applies);
    LabelRow {
        subject: subject.to_string(),
        label: label.to_string(),
        applies,
        decided_how: if agrees {
            DecidedHow::Agent
        } else {
            DecidedHow::Manual
        },
        agent: stored.and_then(|r| r.agent.clone()),
        reason: stored.and_then(|r| r.reason.clone()),
        decided_at_utc: match stored {
            Some(r) if agrees => r.decided_at_utc.clone(),
            _ => now.to_string(),
        },
        reviewed_at_utc: Some(now.to_string()),
        tz_offset: tz.to_string(),
        confidence: stored.and_then(|r| r.confidence),
        run: stored.and_then(|r| r.run.clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_agent_labels_only_where_no_person_has_said_otherwise() {
        let held = |applies, settled| Some(Held { applies, settled });
        assert_eq!(plan_agent_label(None, true), LabelPlan::Write);
        assert_eq!(plan_agent_label(held(true, true), true), LabelPlan::Already);
        assert!(matches!(
            plan_agent_label(held(false, true), true),
            LabelPlan::Refused(_)
        ));
        assert_eq!(
            plan_agent_label(held(true, false), false),
            LabelPlan::Write,
            "an agent may change its mind while no person has looked"
        );
    }

    #[test]
    fn a_label_is_a_lowercase_word_or_phrase() {
        assert_eq!(
            normalize_label("  Bridge  Crew "),
            Some("bridge-crew".into())
        );
        assert_eq!(normalize_label("Real"), Some("real".into()));
        assert_eq!(normalize_label("  "), None);
        assert_eq!(normalize_label(&"x".repeat(65)), None);
    }
}
