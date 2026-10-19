//! Google Maps: the places the account reviewed, saved and photographed,
//! one feed with a document per year.

use datalib_etl::bulk::BulkUpsertable;
use datalib_etl_chat_common::findings::{noted, shape_of, unread_keys};
use datalib_etl_chat_common::types::{NormalizedChat, NormalizedChatItem};
use datalib_etl_google_takeout::ingest::schema_raw::{
    MapsPhotoRow, MapsReviewRow, MapsSavedPlaceRow,
};
use datalib_etl_render::html::{escape_md_block, escape_md_inline, md_link_dest};
use datalib_etl_render::inputs::Inputs;
use datalib_schema::problems::{Problem, Reason};
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
            .map(stars);
        let heading = [place(props), stars]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>();
        let mut problems = feature_findings(&row.payload, REVIEW_KEYS);
        let comment = comment(props, &mut problems);
        let text = paragraphs([
            Some(heading.join(" ")),
            address(props),
            feeds::str_at(props, "/review_text_published").map(escape_md_block),
            answers(props, &mut problems),
            comment,
        ]);
        let mut item = place_item(
            source_id,
            row,
            ids::KIND_MAPS_REVIEW,
            "Google Maps Review",
            text,
            props,
        );
        item.problems.extend(problems);
        items.push(item);
    }
    for row in saved {
        inputs.read(MapsSavedPlaceRow::TABLE, &row.id);
        let props = &row.payload["properties"];
        let mut problems = feature_findings(&row.payload, SAVED_KEYS);
        let comment = comment(props, &mut problems);
        let text = paragraphs([place(props), address(props), comment]);
        let mut item = place_item(
            source_id,
            row,
            ids::KIND_MAPS_SAVED_PLACE,
            "Google Maps Saved Place",
            text,
            props,
        );
        item.problems.extend(problems);
        items.push(item);
    }
    for row in photos {
        inputs.read(MapsPhotoRow::TABLE, &row.id);
        let mut problems = unread_keys(&row.payload, PHOTO_KEYS, "");
        for (key, read) in PHOTO_NESTED {
            if let Some(v) = row.payload.get(*key) {
                problems.extend(unread_keys(v, read, &format!("/{key}")));
            }
        }
        let date_ms = feeds::stamp_ms(row, &mut problems);
        let item = feeds::own_item(
            ids::feed_item(source_id, ids::KIND_MAPS_PHOTO, &row.id, date_ms),
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

/// A GeoJSON feature's keys. Its geometry is where the place is, which
/// the place's own link already shows.
const FEATURE_KEYS: &[&str] = &["type", "geometry", "properties"];
const LOCATION_KEYS: &[&str] = &["name", "address", "country_code"];
/// `Comment` is Google's, not the account's: see [`comment`].
const REVIEW_KEYS: &[&str] = &[
    "date",
    "five_star_rating_published",
    "google_maps_url",
    "location",
    "review_text_published",
    "questions",
    "Comment",
];
const SAVED_KEYS: &[&str] = &["date", "google_maps_url", "location", "Comment"];
const QUESTION_KEYS: &[&str] = &[
    "question",
    "rating",
    "selected_option",
    "selected_options",
    "text_answer",
];
/// A photo's sidecar. Where and when the camera took it, its view count
/// and its upload time are the photo's metadata; the title and caption
/// are what is shown.
const PHOTO_KEYS: &[&str] = &[
    "title",
    "description",
    "creationTime",
    "photoTakenTime",
    "geoData",
    "geoDataExif",
    "imageViews",
    "url",
];
const PHOTO_NESTED: &[(&str, &[&str])] = &[
    ("creationTime", &["timestamp", "formatted"]),
    ("photoTakenTime", &["timestamp", "formatted"]),
];

/// Google writes this in `Comment` for a place it has no location for.
const NO_LOCATION: &[&str] = &[
    "No location information is available for this saved place",
    "No location information is available for this review",
];

fn feature_findings(feature: &Value, read: &[&str]) -> Vec<Problem> {
    let mut out = unread_keys(feature, FEATURE_KEYS, "");
    let props = &feature["properties"];
    out.extend(unread_keys(props, read, "/properties"));
    if let Some(location) = props.get("location") {
        out.extend(unread_keys(location, LOCATION_KEYS, "/properties/location"));
    }
    out
}

/// `Comment` is the sentence Google writes where it has no location for
/// the place: a note on the record, not text. Anything else there would
/// be something written about the place, and is drawn.
fn comment(props: &Value, problems: &mut Vec<Problem>) -> Option<String> {
    let c = feeds::str_at(props, "/Comment")?;
    if NO_LOCATION.contains(&c) {
        problems.push(noted("Comment", "Google has no location for this place"));
        return None;
    }
    problems.push(noted(
        "Comment",
        "a Comment that is not Google's no-location sentence",
    ));
    Some(escape_md_block(c))
}

/// The questions Maps asked after the review, each with what was
/// answered: stars, a choice, choices, or words.
fn answers(props: &Value, problems: &mut Vec<Problem>) -> Option<String> {
    let questions = props.get("questions")?.as_array()?;
    let mut lines = Vec::new();
    for (i, q) in questions.iter().enumerate() {
        let path = format!("/properties/questions/{i}");
        problems.extend(unread_keys(q, QUESTION_KEYS, &path));
        let Some(question) = feeds::str_at(q, "/question") else {
            problems.push(Problem::field("question", Reason::UncoveredType, &shape_of(q)).at(path));
            continue;
        };
        let mut said: Vec<String> = Vec::new();
        if let Some(n) = q.get("rating").and_then(Value::as_u64) {
            said.push(stars(n));
        }
        if let Some(choice) = feeds::str_at(q, "/selected_option") {
            said.push(escape_md_inline(choice));
        }
        if let Some(choices) = q.get("selected_options").and_then(Value::as_array) {
            let picked: Vec<String> = choices
                .iter()
                .filter_map(Value::as_str)
                .map(escape_md_inline)
                .collect();
            if !picked.is_empty() {
                said.push(picked.join("; "));
            }
        }
        if let Some(words) = feeds::str_at(q, "/text_answer") {
            said.push(escape_md_inline(words));
        }
        if said.is_empty() {
            continue;
        }
        lines.push(format!(
            "- **{}:** {}",
            escape_md_inline(question),
            said.join(" · ")
        ));
    }
    (!lines.is_empty()).then(|| lines.join("\n"))
}

fn stars(n: u64) -> String {
    let n = n.min(5) as usize;
    "★".repeat(n) + &"☆".repeat(5 - n)
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
    let mut item = feeds::own_item(
        ids::feed_item(source_id, kind, &row.id, date_ms),
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
                "Comment": "No location information is available for this saved place",
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
        assert!(
            !pin.text.as_deref().unwrap().contains("No location"),
            "Google's sentence is a note, not text: {pin:?}"
        );
        assert_eq!(pin.problems.len(), 1, "{:?}", pin.problems);
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

    /// The questions Maps asks after a review were left out, and a key
    /// the render does not read went unsaid.
    #[test]
    fn a_review_draws_its_answers_and_reports_what_it_leaves_out() {
        let reviews = [row(
            "r1",
            "2364-06-03T19:30:00Z",
            json!({"type": "Feature", "geometry": {"type": "Point", "coordinates": [0, 0]},
                   "properties": {
                "five_star_rating_published": 5,
                "google_maps_url": "https://www.google.com/maps/place/Ten+Forward",
                "location": {"name": "Ten Forward", "country_code": "UFP"},
                "review_text_published": "Best seat: by the window.",
                "questions": [
                    {"question": "Food", "rating": 4},
                    {"question": "Price per person", "selected_option": "Replicated, free"},
                    {"question": "Recommended dishes", "selected_options": ["Gagh", "Tea, Earl Grey"]},
                    {"question": "Anything else?", "text_answer": "Ask Guinan."},
                    {"question": "Wait time"},
                ],
                "visited_with": "Data",
            }}),
        )];
        let chats = build("gt", &reviews, &[], &[]);
        let item = &chats[0].buckets[0].items[0];
        let text = item.text.as_deref().unwrap();
        assert!(
            text.ends_with(
                "- **Food:** ★★★★☆\n\
             - **Price per person:** Replicated, free\n\
             - **Recommended dishes:** Gagh; Tea, Earl Grey\n\
             - **Anything else?:** Ask Guinan."
            ),
            "{text}"
        );
        let unread: Vec<(String, String)> = item
            .problems
            .iter()
            .map(|p| (p.field.clone().unwrap_or_default(), p.sample.clone()))
            .collect();
        assert_eq!(
            unread,
            [("visited_with".to_string(), "string(4 chars)".to_string())]
        );
    }
}
