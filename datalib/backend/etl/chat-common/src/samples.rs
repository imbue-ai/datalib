//! A small corpus of chats that between them hit every layout
//! `render::render_markdown` can produce, plus [`write_samples`] to
//! put the rendered `.md` on disk.
//!
//! It exists so the markdown can be *looked at* — `bazel run
//! //datalib/ui:chat_preview` turns these files into HTML and PNGs
//! through the same markdown-it and the same stylesheet the app uses,
//! which is the only way to review a layout change without a data
//! root. `chat_samples_cover_every_layout` in `render.rs` keeps the
//! corpus honest.

use std::collections::HashMap;
use std::path::Path;

use anyhow::Result;
use datalib_etl::progress::Progress;
use datalib_schema::providers::Provider;
use datalib_time::WhenTsPrecision;

use crate::render::{render_all, RenderProfile, ENTITY_KIND_CONVERSATION};
use crate::types::{
    ItemKind, NormalizedAttachment, NormalizedChat, NormalizedChatItem, NormalizedDoc,
    NormalizedReaction,
};

/// 2369-04-15 08:30:00 UTC, the fixture corpus's era.
const T0: i64 = 12602794200000;

pub fn sample_profile() -> RenderProfile {
    RenderProfile {
        provider: Provider::Claude,
        source_label: "Claude".to_string(),
        chat_kind: "Chat".to_string(),
        message_kind: "Message".to_string(),
        reaction_kind: "Reaction".to_string(),
        chat_entity_kind: ENTITY_KIND_CONVERSATION,
        when_ts_precision: WhenTsPrecision::Seconds,
        render_version: 1,
    }
}

pub fn sample_chats() -> Vec<NormalizedChat> {
    vec![
        assistant_turn_with_tools(),
        group_chat(),
        long_message(),
        edge_cases(),
    ]
}

/// Render every sample into `out_dir` as a source named `samples`, and
/// return the `.md` paths in corpus order.
pub fn write_samples(out_dir: &Path) -> Result<Vec<String>> {
    let mut paths = Vec::new();
    render_all(
        &sample_profile(),
        &sample_chats(),
        out_dir,
        "samples",
        &HashMap::new(),
        &Progress::noop(),
        &HashMap::new(),
        &mut |doc| {
            paths.push(doc.md_path.to_string_lossy().into_owned());
            Ok(())
        },
    )?;
    Ok(paths)
}

fn text(uuid: &str, author: &str, at: i64, body: &str) -> NormalizedChatItem {
    NormalizedChatItem {
        message_uuid: uuid.to_string(),
        author_id: author.to_string(),
        author_display: author.to_string(),
        date_ms: Some(at),
        text: Some(body.to_string()),
        kind: ItemKind::Text,
        attachments: vec![],
        reactions: vec![],
        system_note: None,
        source_url: None,
        kind_label: None,
        source_ref: None,
        is_aside: false,
    }
}

fn aside(uuid: &str, at: i64, body: &str) -> NormalizedChatItem {
    NormalizedChatItem {
        kind_label: Some("Tool Call".to_string()),
        is_aside: true,
        ..text(uuid, "claude-opus-5", at, body)
    }
}

fn chat(id: &str, display: &str, items: Vec<NormalizedChatItem>) -> NormalizedChat {
    NormalizedChat {
        id: id.to_string(),
        chat_uuid: id.to_string(),
        display: display.to_string(),
        title: Some(display.to_string()),
        author: None,
        account: Some("acct-1701".to_string()),
        project: None,
        external_id: Some(format!("upstream-{id}")),
        upstream_scope: None,
        source_url: Some(format!("https://example.invalid/chat/{id}")),
        org_uuid: None,
        org_name: None,
        path_prefix: None,
        buckets: vec![NormalizedDoc {
            period_key: "all".to_string(),
            markdown_uuid: id.to_string(),
            items,
            orphan_reactions: Vec::new(),
        }],
    }
}

