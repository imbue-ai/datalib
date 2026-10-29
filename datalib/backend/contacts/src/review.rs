//! Agents propose, a person decides: links and contacts an agent made,
//! the links a person turned down, and the labels on people and senders
//! (clusters, and how much each matters; the label rules are `labels.rs`). An agent's word waits for a
//! person to confirm it; a person's stands, and no agent may change it. `docs/dev/contacts.md` §"Agents
//! propose, a person decides".

use std::collections::{BTreeSet, HashMap};

use anyhow::{bail, Context, Result};
use datalib_contact_schema::ContactKind;
use datalib_handle::Handle;
use datalib_time::IsoOffsetTimestamp;
use serde::{Deserialize, Serialize};
use sqlx::Row;

use crate::labels::{normalize_label, person_label, plan_agent_label, Held, LabelPlan};
use crate::schema::labels::LabelRow;
use crate::schema::not_linked::NotLinkedRow;
use crate::{
    contact_subject, holder, insert_contact, insert_handle, plan_link, DecidedHow, LinkPlan, Maker,
    Store,
};

/// One batch from one agent, sealed as one commit. `run` names the
/// agent's run, so its proposals can be told from another run's.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Proposals {
    pub agent: String,
    #[serde(default)]
    pub run: Option<String>,
    #[serde(default)]
    pub contacts: Vec<ProposedContact>,
    #[serde(default)]
    pub handles: Vec<ProposedHandle>,
}

/// Handles that are one person: added to `contact_id`, or to a new
/// contact called `name`; and the labels the person should carry.
/// `confidence`, 0 to 1, is how sure the agent is the handles are one
/// person: the review shows the least sure first.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ProposedContact {
    #[serde(default)]
    pub contact_id: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub handles: Vec<String>,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub confidence: Option<f64>,
    #[serde(default)]
    pub labels: Vec<ProposedLabel>,
}

/// Labels for a handle on its own: a sender no contact holds, or one
/// address of a contact that differs from the rest.
#[derive(Debug, Clone, Deserialize)]
pub struct ProposedHandle {
    pub handle: String,
    #[serde(default)]
    pub labels: Vec<ProposedLabel>,
}

/// A label an agent says applies, or, for a close call a person should
/// settle, says does not (`applies: false`).
#[derive(Debug, Clone, Deserialize)]
pub struct ProposedLabel {
    pub label: String,
    #[serde(default = "yes")]
    pub applies: bool,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub confidence: Option<f64>,
}

fn yes() -> bool {
    true
}

/// What a batch came to. Each part an agent could not have is named in
/// `skipped` with the reason, and the rest of the batch still lands.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Proposed {
    pub contacts_created: usize,
    pub linked: usize,
    pub labelled: usize,
    pub skipped: Vec<Skipped>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Skipped {
    pub what: String,
    pub why: String,
}

/// A confidence, if any, between 0 and 1; anything else is the agent's
/// mistake, and the proposal carrying it is skipped.
fn check_confidence(c: Option<f64>) -> Result<Option<f64>, String> {
    match c {
        Some(c) if !(0.0..=1.0).contains(&c) => {
            Err(format!("confidence {c} is not between 0 and 1"))
        }
        c => Ok(c),
    }
}

/// The rejected grouping a new contact's handles would repeat, if any:
/// one whose every handle is among them. A grouping is the handles of an
/// agent's contact a person rejected whole; proposing fewer of them is a
/// different claim and may be made.
fn repeats_grouping<'g>(
    proposed: &BTreeSet<&str>,
    groupings: &'g [(String, BTreeSet<String>)],
) -> Option<&'g str> {
    groupings
        .iter()
        .find(|(_, g)| !g.is_empty() && g.iter().all(|h| proposed.contains(h.as_str())))
        .map(|(name, _)| name.as_str())
}

/// Every grouping a person rejected, as the name its contact had and
/// its handles.
async fn rejected_groupings(
    conn: &mut sqlx::SqliteConnection,
) -> Result<Vec<(String, BTreeSet<String>)>> {
    let rows: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT contact_id, contact_name, handle FROM not_linked \
          WHERE contact_rejected = 1 ORDER BY contact_id, handle",
    )
    .fetch_all(&mut *conn)
    .await
    .context("read the rejected groupings")?;
    let mut by_contact: Vec<(String, String, BTreeSet<String>)> = Vec::new();
    for (id, name, handle) in rows {
        match by_contact.last_mut() {
            Some((last, _, set)) if *last == id => {
                set.insert(handle);
            }
            _ => by_contact.push((id, name, BTreeSet::from([handle]))),
        }
    }
    Ok(by_contact.into_iter().map(|(_, n, s)| (n, s)).collect())
}

/// What an agent's link comes to, given who holds the handle and
/// whether a person turned this pair down.
#[derive(Debug, Clone, PartialEq, Eq)]
enum AgentLink {
    Insert,
    Already,
    Refused(String),
}

fn plan_agent_link(holder: Option<&str>, turned_down: bool, target: &str) -> AgentLink {
    if turned_down {
        return AgentLink::Refused("a person said it is not this contact".to_string());
    }
    match plan_link(holder, target) {
        LinkPlan::Insert => AgentLink::Insert,
        LinkPlan::AlreadyLinked => AgentLink::Already,
        LinkPlan::HeldBy(other) => AgentLink::Refused(format!("already belongs to {other}")),
    }
}

