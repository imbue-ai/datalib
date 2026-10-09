//! Gemini's activity: each entry is the prompt, by the account, then
//! Gemini's response. The files a prompt attached ride on the prompt,
//! the images Gemini drew on the response.

use datalib_etl::blob_cas::CasEdgeRow;
use datalib_etl::bulk::BulkUpsertable;
use datalib_etl_chat_common::types::{NormalizedAttachment, NormalizedChat};
use datalib_etl_google_takeout::ingest::schema_raw::{GeminiActivityRow, GeminiAttachmentRow};
use datalib_etl_render::html::escape_md_block;
use datalib_etl_render::inputs::Inputs;
use serde_json::Value;

use crate::feeds::{self, Row};
use crate::ids;

/// The author of every response.
const GEMINI: &str = "Gemini";

pub fn build(source_id: &str, rows: &[Row]) -> Vec<NormalizedChat> {
    if rows.is_empty() {
        return Vec::new();
    }
    let inputs = Inputs::default();
    let mut items = Vec::with_capacity(rows.len() * 2);
    for row in rows {
        inputs.read(GeminiActivityRow::TABLE, &row.id);
        for id in attachment_ids(row) {
            inputs.read(GeminiAttachmentRow::TABLE, &id);
        }
        let mut problems = Vec::new();
        let date_ms = feeds::stamp_ms(row, &mut problems);

        let prompt = feeds::item(
            ids::feed_item(source_id, ids::KIND_GEMINI_PROMPT, &row.id, date_ms),
            feeds::ME,
            date_ms,
            "Gemini Prompt",
            problems,
        );
        let prompt_text = feeds::str_at(&row.payload, "/promptText").map(escape_md_block);
        let attached = files(&row.payload, "attachedFiles")
            .map(|(file, name)| attachment(&row.id, file, name))
            .collect();
        items.push(feeds::with_attachments(
            feeds::with_text(prompt, prompt_text),
            attached,
        ));

        let drawn: Vec<NormalizedAttachment> = generated_images(&row.payload)
            .map(|file| attachment(&row.id, file, file))
            .collect();
        let response_text = feeds::str_at(&row.payload, "/responseHtml")
            .map(response_markdown)
            .filter(|md| !md.trim().is_empty());
        if response_text.is_none() && drawn.is_empty() {
            continue;
        }
        let response = feeds::item(
            ids::feed_item(source_id, ids::KIND_GEMINI_RESPONSE, &row.id, date_ms),
            GEMINI,
            date_ms,
            "Gemini Response",
            Vec::new(),
        );
        items.push(feeds::with_attachments(
            feeds::with_text(response, response_text),
            drawn,
        ));
    }
    vec![feeds::yearly(
        source_id,
        feeds::GEMINI,
        "Gemini",
        items,
        inputs,
    )]
}

/// Every attachment edge an entry names, as the bundle and the inputs
/// key it: the files it attached and the images it drew.
pub fn attachment_ids(row: &Row) -> Vec<String> {
    files(&row.payload, "attachedFiles")
        .map(|(file, _)| file)
        .chain(generated_images(&row.payload))
        .map(|file| GeminiAttachmentRow::pk_recipe(&row.id, file))
        .collect()
}

/// `(file in the export, name it was uploaded under)` per attached file.
fn files<'a>(payload: &'a Value, key: &str) -> impl Iterator<Item = (&'a str, &'a str)> {
    payload
        .get(key)
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|f| {
            let file = f.get("file").and_then(Value::as_str)?;
            Some((file, f.get("name").and_then(Value::as_str).unwrap_or(file)))
        })
}

fn generated_images(payload: &Value) -> impl Iterator<Item = &str> {
    payload
        .get("generatedImages")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
}

fn attachment(activity_id: &str, file: &str, name: &str) -> NormalizedAttachment {
    feeds::attachment(
        GeminiAttachmentRow::pk_recipe(activity_id, file),
        name,
        file,
    )
}