/// The complaint this whole layout change came from: an LLM turn whose
/// tool plumbing outweighs what was actually said.
fn assistant_turn_with_tools() -> NormalizedChat {
    chat(
        "sample-tool-run",
        "Sensor sweep of the Argolis cluster",
        vec![
            text(
                "s1-user",
                "Human",
                T0,
                "Can you check the long-range sensor logs for anything moving in the \
                 Argolis cluster, and summarize what you find?",
            ),
            aside(
                "s1-tool-1",
                T0 + 4000,
                "<details><summary>Tool use: sensor_query</summary>\n\n\
                 ```json\n{\n  \"band\": \"subspace\",\n  \"sector\": \"argolis\"\n}\n```\n\n\
                 </details>",
            ),
            aside(
                "s1-tool-2",
                T0 + 5000,
                "<details><summary>Tool result: sensor_query</summary>\n\n\
                 ```json\n{\n  \"contacts\": 3,\n  \"nearest_ly\": 0.4\n}\n```\n\n\
                 </details>",
            ),
            aside(
                "s1-tool-3",
                T0 + 6000,
                "<details><summary>Tool use: chart_lookup</summary>\n\n\
                 ```json\n{\n  \"catalogue\": \"federation\",\n  \"sector\": \"argolis\"\n}\n```\n\n\
                 </details>",
            ),
            aside(
                "s1-tool-4",
                T0 + 7000,
                "<details><summary>Tool result: chart_lookup</summary>\n\n\
                 ```json\n{\n  \"known_traffic\": \"none scheduled\"\n}\n```\n\n\
                 </details>",
            ),
            text(
                "s1-answer",
                "claude-opus-5",
                T0 + 9000,
                "Three contacts, the nearest 0.4 ly out and closing slowly. Nothing is \
                 scheduled through Argolis this quarter, so none of them is a Federation \
                 vessel on a filed course.\n\n\
                 | contact | bearing | range |\n\
                 |---|---|---|\n\
                 | A | 041 mark 3 | 0.4 ly |\n\
                 | B | 118 mark 0 | 1.9 ly |\n\
                 | C | 274 mark 7 | 3.1 ly |",
            ),
            aside(
                "s1-tool-5",
                T0 + 11000,
                "<details><summary>Tool use: log_note</summary>\n\n\
                 ```json\n{\n  \"note\": \"flagged for the bridge watch\"\n}\n```\n\n\
                 </details>",
            ),
        ],
    )
}

/// The ordinary case: several people, a reaction, an attachment, a
/// per-message linkout.
fn group_chat() -> NormalizedChat {
    let mut picard = text(
        "s2-picard",
        "Jean-Luc Picard",
        T0,
        "Senior staff, my ready room at 0900.",
    );
    picard.reactions = vec![
        NormalizedReaction {
            reaction_uuid: "s2-react-1".to_string(),
            reactor_display: "Will Riker".to_string(),
            emoji: "🫡".to_string(),
            date_ms: Some(T0 + 60000),
            source_ref: None,
        },
        NormalizedReaction {
            reaction_uuid: "s2-react-2".to_string(),
            reactor_display: "Deanna Troi".to_string(),
            emoji: "👍".to_string(),
            date_ms: Some(T0 + 90000),
            source_ref: None,
        },
    ];
    picard.source_url = Some("https://example.invalid/archive/s2-picard".to_string());

    let mut data = NormalizedChatItem {
        kind: ItemKind::Attachment,
        text: Some("The readout, as promised.".to_string()),
        attachments: vec![NormalizedAttachment {
            rel_path: None,
            file_name: Some("tricorder-readout.png".to_string()),
            mime_type: Some("image/png".to_string()),
            byte_len: Some(184_320),
            source_url: Some("https://example.invalid/blob/readout".to_string()),
            ref_id: None,
        }],
        ..text("s2-data", "Data", T0 + 3_600_000, "")
    };
    data.text = Some("The readout, as promised.".to_string());

    let mut joined = text("s2-system", "", T0 + 7_200_000, "");
    joined.kind = ItemKind::System;
    joined.text = None;
    joined.system_note = Some("Worf joined the channel".to_string());

    chat(
        "sample-group",
        "Bridge Crew",
        vec![
            picard,
            text(
                "s2-riker",
                "Will Riker",
                T0 + 1_800_000,
                "Understood. I'll have the duty roster with me.\n\n\
                 - Ops: Data\n- Tactical: Worf\n- Conn: unassigned",
            ),
            data,
            joined,
        ],
    )
}