/// A person's word on one label inside `tx`: in, out, or (`None`)
/// unknown again, which forgets the row. Returns whether anything
/// changed.
async fn person_says(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    subject: &str,
    label: &str,
    applies: Option<bool>,
    now: &str,
    tz: &str,
) -> Result<bool> {
    let held = labels_of(tx, subject).await?;
    let stored = held.iter().find(|r| r.label == label);
    let Some(applies) = applies else {
        sqlx::query("DELETE FROM labels WHERE subject = ? AND label = ?")
            .bind(subject)
            .bind(label)
            .execute(&mut **tx)
            .await
            .context("forget a label")?;
        return Ok(stored.is_some());
    };
    let settled = stored.is_some_and(|r| r.applies == applies && r.reviewed_at_utc.is_some());
    if settled {
        return Ok(false);
    }
    let row = person_label(stored, subject, label, applies, now, tz);
    write_label(tx, &row)
        .await
        .context("write a person's label")?;
    Ok(true)
}

/// The labels an agent proposed for `subject` that no person has
/// looked at yet, as `(label, applies)`.
async fn awaiting_labels(
    conn: &mut sqlx::SqliteConnection,
    subject: &str,
) -> Result<Vec<(String, bool)>> {
    sqlx::query_as(
        "SELECT label, applies FROM labels \
          WHERE subject = ? AND decided_how = 'agent' AND reviewed_at_utc IS NULL ORDER BY label",
    )
    .bind(subject)
    .fetch_all(&mut *conn)
    .await
    .context("read the labels waiting for review")
}

/// Unlink `handle` inside `tx`, writing a turn-down where an agent made
/// the link. Returns whether it was linked.
pub(crate) async fn unlink_in(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    handle: &str,
    now: &str,
    tz: &str,
) -> Result<bool> {
    let Some(r) = sqlx::query(
        "SELECT h.contact_id, h.linked_how, h.agent, h.reason, c.name \
           FROM handles h LEFT JOIN contacts c USING (contact_id) WHERE h.handle = ?",
    )
    .bind(handle)
    .fetch_optional(&mut **tx)
    .await
    .context("read the link to undo")?
    else {
        return Ok(false);
    };
    let how: String = r.get("linked_how");
    if DecidedHow::parse(&how) == Some(DecidedHow::Agent) {
        let contact_id: String = r.get("contact_id");
        let row = NotLinkedRow {
            handle: handle.to_string(),
            contact_name: r
                .get::<Option<String>, _>("name")
                .unwrap_or_else(|| contact_id.clone()),
            contact_id,
            agent: r.get("agent"),
            reason: r.get("reason"),
            decided_at_utc: now.to_string(),
            tz_offset: tz.to_string(),
            contact_rejected: false,
        };
        sqlx::query(
            "INSERT OR REPLACE INTO not_linked (handle, contact_id, contact_name, agent, \
               reason, decided_at_utc, tz_offset, contact_rejected) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&row.handle)
        .bind(&row.contact_id)
        .bind(&row.contact_name)
        .bind(&row.agent)
        .bind(&row.reason)
        .bind(&row.decided_at_utc)
        .bind(&row.tz_offset)
        .bind(row.contact_rejected)
        .execute(&mut **tx)
        .await
        .context("write down a turned-down link")?;
    }
    sqlx::query("DELETE FROM handles WHERE handle = ?")
        .bind(handle)
        .execute(&mut **tx)
        .await
        .context("unlink a handle")?;
    Ok(true)
}

/// `labels` has a two-column key, which `PortableTable` derives no
/// upsert for.
async fn write_label(conn: &mut sqlx::SqliteConnection, row: &LabelRow) -> Result<()> {
    sqlx::query(
        "INSERT OR REPLACE INTO labels (subject, label, applies, decided_how, agent, reason, \
           decided_at_utc, reviewed_at_utc, tz_offset, confidence, run) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&row.subject)
    .bind(&row.label)
    .bind(row.applies)
    .bind(row.decided_how.as_str())
    .bind(&row.agent)
    .bind(&row.reason)
    .bind(&row.decided_at_utc)
    .bind(&row.reviewed_at_utc)
    .bind(&row.tz_offset)
    .bind(row.confidence)
    .bind(&row.run)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

async fn labels_of(conn: &mut sqlx::SqliteConnection, subject: &str) -> Result<Vec<LabelRow>> {
    let rows = sqlx::query(
        "SELECT subject, label, applies, decided_how, agent, reason, decided_at_utc, \
                reviewed_at_utc, tz_offset, confidence, run FROM labels WHERE subject = ? \
          ORDER BY label",
    )
    .bind(subject)
    .fetch_all(&mut *conn)
    .await
    .context("read a subject's labels")?;
    Ok(rows.iter().map(label_row).collect())
}

/// A stored label as a row. A spelling of `decided_how` this build does
/// not know reads as an agent's, so it is put to a person again rather
/// than taken as theirs.
fn label_row(r: &sqlx::sqlite::SqliteRow) -> LabelRow {
    let how: String = r.get("decided_how");
    LabelRow {
        subject: r.get("subject"),
        label: r.get("label"),
        applies: r.get("applies"),
        decided_how: DecidedHow::parse(&how).unwrap_or(DecidedHow::Agent),
        agent: r.get("agent"),
        reason: r.get("reason"),
        decided_at_utc: r.get("decided_at_utc"),
        reviewed_at_utc: r.get("reviewed_at_utc"),
        tz_offset: r.get("tz_offset"),
        confidence: r.get("confidence"),
        run: r.get("run"),
    }
}

/// The contact a possibly merged-away id answers as, and its name.
async fn survivor(
    conn: &mut sqlx::SqliteConnection,
    contact_id: &str,
) -> Result<Option<(String, String)>> {
    let row = sqlx::query(
        "SELECT c.contact_id, c.name FROM contacts c \
           JOIN contacts asked ON c.contact_id = COALESCE(asked.merged_into, asked.contact_id) \
          WHERE asked.contact_id = ?",
    )
    .bind(contact_id)
    .fetch_optional(&mut *conn)
    .await
    .context("read a contact")?;
    Ok(row.map(|r| (r.get("contact_id"), r.get("name"))))
}

/// A subject is a handle as `datalib_handle` spells it, or
/// `contact:<id>` for a contact the store holds; the survivor's for a
/// merged-away one.
async fn check_subject(conn: &mut sqlx::SqliteConnection, subject: &str) -> Result<String> {
    if let Some(id) = subject.strip_prefix("contact:") {
        return match survivor(conn, id).await? {
            Some((id, _)) => Ok(contact_subject(&id)),
            None => bail!("no contact {id}"),
        };
    }
    match Handle::parse(subject) {
        Some(h) => Ok(h.as_str().to_string()),
        None => bail!("{subject:?} is not a handle or a contact"),
    }
}

/// One agent's labels on one subject, each written if
/// [`plan_agent_label`] lets it. Returns how many it wrote.
async fn agent_labels(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    subject: &str,
    labels: &[ProposedLabel],
    batch: &Batch<'_>,
    skipped: &mut Vec<Skipped>,
) -> Result<usize> {
    let mut written = 0;
    for p in labels {
        let Some(label) = normalize_label(&p.label) else {
            skipped.push(Skipped {
                what: format!("label {:?} on {subject}", p.label),
                why: "not a label".to_string(),
            });
            continue;
        };
        let confidence = match check_confidence(p.confidence) {
            Ok(c) => c,
            Err(why) => {
                skipped.push(Skipped {
                    what: format!("label {label} on {subject}"),
                    why,
                });
                continue;
            }
        };
        let held = labels_of(tx, subject)
            .await?
            .into_iter()
            .find(|r| r.label == label)
            .map(|r| Held {
                settled: r.decided_how == DecidedHow::Manual || r.reviewed_at_utc.is_some(),
                applies: r.applies,
            });
        match plan_agent_label(held, p.applies) {
            LabelPlan::Already => {}
            LabelPlan::Refused(why) => skipped.push(Skipped {
                what: format!("label {label} on {subject}"),
                why,
            }),
            LabelPlan::Write => {
                let row = LabelRow {
                    subject: subject.to_string(),
                    label,
                    applies: p.applies,
                    decided_how: DecidedHow::Agent,
                    agent: Some(batch.agent.to_string()),
                    reason: p.reason.clone(),
                    decided_at_utc: batch.now.to_string(),
                    reviewed_at_utc: None,
                    tz_offset: batch.tz.to_string(),
                    confidence,
                    run: batch.run.map(str::to_string),
                };
                write_label(tx, &row)
                    .await
                    .context("write an agent's label")?;
                written += 1;
            }
        }
    }
    Ok(written)
}

/// Who is proposing, and when: what every row a batch writes carries.
struct Batch<'a> {
    agent: &'a str,
    run: Option<&'a str>,
    now: &'a str,
    tz: &'a str,
}

/// A link as the review shows it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ReviewLink {
    pub handle: String,
    pub linked_how: DecidedHow,
    pub agent: Option<String>,
    pub reason: Option<String>,
    pub confidence: Option<f64>,
    pub run: Option<String>,
    pub linked_at_utc: String,
    pub reviewed_at_utc: Option<String>,
}

impl ReviewLink {
    pub fn awaiting(&self) -> bool {
        self.linked_how == DecidedHow::Agent && self.reviewed_at_utc.is_none()
    }
}

/// A label as the review shows it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ReviewLabel {
    pub label: String,
    pub applies: bool,
    pub decided_how: DecidedHow,
    pub agent: Option<String>,
    pub reason: Option<String>,
    pub confidence: Option<f64>,
    pub run: Option<String>,
    pub decided_at_utc: String,
    pub reviewed_at_utc: Option<String>,
}

