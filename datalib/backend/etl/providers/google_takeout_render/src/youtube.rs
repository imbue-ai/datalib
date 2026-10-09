//! YouTube: the videos the account watched, a document per year, and
//! the channels it subscribes to, which carry no date, in one.

use datalib_etl::bulk::BulkUpsertable;
use datalib_etl_chat_common::types::NormalizedChat;
use datalib_etl_google_takeout::ingest::schema_raw::{YoutubeSubscriptionRow, YoutubeWatchRow};
use datalib_etl_render::html::{escape_md_inline, md_link_dest};
use datalib_etl_render::inputs::Inputs;
use serde_json::Value;

use crate::feeds::{self, Row};
use crate::ids;

pub fn build_history(source_id: &str, rows: &[Row]) -> Vec<NormalizedChat> {
    if rows.is_empty() {
        return Vec::new();
    }
    let inputs = Inputs::default();
    let items = rows
        .iter()
        .map(|row| {
            inputs.read(YoutubeWatchRow::TABLE, &row.id);
            let mut problems = Vec::new();
            let date_ms = feeds::stamp_ms(row, &mut problems);
            let mut item = feeds::item(
                ids::feed_item(source_id, ids::KIND_YOUTUBE_WATCH, &row.id, date_ms),
                feeds::ME,
                date_ms,
                "YouTube Watch",
                problems,
            );
            let video = link(&row.payload, "/videoTitle", "/videoUrl");
            let channel = link(&row.payload, "/channelTitle", "/channelUrl");
            item.text = match (video, channel) {
                (Some(v), Some(c)) => Some(format!("Watched {v} · {c}")),
                (Some(v), None) => Some(format!("Watched {v}")),
                (None, _) => None,
            };
            item.source_url = feeds::str_at(&row.payload, "/videoUrl").map(str::to_string);
            item
        })
        .collect();
    vec![feeds::yearly(
        source_id,
        feeds::YOUTUBE_HISTORY,
        "YouTube history",
        items,
        inputs,
    )]
}

pub fn build_subscriptions(source_id: &str, rows: &[Row]) -> Vec<NormalizedChat> {
    if rows.is_empty() {
        return Vec::new();
    }
    let inputs = Inputs::default();
    let mut items: Vec<_> = rows
        .iter()
        .map(|row| {
            inputs.read(YoutubeSubscriptionRow::TABLE, &row.id);
            let mut item = feeds::item(
                ids::feed_item(source_id, ids::KIND_YOUTUBE_SUBSCRIPTION, &row.id, None),
                feeds::ME,
                None,
                "YouTube Subscription",
                Vec::new(),
            );
            item.text = link(&row.payload, "/channelTitle", "/channelUrl");
            item.source_url = feeds::str_at(&row.payload, "/channelUrl").map(str::to_string);
            item
        })
        .collect();
    items.sort_by(|a, b| a.text.cmp(&b.text));
    vec![feeds::whole(
        source_id,
        feeds::YOUTUBE_SUBSCRIPTIONS,
        "YouTube subscriptions",
        items,
        inputs,
    )]
}

/// `[title](url)`, or the bare title when there is no URL.
fn link(payload: &Value, title: &str, url: &str) -> Option<String> {
    let title = escape_md_inline(feeds::str_at(payload, title)?);
    Some(match feeds::str_at(payload, url) {
        Some(url) => format!("[{title}]({})", md_link_dest(url)),
        None => title,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_watch_links_the_video_and_its_channel() {
        let rows = [Row {
            id: "w1".to_string(),
            payload: json!({
                "videoTitle": "Captain's Log [Stardate 41153.7]",
                "videoUrl": "https://www.youtube.com/watch?v=trekS01E01",
                "channelTitle": "Starfleet Archives",
                "channelUrl": "https://www.youtube.com/channel/UCpicard001",
            }),
            when: Some("2364-06-04T11:48:37-07:00".to_string()),
        }];
        let chats = build_history("gt", &rows);
        let item = &chats[0].buckets[0].items[0];
        assert_eq!(chats[0].buckets[0].period_key, "2364");
        assert_eq!(
            item.text.as_deref(),
            Some(
                "Watched [Captain's Log \\[Stardate 41153.7\\]](https://www.youtube.com/watch?v=trekS01E01) \
                 · [Starfleet Archives](https://www.youtube.com/channel/UCpicard001)"
            )
        );
    }

    #[test]
    fn subscriptions_are_one_document_in_title_order() {
        let sub = |id: &str, title: &str| Row {
            id: id.to_string(),
            payload: json!({"channelTitle": title}),
            when: None,
        };
        let chats = build_subscriptions("gt", &[sub("b", "Riker"), sub("a", "Data")]);
        let [doc] = chats[0].buckets.as_slice() else {
            panic!("one document")
        };
        assert_eq!(doc.period_key, "all");
        let titles: Vec<_> = doc.items.iter().map(|i| i.text.as_deref()).collect();
        assert_eq!(titles, [Some("Data"), Some("Riker")]);
    }
}
