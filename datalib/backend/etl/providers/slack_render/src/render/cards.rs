//! What a Slack message shows besides its `text` and `files`: its layout
//! `blocks` (a bot's sections, headers and context lines) and its
//! `attachments` (a forwarded message, a link preview, an integration's
//! card), drawn as markdown. TRANSLATE.md §"Blocks and attachments" has
//! the rules.

use datalib_etl_render::html::{escape_md_inline, escape_name_inline, md_link_dest};
use datalib_etl_render::message::chip_link;
use datalib_handle::Handle;
use datalib_html_entities::decode;
use serde_json::Value;

use super::emoji::emojize;
use super::mrkdwn::{to_commonmark, Labels};

/// A link preview's description longer than this is left out: Slack cuts
/// it short too, and the page it describes is a click away.
const PREVIEW_TEXT_MAX: usize = 300;

/// What a message's blocks and attachments draw, and what they hold that
/// nothing draws.
#[derive(Debug, Default, PartialEq)]
pub struct Drawn {
    /// The blocks' markdown, rich text left out: that repeats `text`.
    pub blocks: String,
    /// Whether the blocks are what Slack shows in place of `text`, which
    /// is then only the notification's summary of them.
    pub replaces_text: bool,
    /// Where the blocks replace `text`, the first text they show, as
    /// Slack wrote it: what a thread's title reads in its place.
    pub headline: Option<String>,
    /// Each attachment's markdown, in order.
    pub attachments: String,
    /// One line per part nothing draws, such as `call block`.
    pub not_shown: Vec<String>,
}

impl Drawn {
    /// The message's body: its text, unless the blocks replace it, then
    /// the blocks, then the attachments.
    pub fn body(&self, text: String) -> String {
        let text = if self.replaces_text {
            String::new()
        } else {
            text
        };
        [text.as_str(), &self.blocks, &self.attachments]
            .into_iter()
            .map(str::trim_end)
            .filter(|part| !part.trim().is_empty())
            .collect::<Vec<_>>()
            .join("\n\n")
    }
}

pub fn draw(raw: &Value, labels: Labels<'_>) -> Drawn {
    let list = |key| raw.get(key).and_then(Value::as_array).map(Vec::as_slice);
    let mut not_shown = Vec::new();
    let blocks = list("blocks").unwrap_or_default();
    let blocks_md = draw_blocks(blocks, labels, &mut not_shown);
    let has_rich_text = blocks.iter().any(|b| block_type(b) == Some("rich_text"));
    // Slack shows the blocks and keeps `text` for the notification. Where
    // a block cannot be drawn, `text` is the only account of it left.
    let replaces_text = !blocks_md.is_empty() && !has_rich_text && not_shown.is_empty();
    let headline = replaces_text
        .then(|| blocks.iter().find_map(first_text))
        .flatten()
        .map(str::to_string);
    let attachments = list("attachments")
        .unwrap_or_default()
        .iter()
        .filter_map(|a| draw_attachment(a, labels, &mut not_shown))
        .collect::<Vec<_>>()
        .join("\n\n");
    Drawn {
        blocks: blocks_md,
        replaces_text,
        headline,
        attachments,
        not_shown,
    }
}

/// The first text a header or section block shows.
fn first_text(b: &Value) -> Option<&str> {
    if !matches!(block_type(b), Some("header" | "section")) {
        return None;
    }
    b.get("text").and_then(|t| str_of(t, "text")).or_else(|| {
        b.get("fields")
            .and_then(Value::as_array)?
            .iter()
            .find_map(|f| str_of(f, "text"))
    })
}

fn block_type(b: &Value) -> Option<&str> {
    b.get("type").and_then(Value::as_str)
}

fn str_of<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