impl ReviewLabel {
    pub fn awaiting(&self) -> bool {
        self.decided_how == DecidedHow::Agent && self.reviewed_at_utc.is_none()
    }

    /// A person said the opposite of what an agent said.
    pub fn overrode(&self) -> bool {
        self.decided_how == DecidedHow::Manual && self.agent.is_some()
    }
}

impl From<LabelRow> for ReviewLabel {
    fn from(r: LabelRow) -> Self {
        Self {
            label: r.label,
            applies: r.applies,
            decided_how: r.decided_how,
            agent: r.agent,
            reason: r.reason,
            confidence: r.confidence,
            run: r.run,
            decided_at_utc: r.decided_at_utc,
            reviewed_at_utc: r.reviewed_at_utc,
        }
    }
}

/// One thing to review: a contact, with every link and label it holds,
/// or a handle with labels of its own (and the contact holding it, if
/// any).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ReviewItem {
    pub subject: String,
    pub contact_id: Option<String>,
    pub name: Option<String>,
    /// The agent that made the contact, where one did.
    pub contact_agent: Option<String>,
    pub handle: Option<String>,
    pub links: Vec<ReviewLink>,
    pub labels: Vec<ReviewLabel>,
}

impl ReviewItem {
    pub fn awaiting(&self) -> bool {
        self.links.iter().any(ReviewLink::awaiting) || self.labels.iter().any(ReviewLabel::awaiting)
    }

    /// The least sure the agent was of anything here that waits for a
    /// person: what the review sorts close calls first by. `None` when
    /// nothing waiting says how sure it is.
    pub fn least_sure(&self) -> Option<f64> {
        let links = self
            .links
            .iter()
            .filter(|l| l.awaiting())
            .filter_map(|l| l.confidence);
        let labels = self
            .labels
            .iter()
            .filter(|l| l.awaiting())
            .filter_map(|l| l.confidence);
        links.chain(labels).reduce(f64::min)
    }

