//! One document per meeting, through chat-common: a first section with
//! when, who hosted, who came and what Zoom keeps of the recording, then
//! the AI Companion's summary.

use std::collections::BTreeMap;

use datalib_etl_chat_common::normalize::iso_to_ms;
use datalib_etl_chat_common::render::RenderProfile;
use datalib_etl_chat_common::types::{
    own_stamp_ms, AuthorKind, ItemKind, NormalizedChat, NormalizedChatItem, NormalizedDoc,
    UpstreamRef,
};
use datalib_etl_chat_common::TextFormat;
use datalib_etl_render::html::{escape_md_block, escape_md_inline};
use datalib_etl_render::inputs::Inputs;
use datalib_handle::Handle;
use datalib_schema::problems::Problem;
use datalib_schema::providers::Provider;
use serde_json::Value;

use crate::ids;
use crate::store::{Meeting, TABLES};

/// v1: details and summary.
pub const RENDER_VERSION: u32 = 1;

pub const SOURCE_LABEL: &str = "Zoom";
const KIND_DETAILS: &str = "Zoom Meeting Details";
const KIND_SUMMARY: &str = "Zoom Meeting Summary";

pub fn profile() -> RenderProfile {
    RenderProfile {
        stamp_precision: ids::STAMP_PRECISION,
        provider: Provider::Zoom,
        source_label: SOURCE_LABEL.to_string(),
        chat_kind: "Zoom Meeting".to_string(),
        message_kind: KIND_DETAILS.to_string(),
        reaction_kind: "Zoom Reaction".to_string(),
        chat_entity_kind: ids::KIND_MEETING,
        render_version: RENDER_VERSION,
        // The summary is Zoom's markdown; everything else this file
        // writes is escaped where it is put in.
        text_format: TextFormat::Markdown,
    }
}

/// What one meeting renders from besides its rows.
pub struct MeetingInput<'a> {
    pub meeting: &'a Meeting,
    /// The email of the user the store copies, when this meeting is
    /// theirs: whose mirror it is.
    pub account: Option<String>,
}

/// Every row a meeting's document reads, for its bucket's inputs.
pub fn declare_inputs(m: &Meeting, inputs: &Inputs) {
    for table in TABLES {
        inputs.read(table, &m.uuid);
    }
}

pub fn build_chat(source_id: &str, input: MeetingInput<'_>, inputs: &Inputs) -> NormalizedChat {
    let m = input.meeting;
    declare_inputs(m, inputs);
    let chat_id = ids::meeting(source_id, &m.uuid);
    let topic = m
        .str_field("topic")
        .unwrap_or("(untitled meeting)")
        .to_string();
    let mut problems = Vec::new();
    let start_ms = own_stamp_ms(
        m.str_field("start_time"),
        "start_time",
        iso_to_ms,
        &mut problems,
    );
    let people = attendees(m);

    let mut items = vec![details_item(source_id, m, start_ms, &people, problems)];
    if let Some(summary) = summary_item(source_id, m, start_ms) {
        items.push(summary);
    }

    NormalizedChat {
        named_after: Vec::new(),
        contacts: Vec::new(),
        inputs: inputs.declared(),
        path_prefix: None,
        id: m.uuid.clone(),
        chat_uuid: chat_id.uuid.clone(),
        display: topic.clone(),
        title: Some(topic),
        account: input.account,
        author: None,
        project: None,
        external_id: Some(m.uuid.clone()),
        upstream_account: None,
        source_url: m.str_field("share_url").map(str::to_string),
        org_uuid: None,
        org_name: None,
        buckets: vec![NormalizedDoc {
            orphan_reactions: Vec::new(),
            period_key: "all".to_string(),
            markdown_uuid: chat_id.uuid,
            source_ref: None,
            items,
        }],
    }
}

/// One person who attended, with every time they joined added up: Zoom
/// lists a participant again each time they rejoin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attendee {
    pub name: String,
    pub handle: Option<Handle>,
    pub seconds: i64,
}

pub fn attendees(m: &Meeting) -> Vec<Attendee> {
    let list = m
        .participants
        .as_ref()
        .and_then(|p| p.get("participants"))
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    let mut by_who: BTreeMap<(String, String), (usize, i64)> = BTreeMap::new();
    for (i, p) in list.iter().enumerate() {
        let s = |k: &str| {
            p.get(k)
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim()
                .to_string()
        };
        let (name, email) = (s("name"), s("user_email").to_ascii_lowercase());
        if name.is_empty() && email.is_empty() {
            continue;
        }
        let seconds = p.get("duration").and_then(Value::as_i64).unwrap_or(0);
        let slot = by_who.entry((name, email)).or_insert((i, 0));
        slot.1 += seconds;
    }
    let mut out: Vec<(usize, Attendee)> = by_who
        .into_iter()
        .map(|((name, email), (first, seconds))| {
            let attendee = Attendee {
                name: if name.is_empty() { email.clone() } else { name },
                handle: Handle::email(&email),
                seconds,
            };
            (first, attendee)
        })
        .collect();
    out.sort_by_key(|(first, _)| *first);
    out.into_iter().map(|(_, a)| a).collect()
}

