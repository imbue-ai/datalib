//! Friends as contacts through the shared contact renderer. The export
//! gives a friend a name and the day the friendship was made, nothing
//! more — no profile URL, no id.

use datalib_etl_contact_common::{ContactField, ContactRenderProfile, NormalizedContact};
use datalib_etl_facebook::ingest::schema_raw::FRIENDS_TABLE;

use crate::ids;
use datalib_etl_render::inputs::Inputs;
use datalib_schema::providers::Provider;
use serde_json::Value;

use crate::common::{str_field, ts_ms, RENDER_VERSION, SOURCE_LABEL};
use crate::processor::Owner;

const GROUP_LABEL: &str = "Friends";

pub fn friends_profile(owner: &Owner) -> ContactRenderProfile {
    ContactRenderProfile {
        provider: Provider::Facebook,
        source_label: SOURCE_LABEL.to_string(),
        contact_kind: "Contact".to_string(),
        contact_entity_kind: ids::KIND_FRIEND,
        account: owner.account.clone(),
        render_version: RENDER_VERSION,
    }
}

pub fn build_friends(friends: &[(String, Value)], owner: &Owner) -> Vec<NormalizedContact> {
    friends
        .iter()
        .map(|(row_id, v)| {
            let inputs = Inputs::default();
            inputs.read(FRIENDS_TABLE, row_id);
            for input in &owner.inputs {
                inputs.read(&input.table, &input.id);
            }
            let since = ts_ms(v, "timestamp")
                .and_then(datalib_time::IsoOffsetTimestamp::from_unix_millis)
                .map(|t| t.to_rfc3339_secs());
            let id = ids::friend(&owner.source_id, row_id);
            NormalizedContact {
                contact_uuid: id.uuid,
                group_uuid: ids::friends_group(&owner.source_id).uuid,
                group_label: GROUP_LABEL.to_string(),
                display_name: str_field(v, "name").map(str::to_string),
                external_id: Some(id.natural_key),
                upstream_scope: None,
                created_at: since.clone(),
                modified_at: None,
                source_url: None,
                fields: since
                    .map(|s| vec![ContactField::new("Friends since", s)])
                    .unwrap_or_default(),
                photo: None,
                photo_url: None,
                inputs: inputs.declared(),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_friend_is_a_name_and_a_date() {
        let owner = Owner {
            source_id: "fb".to_string(),
            name: "Jean-Luc Picard".to_string(),
            account: Some("picard@enterprise.starfleet".to_string()),
            inputs: Vec::new(),
        };
        let rows = vec![(
            "f1".to_string(),
            json!({"name": "William Riker", "timestamp": 12_400_000_000_i64}),
        )];
        let contacts = build_friends(&rows, &owner);
        assert_eq!(contacts.len(), 1);
        assert_eq!(contacts[0].display_name.as_deref(), Some("William Riker"));
        assert_eq!(contacts[0].group_label, "Friends");
        assert_eq!(contacts[0].fields[0].label, "Friends since");
        assert!(contacts[0]
            .created_at
            .as_deref()
            .unwrap()
            .starts_with("2362-"));
        assert_eq!(contacts[0].inputs[0].table, FRIENDS_TABLE);
    }
}