/// The response's HTML as markdown. Its `<img>`s are the drawn images,
/// which render as the response's attachments instead.
fn response_markdown(html: &str) -> String {
    htmd::HtmlToMarkdown::builder()
        .skip_tags(vec!["script", "style", "img"])
        .build()
        .convert(html)
        .unwrap_or_default()
        .trim()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use datalib_etl_chat_common::types::ItemKind;
    use serde_json::json;

    fn row(id: &str, when: &str, payload: Value) -> Row {
        Row {
            id: id.to_string(),
            payload,
            when: Some(when.to_string()),
        }
    }

    #[test]
    fn an_entry_is_a_prompt_then_its_response() {
        let rows = [
            row(
                "a1",
                "2364-02-14T11:48:37-08:00",
                json!({
                    "promptText": "Tell me about the *Prime Directive*.",
                    "responseHtml": "<p>It is <strong>General Order 1</strong>.</p>\n<ul>\n<li>No interference.</li>\n</ul>",
                    "attachedFiles": [{"file": "Prime Directive-17.txt", "name": "Prime Directive.txt"}],
                    "generatedImages": [],
                }),
            ),
            row(
                "a2",
                "2365-03-03T09:15:00-08:00",
                json!({
                    "promptText": "Draw the Enterprise-D.",
                    "responseHtml": "<p><img alt=\"\" src=\"d.jpeg\"></p>",
                    "attachedFiles": [],
                    "generatedImages": ["d.jpeg"],
                }),
            ),
        ];
        let chats = build("gt", &rows);
        let [chat] = chats.as_slice() else {
            panic!("one feed: {chats:?}")
        };
        let years: Vec<&str> = chat.buckets.iter().map(|b| b.period_key.as_str()).collect();
        assert_eq!(years, ["2364", "2365"]);

        let [prompt, response] = chat.buckets[0].items.as_slice() else {
            panic!("{:?}", chat.buckets[0].items)
        };
        assert_eq!(prompt.author_display, "Me");
        assert_eq!(prompt.kind_label.as_deref(), Some("Gemini Prompt"));
        assert_eq!(
            prompt.attachments[0].file_name.as_deref(),
            Some("Prime Directive.txt")
        );
        assert_eq!(
            prompt.attachments[0].ref_id,
            Some(GeminiAttachmentRow::pk_recipe(
                "a1",
                "Prime Directive-17.txt"
            ))
        );
        assert_eq!(response.author_display, "Gemini");
        let md = response.text.as_deref().unwrap();
        assert!(md.starts_with("It is **General Order 1**."), "{md}");
        assert!(md.contains("No interference."), "{md}");

        let [_, drawn] = chat.buckets[1].items.as_slice() else {
            panic!("{:?}", chat.buckets[1].items)
        };
        assert_eq!(drawn.text, None, "the image is the response");
        assert_eq!(drawn.kind, ItemKind::Attachment);
        assert_eq!(
            drawn.attachments[0].mime_type.as_deref(),
            Some("image/jpeg")
        );
        // Two entries and the three files they name.
        assert_eq!(chat.inputs.len(), 4);
    }

    /// The prompt is what a person typed; the profile is markdown, so
    /// the feed escapes it.
    #[test]
    fn a_prompt_in_markup_renders_escaped() {
        let rows = [row(
            "a1",
            "2364-02-14T11:48:37-08:00",
            json!({"promptText": "<script>x</script> [link](javascript:x)", "responseHtml": ""}),
        )];
        let chats = build("gt", &rows);
        let items = &chats[0].buckets[0].items;
        assert_eq!(items.len(), 1, "no response, no response item");
        let text = items[0].text.as_deref().unwrap();
        assert!(!text.contains("<script>"), "{text}");
        assert!(text.contains(r"\[link\](javascript"), "not a link: {text}");
    }
}