    /// What the subject's `label` row says: in, out, or `None` for no row.
    pub fn holds(&self, label: &str) -> Option<bool> {
        self.labels
            .iter()
            .find(|l| l.label == label)
            .map(|l| l.applies)
    }
}

/// A link a person turned down, as agents read it before proposing.
/// `contact_rejected`: the person rejected the agent's whole contact,
/// so its handles together are a grouping not to propose again.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TurnedDown {
    pub handle: String,
    pub contact_id: String,
    pub contact_name: String,
    pub agent: Option<String>,
    pub reason: Option<String>,
    pub decided_at_utc: String,
    pub contact_rejected: bool,
}

impl Store {
    /// Write an agent's batch: new contacts, links to new or existing
    /// ones, and labels. A link someone holds or a person turned down,
    /// and a label a person answered the other way, are
    /// skipped and named; so are a new contact none of whose handles could
    /// be linked, and one repeating a grouping a person rejected.
    pub async fn propose(&self, p: &Proposals) -> Result<Proposed> {
        let agent = p.agent.trim();
        if agent.is_empty() {
            bail!("a proposal names its agent");
        }
        let run = p.run.as_deref().map(str::trim).filter(|r| !r.is_empty());
        let (now, tz) = IsoOffsetTimestamp::now_local().to_utc_and_offset();
        let batch = Batch {
            agent,
            run,
            now: &now,
            tz: &tz,
        };
        let mut out = Proposed::default();
        let mut tx = self.pool.begin().await?;
        let groupings = rejected_groupings(&mut tx).await?;
        for c in &p.contacts {
            let confidence = match check_confidence(c.confidence) {
                Ok(conf) => conf,
                Err(why) => {
                    out.skipped.push(Skipped {
                        what: format!("contact {}", c.name.as_deref().unwrap_or("")),
                        why,
                    });
                    continue;
                }
            };
            let by = Maker::Agent {
                agent,
                reason: c.reason.as_deref(),
                confidence,
                run,
            };
            let mut handles = Vec::new();
            for h in &c.handles {
                match Handle::parse(h) {
                    Some(h) => handles.push(h),
                    None => out.skipped.push(Skipped {
                        what: h.clone(),
                        why: "not a handle".to_string(),
                    }),
                }
            }
            let target = match c.contact_id.as_deref() {
                Some(id) => match survivor(&mut tx, id).await? {
                    Some((id, _)) => Some(id),
                    None => {
                        out.skipped.push(Skipped {
                            what: format!("contact {id}"),
                            why: "no such contact".to_string(),
                        });
                        continue;
                    }
                },
                None => None,
            };
            let mut plans = Vec::new();
            for h in &handles {
                let held = holder(&mut tx, h).await?;
                let plan = match &target {
                    Some(id) => {
                        let refused = turned_down(&mut tx, h.as_str(), id).await?;
                        plan_agent_link(held.as_deref(), refused, id)
                    }
                    None => plan_agent_link(held.as_deref(), false, ""),
                };
                plans.push((h, plan));
            }
            let contact_id = match target {
                Some(id) => id,
                None => {
                    let name = c.name.as_deref().map(str::trim).unwrap_or("");
                    if name.is_empty() {
                        out.skipped.push(Skipped {
                            what: format!("a new contact with {}", c.handles.join(", ")),
                            why: "a new contact needs a name".to_string(),
                        });
                        continue;
                    }
                    let proposed: BTreeSet<&str> = handles.iter().map(Handle::as_str).collect();
                    if let Some(was) = repeats_grouping(&proposed, &groupings) {
                        out.skipped.push(Skipped {
                            what: format!("new contact {name:?}"),
                            why: format!(
                                "a person rejected these handles as one contact ({was:?})"
                            ),
                        });
                        continue;
                    }
                    if !plans.iter().any(|(_, p)| *p == AgentLink::Insert) {
                        out.skipped.push(Skipped {
                            what: format!("new contact {name:?}"),
                            why: "none of its handles could be linked".to_string(),
                        });
                        for (h, plan) in plans {
                            if let AgentLink::Refused(why) = plan {
                                out.skipped.push(Skipped {
                                    what: h.to_string(),
                                    why,
                                });
                            }
                        }
                        continue;
                    }
                    out.contacts_created += 1;
                    insert_contact(&mut tx, name, ContactKind::Person, by, &now, &tz).await?
                }
            };
            for (h, plan) in plans {
                match plan {
                    AgentLink::Insert => {
                        insert_handle(&mut tx, h, &contact_id, by, &now, &tz).await?;
                        out.linked += 1;
                    }
                    AgentLink::Already => {}
                    AgentLink::Refused(why) => out.skipped.push(Skipped {
                        what: h.to_string(),
                        why,
                    }),
                }
            }
            let subject = contact_subject(&contact_id);
            out.labelled +=
                agent_labels(&mut tx, &subject, &c.labels, &batch, &mut out.skipped).await?;
        }
        for v in &p.handles {
            let Some(h) = Handle::parse(&v.handle) else {
                out.skipped.push(Skipped {
                    what: v.handle.clone(),
                    why: "not a handle".to_string(),
                });
                continue;
            };
            out.labelled +=
                agent_labels(&mut tx, h.as_str(), &v.labels, &batch, &mut out.skipped).await?;
        }
        tx.commit().await?;
        if out.contacts_created + out.linked + out.labelled > 0 {
            let run = run.map_or(String::new(), |r| format!(" (run {r})"));
            self.seal(&format!(
                "contacts: {agent}{run} proposed {} new contacts, {} links, {} labels",
                out.contacts_created, out.linked, out.labelled
            ))
            .await?;
        }
        Ok(out)
    }