fn draw_blocks(blocks: &[Value], labels: Labels<'_>, not_shown: &mut Vec<String>) -> String {
    blocks
        .iter()
        .filter_map(|b| match draw_block(b, labels) {
            Ok(md) => md.filter(|md| !md.trim().is_empty()),
            Err(kind) => {
                not_shown.push(format!("{kind} block"));
                None
            }
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// `Ok(None)` for a block that says nothing to draw; `Err` with its type
/// for one that says something this cannot draw.
fn draw_block(b: &Value, labels: Labels<'_>) -> Result<Option<String>, String> {
    let Some(kind) = block_type(b) else {
        return Err("untyped".to_string());
    };
    let md = match kind {
        "rich_text" | "divider" | "actions" => return Ok(None),
        "header" => b
            .get("text")
            .and_then(|t| text_object(t, labels))
            .map(|t| format!("**{t}**")),
        "section" => {
            let text = b.get("text").and_then(|t| text_object(t, labels));
            let fields = b
                .get("fields")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|f| text_object(f, labels))
                .map(|f| list_item(&f))
                .collect::<Vec<_>>();
            let fields = (!fields.is_empty()).then(|| fields.join("\n"));
            let parts: Vec<String> = text.into_iter().chain(fields).collect();
            (!parts.is_empty()).then(|| parts.join("\n\n"))
        }
        "context" => {
            let texts: Vec<String> = b
                .get("elements")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|e| text_object(e, labels))
                .collect();
            (!texts.is_empty()).then(|| texts.join(" · "))
        }
        "image" => {
            let label = str_of(b, "title")
                .or_else(|| b.get("title").and_then(|t| str_of(t, "text")))
                .or_else(|| str_of(b, "alt_text"))
                .unwrap_or("image");
            let shown = match str_of(b, "image_url").and_then(safe_url) {
                Some(url) => link(&plain(label), url),
                None => plain(label),
            };
            Some(format!("Image: {shown}"))
        }
        "video" => {
            let title = b
                .get("title")
                .and_then(|t| str_of(t, "text"))
                .unwrap_or("video");
            let url = str_of(b, "title_url")
                .or_else(|| str_of(b, "video_url"))
                .and_then(safe_url);
            let shown = match url {
                Some(url) => link(&plain(title), url),
                None => plain(title),
            };
            let by = str_of(b, "provider_name").map(|p| format!(" ({})", plain(p)));
            Some(format!("Video: {shown}{}", by.unwrap_or_default()))
        }
        other => return Err(other.to_string()),
    };
    Ok(md)
}

/// A block's text object: `mrkdwn` through the same conversion as a
/// message's text, `plain_text` as typed.
fn text_object(t: &Value, labels: Labels<'_>) -> Option<String> {
    let text = str_of(t, "text")?;
    let md = match t.get("type").and_then(Value::as_str) {
        Some("plain_text") => plain(text),
        _ => to_commonmark(text, labels),
    };
    (!md.trim().is_empty()).then_some(md)
}

/// Plain text Slack wrote, entities and all, on markdown lines.
fn plain(text: &str) -> String {
    decode(text)
        .lines()
        .map(|line| escape_md_inline(&emojize(line)))
        .collect::<Vec<_>>()
        .join("\n")
}

/// `md` as one item of a bullet list, its later lines inside the item.
fn list_item(md: &str) -> String {
    format!("- {}", md.replace('\n', "\n  "))
}

/// Only a web or mail address becomes a link; anything else upstream
/// names is left as text.
fn safe_url(url: &str) -> Option<&str> {
    let lower = url.get(..8).unwrap_or(url).to_ascii_lowercase();
    ["https://", "http://", "mailto:"]
        .iter()
        .any(|scheme| lower.starts_with(scheme))
        .then_some(url)
}

/// `[label](url)` where `label` is markdown already.
fn link(label: &str, url: &str) -> String {
    format!("[{label}]({})", md_link_dest(url))
}

/// One attachment: its pretext, then the rest quoted, as Slack sets an
/// attachment off with a bar. `None` once it is noted in `not_shown`.
fn draw_attachment(a: &Value, labels: Labels<'_>, not_shown: &mut Vec<String>) -> Option<String> {
    let flag = |key| a.get(key).and_then(Value::as_bool).unwrap_or(false);
    let forwarded = flag("is_share") || flag("is_msg_unfurl");
    let has = |key| a.get(key).is_some_and(|v| !v.is_null());
    let preview = !forwarded
        && !flag("is_app_unfurl")
        && str_of(a, "from_url").is_some()
        && !has("fields")
        && !has("blocks");
    let original = str_of(a, "from_url")
        .or_else(|| str_of(a, "original_url"))
        .and_then(safe_url);
    let mut parts: Vec<String> = Vec::new();

    let mut head: Vec<String> = Vec::new();
    if let Some(service) = str_of(a, "service_name") {
        head.push(plain(service));
    }
    if let Some(author) = author(a, labels) {
        head.push(author);
    }
    let title = str_of(a, "title").map(|t| to_commonmark(t, labels));
    let title_url = str_of(a, "title_link").and_then(safe_url);
    match (title, title_url, forwarded) {
        (Some(t), Some(url), _) => head.push(format!("**{}**", link(&t, url))),
        (Some(t), None, _) => head.push(format!("**{t}**")),
        (None, _, true) => {
            if let Some(url) = original {
                head.push(link("original message", url));
            }
        }
        (None, _, false) => {
            if let Some(url) = original {
                head.push(format!("<{}>", url.replace('<', "%3C").replace('>', "%3E")));
            }
        }
    }
    if !head.is_empty() {
        parts.push(head.join(" · "));
    }

    let text = str_of(a, "text").filter(|t| !preview || t.chars().count() <= PREVIEW_TEXT_MAX);
    if let Some(text) = text {
        parts.push(to_commonmark(text, labels));
    }
    let fields: Vec<String> = a
        .get("fields")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|f| field(f, labels))
        .collect();
    if !fields.is_empty() {
        parts.push(fields.join("\n"));
    }
    let blocks = a
        .get("blocks")
        .and_then(Value::as_array)
        .map(|b| draw_blocks(b, labels, not_shown))
        .unwrap_or_default();
    if !blocks.is_empty() {
        parts.push(blocks);
    }
    let files: Vec<String> = a
        .get("files")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|f| str_of(f, "title").or_else(|| str_of(f, "name")))
        .map(|name| format!("File: {}", escape_name_inline(name)))
        .collect();
    if !files.is_empty() {
        parts.push(files.join("\n"));
    }

    let drew_something = parts.iter().any(|p| !p.trim().is_empty());
    if !drew_something {
        match str_of(a, "fallback") {
            Some(fallback) => parts.push(to_commonmark(fallback, labels)),
            None => {
                not_shown.push("attachment with nothing to show".to_string());
                return None;
            }
        }
    }
    let quoted = quote(&parts.join("\n\n"));
    let pretext = str_of(a, "pretext").map(|p| to_commonmark(p, labels));
    Some(match pretext {
        Some(p) => format!("{p}\n\n{quoted}"),
        None => quoted,
    })
}