fn link(name: &str, handle: Option<&Handle>) -> String {
    match handle {
        Some(h) => format!("[{}]({})", escape_md_inline(name), h.to_uri()),
        None => escape_md_inline(name),
    }
}

fn minutes(n: i64) -> String {
    if n == 1 {
        "1 minute".to_string()
    } else {
        format!("{n} minutes")
    }
}

fn details_item(
    source_id: &str,
    m: &Meeting,
    start_ms: Option<i64>,
    people: &[Attendee],
    problems: Vec<Problem>,
) -> NormalizedChatItem {
    let id = ids::details(source_id, &m.uuid, start_ms);
    let mut lines = Vec::new();
    // A recording names its host by id only; the report names them.
    if let Some(host) = m
        .str_field("host_name")
        .or_else(|| m.str_field("user_name"))
    {
        lines.push(format!("- **Host:** {}", escape_md_inline(host)));
    }
    if let Some(d) = m.field("duration").and_then(Value::as_i64) {
        lines.push(format!("- **Length:** {}", minutes(d)));
    }
    if let Some(n) = m.field("id").and_then(Value::as_i64) {
        lines.push(format!("- **Meeting ID:** {n}"));
    }
    let kept = kept_in_zoom(m);
    if !kept.is_empty() {
        lines.push(format!(
            "- **Zoom keeps:** {}",
            escape_md_inline(&kept.join(", "))
        ));
    }
    let mut text = lines.join("\n");
    if !people.is_empty() {
        let who: Vec<String> = people
            .iter()
            .map(|a| {
                let mins = (a.seconds + 59) / 60;
                format!("- {} ({})", link(&a.name, a.handle.as_ref()), minutes(mins))
            })
            .collect();
        text.push_str(&format!("\n\n**Who attended**\n\n{}", who.join("\n")));
    }
    if let Some(why) = m
        .participants
        .as_ref()
        .and_then(|p| p.get("unavailable"))
        .and_then(Value::as_str)
    {
        text.push_str(&format!(
            "\n\n*Zoom would not say who attended:* {}",
            escape_md_inline(why)
        ));
    }
    NormalizedChatItem {
        message_uuid: id.uuid.clone(),
        recipients: Vec::new(),
        mentions: Vec::new(),
        author_handle: None,
        author_display: "Meeting".to_string(),
        author_kind: AuthorKind::Unknown,
        date_ms: start_ms,
        text: (!text.is_empty()).then_some(text),
        kind: ItemKind::Text,
        attachments: Vec::new(),
        reactions: Vec::new(),
        labels: Vec::new(),
        system_note: None,
        source_url: None,
        kind_label: Some(KIND_DETAILS.to_string()),
        source_ref: Some(UpstreamRef::new(id.entity_kind, id.natural_key.clone())),
        is_aside: false,
        branch: Vec::new(),
        unread: false,
        problems,
    }
}

/// What the recording's files are, as Zoom names their kinds
/// (`audio_transcript`, `shared_screen_with_speaker_view`), each once.
/// None of them is downloaded: INGEST.md says why.
pub fn kept_in_zoom(m: &Meeting) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for f in m.recording_files() {
        let kind = f
            .get("recording_type")
            .or_else(|| f.get("file_type"))
            .and_then(Value::as_str)
            .map(|k| k.to_ascii_lowercase().replace('_', " "));
        if let Some(kind) = kind.filter(|k| !out.contains(k)) {
            out.push(kind);
        }
    }
    out
}

/// The summary as markdown: Zoom's own `summary_content`, else the
/// older fields put together, escaped, since they are plain text.
pub fn summary_markdown(summary: &Value) -> Option<String> {
    let s = |k: &str| {
        summary
            .get(k)
            .and_then(Value::as_str)
            .filter(|v| !v.trim().is_empty())
    };
    if let Some(md) = s("summary_content") {
        return Some(md.trim().to_string());
    }
    let mut parts = Vec::new();
    if let Some(overview) = s("summary_overview") {
        parts.push(escape_md_block(overview.trim()));
    }
    for d in summary
        .get("summary_details")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let label = d.get("label").and_then(Value::as_str).unwrap_or("");
        let body = d.get("summary").and_then(Value::as_str).unwrap_or("");
        if !body.trim().is_empty() {
            parts.push(format!(
                "**{}**\n\n{}",
                escape_md_inline(label.trim()),
                escape_md_block(body.trim())
            ));
        }
    }
    let steps: Vec<String> = summary
        .get("next_steps")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(|step| format!("- {}", escape_md_inline(step.trim())))
        .collect();
    if !steps.is_empty() {
        parts.push(format!("**Next steps**\n\n{}", steps.join("\n")));
    }
    (!parts.is_empty()).then(|| parts.join("\n\n"))
}

