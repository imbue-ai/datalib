//! Google Maps: the places the account reviewed, saved and photographed,
//! one feed with a document per year.

use datalib_etl::bulk::BulkUpsertable;
use datalib_etl_chat_common::types::{NormalizedChat, NormalizedChatItem};
use datalib_etl_google_takeout::ingest::schema_raw::{
    MapsPhotoRow, MapsReviewRow, MapsSavedPlaceRow,
};
use datalib_etl_render::html::{escape_md_block, escape_md_inline, md_link_dest};
use datalib_etl_render::inputs::Inputs;
use serde_json::Value;

use crate::feeds::{self, Row};
use crate::ids;

pub fn build(
    source_id: &str,
    reviews: &[Row],
    saved: &[Row],
    photos: &[Row],
) -> Vec<NormalizedChat> {
    if reviews.is_empty() && saved.is_empty() && photos.is_empty() {
        return Vec::new();
    }
    let inputs = Inputs::default();
    let mut items = Vec::with_capacity(reviews.len() + saved.len() + photos.len());
    for row in reviews {
        inputs.read(MapsReviewRow::TABLE, &row.id);
        let props = &row.payload["properties"];
        let stars = props
            .get("five_star_rating_published")
            .and_then(Value::as_u64)
            .map(|n| "★".repeat(n.min(5) as usize) + &"☆".repeat(5 - n.min(5) as usize));
        let heading = [place(props), stars]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>();
        let text = paragraphs([
            Some(heading.join(" ")),
            address(props),
            feeds::str_at(props, "/review_text_published").map(escape_md_block),
        ]);
        items.push(place_item(
            source_id,
            row,
            ids::KIND_MAPS_REVIEW,
            "Google Maps Review",
            text,
            props,
        ));
    }
    for row in saved {
        inputs.read(MapsSavedPlaceRow::TABLE, &row.id);
        let props = &row.payload["properties"];
        let text = paragraphs([
            place(props),
            address(props),
            feeds::str_at(props, "/Comment").map(escape_md_block),
        ]);
        items.push(place_item(
            source_id,
            row,
            ids::KIND_MAPS_SAVED_PLACE,
            "Google Maps Saved Place",
            text,
            props,
        ));
    }
    for row in photos {
        inputs.read(MapsPhotoRow::TABLE, &row.id);
        let mut problems = Vec::new();
        let date_ms = feeds::stamp_ms(row, &mut problems);
        let item = feeds::item(
            ids::feed_item(source_id, ids::KIND_MAPS_PHOTO, &row.id, date_ms),
            feeds::ME,
            date_ms,
            "Google Maps Photo",
            problems,
        );
        let caption = feeds::str_at(&row.payload, "/description").map(escape_md_block);
        let name = feeds::str_at(&row.payload, "/title").unwrap_or(&row.id);
        items.push(feeds::with_attachments(
            feeds::with_text(item, caption),
            vec![feeds::attachment(row.id.clone(), name, &row.id)],
        ));
    }
    vec![feeds::yearly(
        source_id,
        feeds::MAPS,
        "Google Maps",
        items,
        inputs,
    )]
}

fn place_item(
    source_id: &str,
    row: &Row,
    kind: &'static str,
    kind_label: &str,
    text: Option<String>,
    props: &Value,
) -> NormalizedChatItem {
    let mut problems = Vec::new();
    let date_ms = feeds::stamp_ms(row, &mut problems);
    let mut item = feeds::item(
        ids::feed_item(source_id, kind, &row.id, date_ms),
        feeds::ME,
        date_ms,
        kind_label,
        problems,
    );
    item.text = text;
    item.source_url = feeds::str_at(props, "/google_maps_url").map(str::to_string);
    item
}

/// The place's name, linked to it on Maps, in bold. A pin dropped on an
/// address has no name; its `q=` query is the address it was dropped on.
fn place(props: &Value) -> Option<String> {
    let url = feeds::str_at(props, "/google_maps_url");
    let name = feeds::str_at(props, "/location/name")
        .map(str::to_string)
        .or_else(|| url.and_then(address_query))?;
    let name = escape_md_inline(&name);
    Some(match url {
        Some(url) => format!("**[{name}]({})**", md_link_dest(url)),
        None => format!("**{name}**"),
    })
}

fn address(props: &Value) -> Option<String> {
    feeds::str_at(props, "/location/address").map(escape_md_inline)
}

fn address_query(url: &str) -> Option<String> {
    let query = url::Url::parse(url).ok()?;
    let (_, q) = query.query_pairs().find(|(k, _)| k == "q")?;
    Some(q.into_owned()).filter(|q| !q.trim().is_empty())
}

fn paragraphs<const N: usize>(parts: [Option<String>; N]) -> Option<String> {
    let text: Vec<String> = parts
        .into_iter()
        .flatten()
        .filter(|p| !p.is_empty())
        .collect();
    (!text.is_empty()).then(|| text.join("\n\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn row(id: &str, when: &str, payload: Value) -> Row {
        Row {
            id: id.to_string(),
            payload,
            when: Some(when.to_string()),
        }
    }

    #[test]
    fn reviews_saved_places_and_photos_share_one_feed_by_year() {
        let reviews = [row(
            "r1",
            "2364-06-03T19:30:00Z",
            json!({"properties": {
                "five_star_rating_published": 4,
                "google_maps_url": "https://www.google.com/maps/place/Ten+Forward",
                "location": {"name": "Ten Forward", "address": "Deck 10, USS Enterprise-D"},
                "review_text_published": "Synthehol is *acceptable*.",
            }}),
        )];
        let saved = [row(
            "s1",
            "2365-03-01T08:00:00Z",
            json!({"properties": {
                "google_maps_url": "http://maps.google.com/?q=Quark%27s+Bar,+Deep+Space+Nine",
                "Comment": "Avoid the Dabo table",
            }}),
        )];
        let photos = [row(
            "2364-06-04-tenfwd.jpg",
            "12446854200",
            json!({"title": "Ten Forward bar", "description": "View from the bar"}),
        )];
        let chats = build("gt", &reviews, &saved, &photos);
        let [chat] = chats.as_slice() else {
            panic!("one feed")
        };
        let years: Vec<&str> = chat.buckets.iter().map(|b| b.period_key.as_str()).collect();
        assert_eq!(years, ["2364", "2365"]);

        let review = &chat.buckets[0].items[0];
        assert_eq!(
            review.text.as_deref(),
            Some(
                "**[Ten Forward](https://www.google.com/maps/place/Ten+Forward)** ★★★★☆\n\n\
                 Deck 10, USS Enterprise-D\n\nSynthehol is *acceptable*."
            )
        );
        let pin = &chat.buckets[1].items[0];
        assert!(
            pin.text
                .as_deref()
                .unwrap()
                .starts_with("**[Quark's Bar, Deep Space Nine]("),
            "{pin:?}"
        );
        let photo = &chat.buckets[0].items[1];
        assert_eq!(photo.text.as_deref(), Some("View from the bar"));
        assert_eq!(
            photo.attachments[0].ref_id.as_deref(),
            Some("2364-06-04-tenfwd.jpg")
        );
        assert_eq!(
            photo.attachments[0].mime_type.as_deref(),
            Some("image/jpeg"),
            "typed by its file, not its title"
        );
        assert_eq!(chat.inputs.len(), 3);
    }
}