/// Who wrote what an attachment carries: a person the viewer can resolve
/// when Slack names their user id, else the name Slack showed.
fn author(a: &Value, labels: Labels<'_>) -> Option<String> {
    let name = str_of(a, "author_name").or_else(|| str_of(a, "author_subname"));
    let user = str_of(a, "author_id").filter(|id| id.starts_with(['U', 'W']));
    // A message forwarded from another workspace names its author there.
    let team = str_of(a, "channel_team").unwrap_or(labels.team_id);
    match user.and_then(|u| Handle::slack(team, u).map(|h| (u, h))) {
        Some((user, handle)) => {
            let shown = name
                .map(|n| decode(n).into_owned())
                .or_else(|| labels.users.get(user).cloned())
                .unwrap_or_else(|| user.to_string());
            Some(chip_link(&emojize(&shown), &handle))
        }
        None => name.map(plain),
    }
}

/// A card's field, `title: value`, as one list item.
fn field(f: &Value, labels: Labels<'_>) -> Option<String> {
    let title = str_of(f, "title").map(plain);
    let value = str_of(f, "value").map(|v| to_commonmark(v, labels));
    let md = match (title, value) {
        (Some(t), Some(v)) => format!("**{t}:** {v}"),
        (Some(t), None) => format!("**{t}**"),
        (None, Some(v)) => v,
        (None, None) => return None,
    };
    Some(list_item(&md))
}