fn summary_item(source_id: &str, m: &Meeting, start_ms: Option<i64>) -> Option<NormalizedChatItem> {
    let summary = m.summary.as_ref()?;
    let text = summary_markdown(summary)?;
    let mut problems = Vec::new();
    let created = summary.get("summary_created_time").and_then(Value::as_str);
    let at = own_stamp_ms(created, "summary_created_time", iso_to_ms, &mut problems).or(start_ms);
    let id = ids::summary(source_id, &m.uuid, at);
    Some(NormalizedChatItem {
        message_uuid: id.uuid.clone(),
        recipients: Vec::new(),
        mentions: Vec::new(),
        author_handle: None,
        author_display: "Zoom AI Companion".to_string(),
        author_kind: AuthorKind::Named,
        date_ms: at,
        text: Some(text),
        kind: ItemKind::Text,
        attachments: Vec::new(),
        reactions: Vec::new(),
        labels: Vec::new(),
        system_note: None,
        source_url: summary
            .get("summary_doc_url")
            .and_then(Value::as_str)
            .map(str::to_string),
        kind_label: Some(KIND_SUMMARY.to_string()),
        source_ref: Some(UpstreamRef::new(id.entity_kind, id.natural_key.clone())),
        is_aside: false,
        branch: Vec::new(),
        unread: false,
        problems,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn meeting() -> Meeting {
        Meeting {
            uuid: "nCC1701+D/briefing==".to_string(),
            recording: Some(json!({
                "uuid": "nCC1701+D/briefing==", "id": 85017010, "topic": "Senior staff briefing",
                "start_time": "2026-10-05T09:00:00Z", "duration": 45,
                "recording_files": [
                    {"id": "a-mp4", "file_type": "MP4", "recording_type": "shared_screen_with_speaker_view"},
                    {"id": "a-m4a", "file_type": "M4A", "recording_type": "audio_only"},
                    {"id": "a-vtt", "file_type": "TRANSCRIPT", "recording_type": "audio_transcript"},
                    {"file_type": "TIMELINE"},
                    {"file_type": "TIMELINE"}]})),
            participants: Some(json!({"participants": [
                {"name": "Jean-Luc Picard", "user_email": "picard@enterprise.example", "duration": 1500},
                {"name": "William Riker", "user_email": "riker@enterprise.example", "duration": 600},
                {"name": "Jean-Luc Picard", "user_email": "Picard@Enterprise.example", "duration": 1199},
                {"name": "Guinan", "user_email": "", "duration": 60}]})),
            ..Meeting::default()
        }
    }

    #[test]
    fn a_participant_who_rejoined_is_listed_once_with_their_whole_time() {
        let people = attendees(&meeting());
        let got: Vec<(&str, i64, bool)> = people
            .iter()
            .map(|a| (a.name.as_str(), a.seconds, a.handle.is_some()))
            .collect();
        assert_eq!(
            got,
            [
                ("Jean-Luc Picard", 2699, true),
                ("William Riker", 600, true),
                ("Guinan", 60, false)
            ]
        );
    }

    #[test]
    fn a_summary_without_markdown_is_assembled_and_escaped() {
        let md = summary_markdown(&json!({
            "summary_overview": "The crew discussed <the anomaly>.",
            "summary_details": [{"label": "Engineering", "summary": "Warp core *offline* for an hour."}],
            "next_steps": ["La Forge: realign the dilithium matrix"]
        }))
        .unwrap();
        assert!(md.contains("&lt;the anomaly&gt;"), "{md}");
        assert!(md.contains("**Engineering**"), "{md}");
        assert!(md.contains("Warp core *offline* for an hour."), "{md}");
        assert!(md.contains("**Next steps**\n\n- La Forge"), "{md}");
        assert_eq!(
            summary_markdown(&json!({"summary_content": "## Key takeaways\n- Engage."})).as_deref(),
            Some("## Key takeaways\n- Engage.")
        );
        assert_eq!(summary_markdown(&json!({"unavailable": "trashed"})), None);
    }

    #[test]
    fn the_details_list_who_came_and_what_zoom_would_not_say() {
        let mut m = meeting();
        let item = details_item("src", &m, Some(0), &attendees(&m), Vec::new());
        let text = item.text.unwrap();
        assert!(text.contains("- **Length:** 45 minutes"), "{text}");
        assert!(
            text.contains(
                "- **Zoom keeps:** shared screen with speaker view, audio only, \
                 audio transcript, timeline\n"
            ),
            "each kind once: {text}"
        );
        assert!(
            text.contains("- [Jean-Luc Picard](mailto:picard@enterprise.example) (45 minutes)"),
            "{text}"
        );
        m.participants = Some(json!({"participants": [], "unavailable": "too old"}));
        let item = details_item("src", &m, Some(0), &attendees(&m), Vec::new());
        assert!(item
            .text
            .unwrap()
            .contains("Zoom would not say who attended"));
    }
}
