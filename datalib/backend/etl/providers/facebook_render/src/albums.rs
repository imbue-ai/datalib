//! Photo albums: one document per album, its description first and
//! then every photo in the order it was added.

use datalib_etl_chat_common::render::RenderProfile;
use datalib_etl_chat_common::types::{
    ItemKind, NormalizedChat, NormalizedChatItem, NormalizedDoc, UpstreamRef,
};
use datalib_etl_facebook::ingest::schema_raw::ALBUMS_TABLE;

use crate::ids;
use datalib_etl_render::inputs::Inputs;
use serde_json::Value;

use crate::common::{media_attachment, media_caption, profile, str_field, strip_mentions, ts_ms};
use crate::processor::Owner;

pub fn albums_profile() -> RenderProfile {
    profile("Facebook Album", "Facebook Album Message", ids::KIND_ALBUM)
}

pub fn build_albums(albums: &[(String, Value)], owner: &Owner) -> Vec<NormalizedChat> {
    albums.iter().map(|(id, v)| album(id, v, owner)).collect()
}

fn album(row_id: &str, v: &Value, owner: &Owner) -> NormalizedChat {
    let name = str_field(v, "name").unwrap_or("Untitled album");
    let photos: Vec<&Value> = v
        .get("photos")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .collect();
    let first_photo_ms = photos
        .iter()
        .filter_map(|p| ts_ms(p, "creation_timestamp"))
        .min();
    let id = format!("album:{row_id}");
    let inputs = Inputs::default();
    inputs.read(ALBUMS_TABLE, row_id);

    let mut items = Vec::with_capacity(photos.len() + 1);
    if let Some(description) = str_field(v, "description") {
        let date_ms = first_photo_ms.or_else(|| ts_ms(v, "last_modified_timestamp"));
        let item_id = ids::album_description(&owner.source_id, row_id, date_ms);
        items.push(NormalizedChatItem {
            message_uuid: item_id.uuid,
            author_id: "me".to_string(),
            author_display: owner.name.clone(),
            date_ms,
            text: Some(strip_mentions(description)),
            kind: ItemKind::Text,
            attachments: Vec::new(),
            reactions: Vec::new(),
            system_note: None,
            source_url: None,
            kind_label: None,
            source_ref: Some(UpstreamRef::new(item_id.entity_kind, item_id.natural_key)),
            is_aside: false,
            problems: Vec::new(),
        });
    }
    for (i, photo) in photos.iter().enumerate() {
        let Some(att) = media_attachment(photo, row_id, &inputs) else {
            continue;
        };
        let uri = att.ref_id.clone().unwrap_or_else(|| i.to_string());
        let date_ms = ts_ms(photo, "creation_timestamp");
        let item_id = ids::photo(&owner.source_id, row_id, &uri, date_ms);
        items.push(NormalizedChatItem {
            message_uuid: item_id.uuid,
            author_id: "me".to_string(),
            author_display: owner.name.clone(),
            date_ms,
            text: media_caption(photo, Some(name)),
            kind: ItemKind::Attachment,
            attachments: vec![att],
            reactions: Vec::new(),
            system_note: None,
            source_url: None,
            kind_label: Some("Facebook Photo".to_string()),
            source_ref: Some(UpstreamRef::new(item_id.entity_kind, item_id.natural_key)),
            is_aside: false,
            problems: Vec::new(),
        });
    }
    items.sort_by_key(|i| i.date_ms);

    for input in &owner.inputs {
        inputs.read(&input.table, &input.id);
    }
    let album = ids::album(&owner.source_id, row_id);
    NormalizedChat {
        inputs: inputs.declared(),
        path_prefix: None,
        id: id.clone(),
        chat_uuid: album.uuid.clone(),
        display: name.to_string(),
        title: None,
        author: Some(owner.name.clone()),
        account: owner.account.clone(),
        project: None,
        external_id: Some(album.natural_key),
        source_url: None,
        upstream_scope: None,
        org_uuid: None,
        org_name: None,
        buckets: vec![NormalizedDoc {
            orphan_reactions: Vec::new(),
            period_key: "all".to_string(),
            markdown_uuid: album.uuid,
            source_ref: None,
            items,
        }],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn description_then_photos_oldest_first() {
        let album = json!({
            "name": "Ten Forward Nights",
            "photos": [
                {"uri": "m/2.png", "creation_timestamp": 20, "title": "Ten Forward Nights", "description": "Guinan behind the bar."},
                {"uri": "m/1.png", "creation_timestamp": 10, "title": "Ten Forward Nights"},
            ],
            "cover_photo": {"uri": "m/1.png", "creation_timestamp": 10},
            "last_modified_timestamp": 30,
            "description": "Off-duty evenings on Deck 10.",
        });
        let owner = Owner {
            source_id: "fb".to_string(),
            name: "Jean-Luc Picard".to_string(),
            account: None,
            inputs: Vec::new(),
        };
        let chats = build_albums(&[("a1".to_string(), album)], &owner);
        assert_eq!(chats.len(), 1);
        assert_eq!(chats[0].display, "Ten Forward Nights");
        let items = &chats[0].buckets[0].items;
        assert_eq!(items.len(), 3);
        assert_eq!(items[0].kind, ItemKind::Text);
        assert_eq!(
            items[0].text.as_deref(),
            Some("Off-duty evenings on Deck 10.")
        );
        // The description is dated with the first photo, not the album's
        // last edit.
        assert_eq!(items[0].date_ms, Some(10_000));
        assert_eq!(items[1].attachments[0].ref_id.as_deref(), Some("m/1.png"));
        // A photo whose title is just the album name has no caption.
        assert_eq!(items[1].text, None);
        assert_eq!(items[2].text.as_deref(), Some("Guinan behind the bar."));
    }
}