/// `md` as a blockquote, every line of it, so a blank line inside does
/// not end the quote.
fn quote(md: &str) -> String {
    md.lines()
        .map(|line| {
            if line.is_empty() {
                ">".to_string()
            } else {
                format!("> {line}")
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::LazyLock;

    use datalib_etl_render::inputs::Inputs;
    use serde_json::json;

    use super::*;

    static USERS: LazyLock<BTreeMap<String, String>> = LazyLock::new(|| {
        BTreeMap::from([
            ("U_DATA".to_string(), "Lt. Cmdr. Data".to_string()),
            ("U_WORF".to_string(), "Worf".to_string()),
        ])
    });
    static NONE: LazyLock<BTreeMap<String, String>> = LazyLock::new(BTreeMap::new);
    static INPUTS: LazyLock<Inputs> = LazyLock::new(Inputs::default);

    fn labels() -> Labels<'static> {
        Labels {
            users: INPUTS.lookup("users", &USERS),
            channels: INPUTS.lookup("channels", &NONE),
            team_id: "T01",
        }
    }

    fn data_chip() -> &'static str {
        "[@Lt. Cmdr. Data](slack://user?team=T01&id=U_DATA \"@Lt. Cmdr. Data (slack:T01/U_DATA)\")"
    }

    /// A forward once left its message blank: the quote names who wrote
    /// it, as a person, says what they wrote and links the original.
    #[test]
    fn a_forward_quotes_its_author_text_and_original() {
        let drawn = draw(
            &json!({"attachments": [{
                "is_share": true,
                "author_id": "U_WORF",
                "author_name": "Worf",
                "author_subname": "worf",
                "channel_team": "T01",
                "text": "I recommend *raising shields*, <@U_DATA>.\n\nNow.",
                "from_url": "https://enterprise-d.slack.example/archives/C_BRIDGE/p1",
                "files": [{"name": "shields.png", "title": "Shield grid"}],
            }]}),
            labels(),
        );
        assert_eq!(
            drawn.attachments,
            format!(
                "> [Worf](slack://user?team=T01&id=U_WORF \"Worf (slack:T01/U_WORF)\") · \
                 [original message](https://enterprise-d.slack.example/archives/C_BRIDGE/p1)\n\
                 >\n\
                 > I recommend **raising shields**, {}.\n\
                 >\n\
                 > Now.\n\
                 >\n\
                 > File: Shield grid",
                data_chip()
            )
        );
        assert!(drawn.not_shown.is_empty());
        assert!(!drawn.replaces_text);
    }

    /// A link preview is the service and its linked title, and its
    /// description only while it is short; its image is not fetched.
    #[test]
    fn a_link_preview_is_its_service_and_linked_title() {
        let preview = |text: &str| {
            draw(
                &json!({"attachments": [{
                    "from_url": "https://memory-alpha.example/wiki/Romulan",
                    "service_name": "Memory Alpha",
                    "title": "Romulan Star Empire",
                    "title_link": "https://memory-alpha.example/wiki/Romulan",
                    "image_url": "https://memory-alpha.example/romulan.png",
                    "text": text,
                }]}),
                labels(),
            )
            .attachments
        };
        assert_eq!(
            preview("An interstellar state."),
            "> Memory Alpha · **[Romulan Star Empire](https://memory-alpha.example/wiki/Romulan)**\n\
             >\n\
             > An interstellar state."
        );
        assert_eq!(
            preview(&"Long. ".repeat(60)),
            "> Memory Alpha · **[Romulan Star Empire](https://memory-alpha.example/wiki/Romulan)**"
        );
        assert_eq!(
            draw(
                &json!({"attachments": [{"from_url": "https://memory-alpha.example/a"}]}),
                labels()
            )
            .attachments,
            "> <https://memory-alpha.example/a>",
            "a preview with no title is its address"
        );
    }

    /// An integration's card: pretext above the quote, the linked title,
    /// the text and each field as `title: value`.
    #[test]
    fn a_card_draws_pretext_title_text_and_fields() {
        let drawn = draw(
            &json!({"attachments": [{
                "pretext": "Sensor sweep complete",
                "title": "Romulan signature",
                "title_link": "https://sensors.enterprise.example/sweeps/4",
                "text": "Bearing confirmed by <@U_DATA>.",
                "fields": [
                    {"title": "Bearing", "value": "271 mark 4", "short": true},
                    {"title": "Confidence", "value": "*87%*\nrising", "short": true},
                ],
                "fallback": "Sensor sweep: Romulan signature",
                "actions": [{"type": "button", "text": "Hail"}],
            }]}),
            labels(),
        );
        assert_eq!(
            drawn.attachments,
            format!(
                "Sensor sweep complete\n\
                 \n\
                 > **[Romulan signature](https://sensors.enterprise.example/sweeps/4)**\n\
                 >\n\
                 > Bearing confirmed by {}.\n\
                 >\n\
                 > - **Bearing:** 271 mark 4\n\
                 > - **Confidence:** **87%**\n\
                 >   rising",
                data_chip()
            )
        );
        assert!(drawn.not_shown.is_empty());
    }

    /// Only an attachment that draws nothing falls back to its
    /// `fallback`, and one without that is still flagged.
    #[test]
    fn a_fallback_stands_in_only_for_an_empty_attachment() {
        let drawn = draw(
            &json!({"attachments": [
                {"fallback": "Red alert", "actions": [{"type": "button"}]},
                {"actions": [{"type": "button"}]},
            ]}),
            labels(),
        );
        assert_eq!(drawn.attachments, "> Red alert");
        assert_eq!(drawn.not_shown, ["attachment with nothing to show"]);
    }

    /// A bot's blocks are what Slack shows; its `text` is only the
    /// notification's summary, so the blocks take its place. Mentions in
    /// them become people, and plain text stays as typed.
    #[test]
    fn a_bots_blocks_replace_its_text() {
        let drawn = draw(
            &json!({"blocks": [
                {"type": "header", "text": {"type": "plain_text", "text": "Warp core *report* :rocket:"}},
                {"type": "section", "text": {"type": "mrkdwn", "text": "Checked by <@U_DATA>"},
                 "accessory": {"type": "button"},
                 "fields": [
                    {"type": "mrkdwn", "text": "*Efficiency*\n99.7%"},
                    {"type": "plain_text", "text": "Dilithium: aligned"},
                 ]},
                {"type": "divider"},
                {"type": "context", "elements": [
                    {"type": "image", "image_url": "https://x.example/i.png", "alt_text": "badge"},
                    {"type": "mrkdwn", "text": "Main Engineering"},
                    {"type": "plain_text", "text": "Stardate 47988"},
                ]},
                {"type": "actions", "elements": []},
            ]}),
            labels(),
        );
        assert_eq!(
            drawn.blocks,
            format!(
                "**Warp core \\*report\\* 🚀**\n\
                 \n\
                 Checked by {}\n\
                 \n\
                 - **Efficiency**\n  99.7%\n\
                 - Dilithium: aligned\n\
                 \n\
                 Main Engineering · Stardate 47988",
                data_chip()
            )
        );
        assert!(drawn.replaces_text);
        assert_eq!(drawn.body("Warp core report".into()), drawn.blocks);
        assert_eq!(
            drawn.headline.as_deref(),
            Some("Warp core *report* :rocket:"),
            "a thread's title reads what the blocks show, not the summary"
        );
    }

    /// Rich text is the message's `text` again, so where a message has it
    /// the text stays and the other blocks follow. A block nothing draws
    /// keeps the text too, as the one account of it, and is flagged by
    /// its kind.
    #[test]
    fn text_stays_beside_rich_text_or_a_block_not_drawn() {
        let with_rich_text = draw(
            &json!({"blocks": [
                {"type": "rich_text", "elements": []},
                {"type": "context", "elements": [{"type": "mrkdwn", "text": "edited"}]},
            ]}),
            labels(),
        );
        assert!(!with_rich_text.replaces_text);
        assert_eq!(with_rich_text.body("Engage.".into()), "Engage.\n\nedited");

        let with_call = draw(
            &json!({"blocks": [
                {"type": "call", "call_id": "R1"},
                {"type": "section", "text": {"type": "mrkdwn", "text": "Passcode: 1701"}},
                {"type": "table", "rows": []},
            ]}),
            labels(),
        );
        assert!(!with_call.replaces_text);
        assert_eq!(with_call.not_shown, ["call block", "table block"]);
        assert_eq!(
            with_call.body("Picard started a call".into()),
            "Picard started a call\n\nPasscode: 1701"
        );
    }

    /// An image or a video block is a link to it, never an image the
    /// page loads; a link that is not a web address stays text.
    #[test]
    fn an_image_or_video_block_is_a_link() {
        let drawn = draw(
            &json!({"blocks": [
                {"type": "image", "image_url": "https://x.example/warp.png", "alt_text": "Warp [core]"},
                {"type": "video", "title": {"type": "plain_text", "text": "Holodeck"},
                 "title_url": "https://video.example/1", "provider_name": "Holonet"},
                {"type": "video", "title": {"type": "plain_text", "text": "Q"},
                 "title_url": "javascript:alert(1)"},
                {"type": "image", "image_url": "javascript:alert(1)", "alt_text": "Q again"},
            ]}),
            labels(),
        );
        assert_eq!(
            drawn.blocks,
            "Image: [Warp \\[core\\]](https://x.example/warp.png)\n\
             \n\
             Video: [Holodeck](https://video.example/1) (Holonet)\n\
             \n\
             Video: Q\n\
             \n\
             Image: Q again"
        );
        assert!(drawn.not_shown.is_empty());
    }

    /// An app's unfurl carries blocks of its own, drawn as a message's
    /// would be, inside the quote.
    #[test]
    fn an_attachments_blocks_are_drawn_in_its_quote() {
        let drawn = draw(
            &json!({"attachments": [{
                "is_app_unfurl": true,
                "blocks": [
                    {"type": "section", "text": {"type": "mrkdwn", "text": "*Ticket 1701*\nShield grid"}},
                    {"type": "input", "label": {"type": "plain_text", "text": "Reply"}},
                ],
            }]}),
            labels(),
        );
        assert_eq!(drawn.attachments, "> **Ticket 1701**\n> Shield grid");
        assert_eq!(drawn.not_shown, ["input block"]);
    }

    #[test]
    fn a_message_with_neither_draws_nothing() {
        let drawn = draw(&json!({"text": "Engage."}), labels());
        assert_eq!(drawn, Drawn::default());
        assert_eq!(drawn.body("Engage.".into()), "Engage.");
    }
}