/// A message far taller than the pane, with an ordinary short one on
/// either side. The case the clamp and the sticky jump controls exist
/// for: without them, one message is a scroll you have to ride out.
fn long_message() -> NormalizedChat {
    let mut body = String::from(
        "Full diagnostic dump from the port nacelle, as requested. \
         The interesting part is around line 60.\n",
    );
    for i in 1..=100 {
        body.push_str(&format!(
            "\n{i}. plasma conduit {i:03} — flow {flow:.2} kPa, ripple {ripple:.3}%{note}",
            flow = 40.0 + (i as f64 * 0.37) % 6.0,
            ripple = (i as f64 * 0.11) % 1.0,
            note = if i == 60 {
                "  ← ripple outside tolerance"
            } else {
                ""
            },
        ));
    }
    body.push_str("\n\nEnd of dump.");

    chat(
        "sample-long",
        "Nacelle diagnostic dump",
        vec![
            text(
                "s4-ask",
                "Geordi La Forge",
                T0,
                "Can you paste the whole port-nacelle conduit dump? I want to see \
                 every line, not a summary.",
            ),
            text("s4-dump", "Reginald Barclay", T0 + 60000, &body),
            text(
                "s4-reply",
                "Geordi La Forge",
                T0 + 120000,
                "Line 60 is the one. Thanks — I'll pull that conduit at the next \
                 maintenance window.",
            ),
        ],
    )
}

/// The shapes that have broken this renderer before: no timestamp at
/// all, an author whose name is markup, a fenced code block, a long
/// unbroken token.
fn edge_cases() -> NormalizedChat {
    let mut undated = text(
        "s3-undated",
        "Unknown sender",
        T0,
        "Who sent this, and when?",
    );
    undated.date_ms = None;

    chat(
        "sample-edges",
        "Edge cases",
        vec![
            undated,
            text(
                "s3-markup",
                "<script>alert(1)</script> & Co.",
                T0 + 1000,
                "An author name that is markup must not become markup.",
            ),
            text(
                "s3-code",
                "Geordi La Forge",
                T0 + 2000,
                "Here's the plasma-flow patch:\n\n\
                 ```rust\nfn realign(coil: &mut Coil) {\n    coil.phase += 0.5;\n}\n```",
            ),
            text(
                "s3-long",
                "Reginald Barclay",
                T0 + 3000,
                "The transporter buffer id is \
                 aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa \
                 and it should wrap rather than widen the column.",
            ),
        ],
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The corpus is only worth reviewing if it still contains one of
    /// everything, and it is easy to delete a sample without noticing
    /// which layout went with it.
    #[test]
    fn every_layout_is_represented() {
        let chats = sample_chats();
        let items: Vec<&NormalizedChatItem> = chats
            .iter()
            .flat_map(|c| c.buckets.iter().flat_map(|b| b.items.iter()))
            .collect();

        assert!(items.iter().any(|i| i.is_aside), "a tool step");
        assert!(
            items.iter().any(|i| matches!(i.kind, ItemKind::System)),
            "a system event",
        );
        assert!(
            items.iter().any(|i| matches!(i.kind, ItemKind::Attachment)),
            "an attachment",
        );
        assert!(items.iter().any(|i| !i.reactions.is_empty()), "a reaction");
        assert!(items.iter().any(|i| i.date_ms.is_none()), "an undated item");
        assert!(items.iter().any(|i| i.source_url.is_some()), "a linkout");
        assert!(
            items.iter().any(|i| i.author_display.contains('<')),
            "an author name that is markup",
        );
        assert!(
            items
                .iter()
                .any(|i| i.text.as_deref().is_some_and(|t| t.contains("```"))),
            "a fenced code block",
        );
        assert!(
            items
                .iter()
                .any(|i| i.text.as_deref().is_some_and(|t| t.contains("|---"))),
            "a table",
        );
        assert!(
            items
                .iter()
                .any(|i| i.text.as_deref().is_some_and(|t| t.lines().count() > 100)),
            "a message too tall for the pane",
        );
    }

    /// A run of asides has to be *adjacent* to become one `<details>`,
    /// and the sample that demonstrates it must contain both a run and
    /// a lone one.
    #[test]
    fn the_tool_sample_has_a_run_and_a_singleton() {
        let chat = assistant_turn_with_tools();
        let runs: Vec<usize> = chat.buckets[0]
            .items
            .chunk_by(|a, b| a.is_aside == b.is_aside)
            .filter(|c| c[0].is_aside)
            .map(<[NormalizedChatItem]>::len)
            .collect();
        assert_eq!(runs, vec![4, 1], "{runs:?}");
    }
}