    /// A person confirms what agents proposed for each subject: a
    /// contact's links and labels, or a handle's link and labels.
    /// Returns how many links and labels it confirmed.
    pub async fn confirm(&self, subjects: &[String]) -> Result<usize> {
        self.review_with(subjects, true).await
    }

    /// A person turns down what agents proposed for each subject: each
    /// link waiting for review is unlinked and written down in
    /// `not_linked`, and each label waiting for review is flipped, so a
    /// label the agent put in is out and one it put out is in. A contact
    /// an agent made that holds no handle afterwards goes too, since it
    /// was only the agent's idea. Returns how many it turned down.
    pub async fn reject(&self, subjects: &[String]) -> Result<usize> {
        self.review_with(subjects, false).await
    }

    async fn review_with(&self, subjects: &[String], agree: bool) -> Result<usize> {
        let (now, tz) = IsoOffsetTimestamp::now_local().to_utc_and_offset();
        let mut tx = self.pool.begin().await?;
        let mut done = 0;
        for s in subjects {
            let subject = check_subject(&mut tx, s).await?;
            let pending: Vec<String> = match subject.strip_prefix("contact:") {
                Some(id) => sqlx::query_scalar(
                    "SELECT handle FROM handles \
                      WHERE contact_id = ? AND linked_how = 'agent' AND reviewed_at_utc IS NULL",
                )
                .bind(id),
                None => sqlx::query_scalar(
                    "SELECT handle FROM handles \
                      WHERE handle = ? AND linked_how = 'agent' AND reviewed_at_utc IS NULL",
                )
                .bind(&subject),
            }
            .fetch_all(&mut *tx)
            .await
            .context("read the links waiting for review")?;
            for h in &pending {
                if agree {
                    sqlx::query(
                        "UPDATE handles SET reviewed_at_utc = ?, tz_offset = ? WHERE handle = ?",
                    )
                    .bind(&now)
                    .bind(&tz)
                    .bind(h)
                    .execute(&mut *tx)
                    .await
                    .context("confirm an agent's link")?;
                    done += 1;
                } else if unlink_in(&mut tx, h, &now, &tz).await? {
                    done += 1;
                }
            }
            for (label, applies) in awaiting_labels(&mut tx, &subject).await? {
                let says = if agree { applies } else { !applies };
                if person_says(&mut tx, &subject, &label, Some(says), &now, &tz).await? {
                    done += 1;
                }
            }
            if let Some(id) = subject.strip_prefix("contact:").filter(|_| !agree) {
                let emptied: Option<i64> = sqlx::query_scalar(
                    "SELECT 1 FROM contacts WHERE contact_id = ?1 AND agent IS NOT NULL \
                      AND NOT EXISTS (SELECT 1 FROM handles WHERE contact_id = ?1) \
                      AND NOT EXISTS (SELECT 1 FROM fields WHERE contact_id = ?1) \
                      AND NOT EXISTS (SELECT 1 FROM contacts WHERE merged_into = ?1)",
                )
                .bind(id)
                .fetch_optional(&mut *tx)
                .await
                .context("read whether an agent's contact is empty")?;
                if emptied.is_some() {
                    for sql in [
                        "UPDATE not_linked SET contact_rejected = 1 WHERE contact_id = ?1",
                        "DELETE FROM labels WHERE subject = 'contact:' || ?1",
                        "DELETE FROM contacts WHERE contact_id = ?1",
                    ] {
                        sqlx::query(sql)
                            .bind(id)
                            .execute(&mut *tx)
                            .await
                            .context("drop an emptied agent contact")?;
                    }
                }
            }
        }
        tx.commit().await?;
        if done > 0 {
            let what = if agree { "confirmed" } else { "turned down" };
            self.seal(&format!(
                "contacts: {what} {done} of an agent's links and labels"
            ))
            .await?;
        }
        Ok(done)
    }

    /// A person's word on `label` for `subject`: in (`Some(true)`), out
    /// (`Some(false)`), or unknown again (`None`, which forgets the row,
    /// an agent's included). Returns whether anything changed.
    pub async fn set_label(
        &self,
        subject: &str,
        label: &str,
        applies: Option<bool>,
    ) -> Result<bool> {
        let Some(label) = normalize_label(label) else {
            bail!("{label:?} is not a label: give a word or a short phrase");
        };
        let (now, tz) = IsoOffsetTimestamp::now_local().to_utc_and_offset();
        let mut tx = self.pool.begin().await?;
        let subject = check_subject(&mut tx, subject).await?;
        let changed = person_says(&mut tx, &subject, &label, applies, &now, &tz).await?;
        tx.commit().await?;
        if changed {
            let what = match applies {
                Some(true) => "in",
                Some(false) => "out",
                None => "unknown",
            };
            self.seal(&format!("contacts: {subject} is {what} of {label}"))
                .await?;
        }
        Ok(changed)
    }

