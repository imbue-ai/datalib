//! Which of an account's contacts the config chose: everyone, or those
//! with a label it names.

use std::collections::BTreeSet;

use datalib_etl::download_problems::{self, DownloadProblem};
use serde_json::Value;

use super::str_of;

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Selection {
    /// The chosen labels' resource names; `None` is every contact.
    labels: Option<BTreeSet<String>>,
    /// A name the config gives is no label of the account's. What it
    /// chose before may still be stored, so nothing is deleted for not
    /// being chosen.
    unmatched: bool,
}

impl Selection {
    /// The labels named `configured`, among the account's `groups`. An
    /// error when names were given and none is a label: read as no choice
    /// at all, it would copy everyone.
    pub fn resolve(
        configured: &[String],
        groups: &[Value],
    ) -> anyhow::Result<(Self, Vec<DownloadProblem>)> {
        if configured.is_empty() {
            return Ok((Self::default(), Vec::new()));
        }
        let names = || {
            let mut names: Vec<String> = groups
                .iter()
                .filter(|g| is_label(g))
                .filter_map(name)
                .collect();
            names.sort();
            names.join(", ")
        };
        let resolution = download_problems::resolve_configured("labels", configured, |want| {
            groups
                .iter()
                .filter(|g| is_label(g) && name(g).as_deref() == Some(want))
                .find_map(|g| str_of(g, "resourceName"))
                .ok_or_else(|| format!("no label has that name; the account has {}", names()))
        });
        if resolution.nothing_resolved() {
            anyhow::bail!(
                "none of the configured labels ({}) exists; the account has {}",
                configured.join(", "),
                names()
            );
        }
        let selection = Self {
            labels: Some(resolution.resolved.into_iter().collect()),
            unmatched: !resolution.problems.is_empty(),
        };
        Ok((selection, resolution.problems))
    }

    pub fn chooses(&self, person: &Value) -> bool {
        let Some(chosen) = &self.labels else {
            return true;
        };
        memberships(person).any(|group| chosen.contains(group))
    }

    /// Google's own groups come with any choice: a contact shows which of
    /// them it is in.
    pub fn keeps_group(&self, group: &Value) -> bool {
        let Some(chosen) = &self.labels else {
            return true;
        };
        !is_label(group) || str_of(group, "resourceName").is_some_and(|g| chosen.contains(&g))
    }

    /// Whether a stored contact or label this does not choose may go.
    pub fn drops_the_rest(&self) -> bool {
        !self.unmatched
    }

    /// What a sync token was listed under, so one made under another
    /// choice is not read as having covered this one. `None` is everyone.
    pub fn key(&self) -> Option<String> {
        let labels = self.labels.as_ref()?;
        let mut key = labels.iter().cloned().collect::<Vec<_>>().join(",");
        if self.unmatched {
            key.push_str(",unmatched");
        }
        Some(key)
    }
}

/// A label someone made, as against one of Google's own groups.
pub fn is_label(group: &Value) -> bool {
    group.get("groupType").and_then(Value::as_str) == Some("USER_CONTACT_GROUP")
}

pub fn name(group: &Value) -> Option<String> {
    str_of(group, "formattedName").or_else(|| str_of(group, "name"))
}

/// The resource names of the groups a contact is in.
pub fn memberships(person: &Value) -> impl Iterator<Item = &str> {
    let items = person.get("memberships").and_then(Value::as_array);
    items.into_iter().flatten().filter_map(|m| {
        m.pointer("/contactGroupMembership/contactGroupResourceName")
            .and_then(Value::as_str)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn groups() -> Vec<Value> {
        let group = |id: &str, name: &str, kind: &str| json!({"resourceName": format!("contactGroups/{id}"), "name": name, "formattedName": name, "groupType": kind});
        vec![
            group("b1", "Bridge crew", "USER_CONTACT_GROUP"),
            group("a1", "Away team", "USER_CONTACT_GROUP"),
            group("starred", "Starred", "SYSTEM_CONTACT_GROUP"),
        ]
    }

    fn of(configured: &[&str]) -> anyhow::Result<(Selection, Vec<DownloadProblem>)> {
        let configured: Vec<String> = configured.iter().map(|s| s.to_string()).collect();
        Selection::resolve(&configured, &groups())
    }

    fn in_groups(ids: &[&str]) -> Value {
        let m: Vec<Value> = ids
            .iter()
            .map(|id| json!({"contactGroupMembership": {"contactGroupResourceName": format!("contactGroups/{id}")}}))
            .collect();
        json!({ "memberships": m })
    }

    #[test]
    fn a_choice_keeps_its_labels_and_googles_own_groups() {
        let (everyone, _) = of(&[]).unwrap();
        assert!(everyone.chooses(&in_groups(&[])));
        assert_eq!(everyone.key(), None);

        let (bridge, problems) = of(&["Bridge crew"]).unwrap();
        assert!(problems.is_empty() && bridge.drops_the_rest());
        assert!(bridge.chooses(&in_groups(&["starred", "b1"])));
        assert!(!bridge.chooses(&in_groups(&["a1", "starred"])));
        assert!(!bridge.chooses(&json!({})));
        let kept: Vec<bool> = groups().iter().map(|g| bridge.keeps_group(g)).collect();
        assert_eq!(kept, [true, false, true]);
        assert_ne!(
            bridge.key(),
            of(&["Bridge crew", "Away team"]).unwrap().0.key()
        );
        assert_eq!(
            of(&["Away team", "Bridge crew"]).unwrap().0.key(),
            of(&["Bridge crew", "Away team"]).unwrap().0.key(),
            "the order of the names is not a choice"
        );
    }

    /// Reading a choice nothing matches as "everyone" would copy the whole
    /// address book the choice was there to leave out.
    #[test]
    fn a_name_no_label_has_costs_itself_unless_it_is_the_only_one() {
        let (some, problems) = of(&["Bridge crew", "Holodeck"]).unwrap();
        assert_eq!(problems.len(), 1);
        assert!(!some.drops_the_rest());
        assert_ne!(some.key(), of(&["Bridge crew"]).unwrap().0.key());
        let err = of(&["Holodeck"]).unwrap_err().to_string();
        assert!(err.contains("Away team, Bridge crew"), "{err}");
        assert!(of(&["Starred"]).is_err(), "Starred is no label");
    }
}