    /// Every contact with a label, a link an agent made, or made by an
    /// agent itself, and every handle with a label of its own: what the
    /// review lists, contacts by name, then handles.
    pub async fn review(&self) -> Result<Vec<ReviewItem>> {
        let mut labels: HashMap<String, Vec<ReviewLabel>> = HashMap::new();
        for r in sqlx::query(
            "SELECT subject, label, applies, decided_how, agent, reason, decided_at_utc, \
                    reviewed_at_utc, tz_offset, confidence, run FROM labels ORDER BY subject, label",
        )
        .fetch_all(&self.pool)
        .await
        .context("read the labels")?
        {
            let row = label_row(&r);
            labels
                .entry(row.subject.clone())
                .or_default()
                .push(row.into());
        }
        let mut links: HashMap<String, Vec<ReviewLink>> = HashMap::new();
        let mut holders: HashMap<String, String> = HashMap::new();
        for r in sqlx::query(
            "SELECT handle, contact_id, linked_how, agent, reason, confidence, run, linked_at_utc, \
                    reviewed_at_utc FROM handles ORDER BY contact_id, handle",
        )
        .fetch_all(&self.pool)
        .await
        .context("read the links")?
        {
            let how: String = r.get("linked_how");
            let handle: String = r.get("handle");
            let contact_id: String = r.get("contact_id");
            holders.insert(handle.clone(), contact_id.clone());
            links.entry(contact_id).or_default().push(ReviewLink {
                handle,
                linked_how: DecidedHow::parse(&how).unwrap_or(DecidedHow::Agent),
                agent: r.get("agent"),
                reason: r.get("reason"),
                confidence: r.get("confidence"),
                run: r.get("run"),
                linked_at_utc: r.get("linked_at_utc"),
                reviewed_at_utc: r.get("reviewed_at_utc"),
            });
        }
        let contacts = sqlx::query(
            "SELECT contact_id, name, agent FROM contacts WHERE merged_into IS NULL \
              ORDER BY name COLLATE NOCASE, contact_id",
        )
        .fetch_all(&self.pool)
        .await
        .context("read the contacts")?;
        let mut names: HashMap<String, String> = HashMap::new();
        let mut out = Vec::new();
        for r in &contacts {
            let id: String = r.get("contact_id");
            let name: String = r.get("name");
            names.insert(id.clone(), name.clone());
            let agent: Option<String> = r.get("agent");
            let subject = contact_subject(&id);
            let own = labels.remove(&subject).unwrap_or_default();
            let held = links.remove(&id).unwrap_or_default();
            let by_agent = held.iter().any(|l| l.linked_how == DecidedHow::Agent);
            if own.is_empty() && agent.is_none() && !by_agent {
                continue;
            }
            out.push(ReviewItem {
                subject,
                contact_id: Some(id),
                name: Some(name),
                contact_agent: agent,
                handle: None,
                links: held,
                labels: own,
            });
        }
        let mut alone: Vec<(String, Vec<ReviewLabel>)> = labels
            .into_iter()
            .filter(|(s, _)| !s.starts_with("contact:"))
            .collect();
        alone.sort_by(|a, b| a.0.cmp(&b.0));
        for (handle, own) in alone {
            let contact_id = holders.get(&handle).cloned();
            out.push(ReviewItem {
                subject: handle.clone(),
                name: contact_id.as_ref().and_then(|id| names.get(id).cloned()),
                contact_id,
                contact_agent: None,
                handle: Some(handle),
                links: Vec::new(),
                labels: own,
            });
        }
        Ok(out)
    }

    /// Every link a person turned down, newest first: what an agent
    /// reads before it proposes.
    pub async fn turned_down(&self) -> Result<Vec<TurnedDown>> {
        let rows = sqlx::query(
            "SELECT handle, contact_id, contact_name, agent, reason, decided_at_utc, \
                    contact_rejected FROM not_linked ORDER BY decided_at_utc DESC, handle",
        )
        .fetch_all(&self.pool)
        .await
        .context("read the turned-down links")?;
        Ok(rows
            .iter()
            .map(|r| TurnedDown {
                handle: r.get("handle"),
                contact_id: r.get("contact_id"),
                contact_name: r.get("contact_name"),
                agent: r.get("agent"),
                reason: r.get("reason"),
                decided_at_utc: r.get("decided_at_utc"),
                contact_rejected: r.get("contact_rejected"),
            })
            .collect())
    }
}

async fn turned_down(
    conn: &mut sqlx::SqliteConnection,
    handle: &str,
    contact_id: &str,
) -> Result<bool> {
    let hit: Option<i64> =
        sqlx::query_scalar("SELECT 1 FROM not_linked WHERE handle = ? AND contact_id = ?")
            .bind(handle)
            .bind(contact_id)
            .fetch_optional(&mut *conn)
            .await
            .context("read whether a person turned a link down")?;
    Ok(hit.is_some())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store_path;

    fn email(s: &str) -> String {
        Handle::email(s).unwrap().as_str().to_string()
    }

    fn label(l: &str, why: &str) -> ProposedLabel {
        ProposedLabel {
            label: l.to_string(),
            applies: true,
            reason: Some(why.to_string()),
            confidence: None,
        }
    }

    /// A subject's labels as `label=in` or `label=out`, with `?` while an
    /// agent's waits for a person and `!` where a person overrode one.
    async fn labels_on(store: &Store, subject: &str) -> Vec<String> {
        let mut conn = store.pool.acquire().await.unwrap();
        labels_of(&mut conn, subject)
            .await
            .unwrap()
            .into_iter()
            .map(|r| {
                let l = ReviewLabel::from(r);
                let mark = if l.awaiting() {
                    "?"
                } else if l.overrode() {
                    "!"
                } else {
                    ""
                };
                format!("{}={}{mark}", l.label, if l.applies { "in" } else { "out" })
            })
            .collect()
    }

    #[test]
    fn an_agent_never_takes_a_handle_or_repeats_what_a_person_refused() {
        assert_eq!(plan_agent_link(None, false, "a"), AgentLink::Insert);
        assert_eq!(plan_agent_link(Some("a"), false, "a"), AgentLink::Already);
        assert!(matches!(
            plan_agent_link(Some("b"), false, "a"),
            AgentLink::Refused(_)
        ));
        assert!(matches!(
            plan_agent_link(None, true, "a"),
            AgentLink::Refused(_)
        ));
    }

    #[test]
    fn a_rejected_grouping_is_refused_whole_or_with_more_but_not_in_part() {
        let groupings = vec![(
            "Will Riker".to_string(),
            BTreeSet::from(["email:riker@e.org".to_string(), "slack:T/U".to_string()]),
        )];
        let set = |v: &[&'static str]| -> BTreeSet<&'static str> { v.iter().copied().collect() };
        assert_eq!(
            repeats_grouping(&set(&["email:riker@e.org", "slack:T/U"]), &groupings),
            Some("Will Riker")
        );
        assert_eq!(
            repeats_grouping(
                &set(&["email:riker@e.org", "slack:T/U", "tel:+1"]),
                &groupings
            ),
            Some("Will Riker")
        );
        assert_eq!(
            repeats_grouping(&set(&["email:riker@e.org"]), &groupings),
            None
        );
    }

    #[test]
    fn a_confidence_is_between_0_and_1() {
        assert_eq!(check_confidence(None), Ok(None));
        assert_eq!(check_confidence(Some(0.0)), Ok(Some(0.0)));
        assert_eq!(check_confidence(Some(1.0)), Ok(Some(1.0)));
        assert!(check_confidence(Some(1.5)).is_err());
        assert!(check_confidence(Some(f64::NAN)).is_err());
    }

    /// The whole loop on a real file: an agent proposes, a person
    /// confirms one contact, turns another down and overrides a label; a
    /// second pass by the agent leaves every one of those as the person
    /// left it.
    #[tokio::test]
    async fn an_agent_proposes_and_a_person_has_the_last_word() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&store_path(dir.path())).await.unwrap();
        let picard = store
            .create(
                "Jean-Luc Picard",
                ContactKind::Person,
                &[Handle::parse(&email("picard@enterprise.org")).unwrap()],
            )
            .await
            .unwrap();
        let ferengi = email("deals@ferengi.test");
        let quark = email("quark@ds9.test");
        let batch = Proposals {
            agent: "claude-test".into(),
            run: Some("run-1".into()),
            contacts: vec![
                ProposedContact {
                    contact_id: Some(picard.clone()),
                    handles: vec!["slack:T01/U_PICARD".into(), email("picard@enterprise.org")],
                    reason: Some("Slack profile lists picard@enterprise.org".into()),
                    labels: vec![label("Real", "wrote back 40 times")],
                    ..Default::default()
                },
                ProposedContact {
                    name: Some("Will Riker".into()),
                    handles: vec![email("riker@enterprise.org"), "slack:T01/U_RIKER".into()],
                    reason: Some("same name".into()),
                    confidence: Some(0.55),
                    labels: vec![label("real", "a few messages")],
                    ..Default::default()
                },
                ProposedContact {
                    name: Some("Nobody".into()),
                    handles: vec![email("picard@enterprise.org")],
                    ..Default::default()
                },
            ],
            handles: vec![
                ProposedHandle {
                    handle: ferengi.clone(),
                    labels: vec![ProposedLabel {
                        label: "real".into(),
                        applies: false,
                        reason: Some("one-way offers".into()),
                        confidence: Some(0.95),
                    }],
                },
                ProposedHandle {
                    handle: quark.clone(),
                    labels: vec![ProposedLabel {
                        label: "real".into(),
                        applies: true,
                        reason: Some("sounds like a pitch, but you wrote back".into()),
                        confidence: Some(0.6),
                    }],
                },
                ProposedHandle {
                    handle: "not a handle".into(),
                    labels: vec![],
                },
            ],
        };
        let got = store.propose(&batch).await.unwrap();
        assert_eq!((got.contacts_created, got.linked, got.labelled), (1, 3, 4));
        let skipped: Vec<&str> = got.skipped.iter().map(|s| s.what.as_str()).collect();
        assert_eq!(
            skipped,
            [
                "new contact \"Nobody\"",
                "email:picard@enterprise.org",
                "not a handle"
            ],
            "a contact with nothing to link is not made, and the rest of the batch lands"
        );

        let items = store.review().await.unwrap();
        let riker = items[1].subject.clone();
        let subjects: Vec<&str> = items.iter().map(|i| i.subject.as_str()).collect();
        assert_eq!(
            subjects,
            [
                contact_subject(&picard).as_str(),
                riker.as_str(),
                ferengi.as_str(),
                quark.as_str()
            ]
        );
        assert!(items.iter().all(ReviewItem::awaiting));
        assert!(
            !items[0].links[0].awaiting(),
            "the person's own link waits for nobody"
        );
        assert_eq!(items[1].contact_agent.as_deref(), Some("claude-test"));
        assert_eq!(items[0].holds("real"), Some(true));
        assert_eq!(items[2].holds("real"), Some(false));
        assert_eq!(items[2].holds("family"), None, "no row: nobody knows");
        assert_eq!(
            items[1].least_sure(),
            Some(0.55),
            "the join is the close call"
        );
        assert_eq!(items[1].links[0].run.as_deref(), Some("run-1"));

        assert_eq!(
            store
                .confirm(&[contact_subject(&picard), quark.clone()])
                .await
                .unwrap(),
            3,
            "one link and one label, and the close call"
        );
        assert_eq!(labels_on(&store, &quark).await, ["real=in"]);
        assert_eq!(store.reject(std::slice::from_ref(&riker)).await.unwrap(), 3);
        assert!(
            store
                .contact(riker.trim_start_matches("contact:"))
                .await
                .unwrap()
                .is_none(),
            "a contact only the agent wanted goes with its links"
        );
        assert!(store.set_label(&ferengi, "real", Some(true)).await.unwrap());
        assert_eq!(
            labels_on(&store, &ferengi).await,
            ["real=in!"],
            "the person's in, and the row still names the agent that said out"
        );
        assert!(!store.set_label(&ferengi, "real", Some(true)).await.unwrap());

        let turned: Vec<(String, String)> = store
            .turned_down()
            .await
            .unwrap()
            .into_iter()
            .map(|t| (t.handle, t.contact_name))
            .collect();
        assert_eq!(
            turned,
            [
                (
                    "email:riker@enterprise.org".to_string(),
                    "Will Riker".to_string()
                ),
                ("slack:T01/U_RIKER".to_string(), "Will Riker".to_string()),
            ]
        );

        let again = store.propose(&batch).await.unwrap();
        assert_eq!(
            (again.contacts_created, again.linked, again.labelled),
            (0, 0, 0),
            "nothing: the person has had their say on every part of the batch"
        );
        let why: Vec<(&str, &str)> = again
            .skipped
            .iter()
            .map(|s| (s.what.as_str(), s.why.as_str()))
            .collect();
        assert!(
            why.contains(&(
                format!("label real on {ferengi}").as_str(),
                "a person said otherwise"
            )),
            "{why:?}"
        );
        assert!(
            why.contains(&(
                "new contact \"Will Riker\"",
                "a person rejected these handles as one contact (\"Will Riker\")"
            )),
            "the rejected grouping is not proposed again: {why:?}"
        );
        assert_eq!(
            labels_on(&store, &contact_subject(&picard)).await,
            ["real=in"]
        );
        store.close().await;
    }

    /// In, out and unknown: a person can say a label does not apply
    /// before any agent asks, and can forget a label altogether.
    #[tokio::test]
    async fn a_label_is_in_out_or_unknown() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&store_path(dir.path())).await.unwrap();
        let worf = store
            .create("Worf", ContactKind::Person, &[])
            .await
            .unwrap();
        let subject = contact_subject(&worf);
        store
            .set_label(&subject, "family", Some(false))
            .await
            .unwrap();
        store.set_label(&subject, "real", Some(true)).await.unwrap();
        assert_eq!(labels_on(&store, &subject).await, ["family=out", "real=in"]);
        store
            .set_label(&subject, "real", Some(false))
            .await
            .unwrap();
        assert_eq!(
            labels_on(&store, &subject).await,
            ["family=out", "real=out"],
            "each label stands alone"
        );
        assert!(store.set_label(&subject, "family", None).await.unwrap());
        assert!(!store.set_label(&subject, "family", None).await.unwrap());
        let err = store
            .set_label("nobody", "x", Some(true))
            .await
            .unwrap_err();
        assert!(
            format!("{err:#}").contains("not a handle or a contact"),
            "{err:#}"
        );
        store.close().await;
    }

    /// A person's unlink of an agent's link is written down; their own
    /// link of the same pair later takes the turn-down back.
    #[tokio::test]
    async fn unlinking_an_agents_link_writes_it_down_until_a_person_relinks_it() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&store_path(dir.path())).await.unwrap();
        let data = store
            .create("Data", ContactKind::Person, &[])
            .await
            .unwrap();
        let lore = Handle::parse(&email("lore@enterprise.org")).unwrap();
        store
            .propose(&Proposals {
                agent: "claude-test".into(),
                run: None,
                contacts: vec![ProposedContact {
                    contact_id: Some(data.clone()),
                    handles: vec![lore.as_str().to_string()],
                    ..Default::default()
                }],
                handles: vec![],
            })
            .await
            .unwrap();
        assert!(store.unlink(&lore).await.unwrap());
        assert_eq!(store.turned_down().await.unwrap().len(), 1);
        store.link(&lore, &data).await.unwrap();
        assert!(store.turned_down().await.unwrap().is_empty());
        assert!(store.unlink(&lore).await.unwrap());
        assert!(
            store.turned_down().await.unwrap().is_empty(),
            "a person's own link is theirs to undo without a record"
        );
        store.close().await;
    }

    /// A merge carries the merged-away contact's labels and turn-downs to
    /// the survivor, the survivor's own answer winning where both have
    /// one; a delete takes the labels and keeps the record of what was
    /// turned down.
    #[tokio::test]
    async fn labels_and_turn_downs_follow_a_merge() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&store_path(dir.path())).await.unwrap();
        let keep = store
            .create("Worf", ContactKind::Person, &[])
            .await
            .unwrap();
        let dup = store
            .create("Worf", ContactKind::Person, &[])
            .await
            .unwrap();
        for (who, l, applies) in [
            (&keep, "real", true),
            (&dup, "real", false),
            (&dup, "klingon", true),
        ] {
            store
                .set_label(&contact_subject(who), l, Some(applies))
                .await
                .unwrap();
        }
        store
            .propose(&Proposals {
                agent: "claude-test".into(),
                run: None,
                contacts: vec![ProposedContact {
                    contact_id: Some(dup.clone()),
                    handles: vec![email("alexander@enterprise.org")],
                    ..Default::default()
                }],
                handles: vec![],
            })
            .await
            .unwrap();
        store.reject(&[contact_subject(&dup)]).await.unwrap();
        store.merge(&dup, &keep).await.unwrap();
        assert_eq!(
            labels_on(&store, &contact_subject(&keep)).await,
            ["klingon=in", "real=in"]
        );
        assert_eq!(store.turned_down().await.unwrap()[0].contact_id, keep);

        store.delete(&keep).await.unwrap();
        assert!(store.review().await.unwrap().is_empty());
        assert_eq!(
            store.turned_down().await.unwrap().len(),
            1,
            "the record stays"
        );
        store.close().await;
    }
}
