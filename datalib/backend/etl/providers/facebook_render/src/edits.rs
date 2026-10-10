//! Edit history: `posts/edits_you_made_to_posts.json` and
//! `comments_and_reactions/your_comment_edits.json` hold a post's or a
//! comment's saved versions — the first at the moment it was posted, the
//! last the text it has now — and name neither the post nor the comment.
//! A version is tied to what it is a version of by its text: the most
//! alike post or comment made no later than it. Versions sharing an
//! `fbid` are one thing's versions, and go together.

use std::collections::BTreeMap;

use datalib_etl_chat_common::types::NormalizedChatItem;
use datalib_etl_render::html::escape_md_block;
use serde_json::Value;
use similar::TextDiff;

use datalib_schema::problems::Problem;

use crate::common::{
    chat_item, label_value, str_field, strip_mentions, ts_ms, unread_keys, unread_labels,
};
use crate::ids;
use crate::processor::Owner;

/// How alike a version's text and a post's must be, as `similar`'s
/// ratio, to be its version. On a real export an earlier version scored
/// 0.95 and 0.999 against the text it became.
const MIN_RATIO: f32 = 0.6;

/// One saved version, as the edit file holds it.
#[derive(Debug, Clone, PartialEq)]
pub struct Version {
    pub row_id: String,
    pub fbid: Option<String>,
    pub text: String,
    pub date_ms: Option<i64>,
    /// What render does not read of the record: a key, or a label with
    /// something in it, beside its `Text`.
    pub problems: Vec<Problem>,
}

impl Version {
    /// From an edit record's `label_values`: the `Text` entry. `None`
    /// for a record with none.
    pub fn from_record(row_id: &str, v: &Value) -> Option<Self> {
        let text = label_value(v, "Text").and_then(|lv| str_field(lv, "value"))?;
        Some(Self {
            row_id: row_id.to_string(),
            fbid: str_field(v, "fbid").map(str::to_string),
            text: text.to_string(),
            date_ms: ts_ms(v, "timestamp"),
            problems: unread_keys(v, &["fbid", "timestamp", "label_values", "media"], "")
                .into_iter()
                .chain(unread_labels(v, &["Text"]))
                .collect(),
        })
    }
}

/// What a version may belong to: a post's or a comment's text now, and
/// when it was made.
pub struct Target<'a> {
    pub text: &'a str,
    pub date_ms: Option<i64>,
}

/// One thing's versions, oldest first, and which target they are of.
#[derive(Debug, Clone, PartialEq)]
pub struct History {
    pub target: Option<usize>,
    pub versions: Vec<Version>,
}

impl History {
    /// What the versions not shown — the one that is the text now —
    /// could not have read: theirs to carry on the target's own item.
    pub fn unshown_problems(&self, now: &str) -> Vec<Problem> {
        self.versions
            .iter()
            .filter(|v| v.text.trim() == now.trim())
            .flat_map(|v| v.problems.iter().cloned())
            .collect()
    }

    /// The versions to show beside the target's text: every one whose
    /// text is not the text it has now.
    pub fn earlier<'a>(&'a self, now: &'a str) -> impl Iterator<Item = &'a Version> + 'a {
        self.versions
            .iter()
            .filter(move |v| v.text.trim() != now.trim())
    }
}

/// Every edit record, grouped into one history per thing edited and
/// tied to its target where one is alike enough.
pub fn histories(versions: Vec<Version>, targets: &[Target<'_>]) -> Vec<History> {
    let mut groups: BTreeMap<String, Vec<Version>> = BTreeMap::new();
    for v in versions {
        let key = v
            .fbid
            .clone()
            .unwrap_or_else(|| format!("row:{}", v.row_id));
        groups.entry(key).or_default().push(v);
    }
    groups
        .into_values()
        .map(|mut versions| {
            versions.sort_by_key(|v| v.date_ms);
            let target = versions
                .iter()
                .filter_map(|v| best_target(v, targets))
                .max_by(|a, b| a.1.total_cmp(&b.1))
                .map(|(i, _)| i);
            History { target, versions }
        })
        .collect()
}

/// Earlier versions as items of their own, oldest first, folded
/// together into one "another version" just above the text they became.
pub fn version_items(
    versions: &[&Version],
    entity_kind: &'static str,
    kind_label: &str,
    owner: &Owner,
) -> Vec<NormalizedChatItem> {
    let Some(first) = versions.first() else {
        return Vec::new();
    };
    let branch = format!("versions:{}", first.row_id);
    versions
        .iter()
        .map(|v| {
            let id = ids::version(&owner.source_id, entity_kind, &v.row_id, v.date_ms);
            let text = escape_md_block(&strip_mentions(&v.text));
            NormalizedChatItem {
                kind_label: Some(kind_label.to_string()),
                branch: vec![branch.clone()],
                problems: v.problems.clone(),
                ..chat_item(id, owner.name.clone(), v.date_ms, Some(text), Vec::new())
            }
        })
        .collect()
}

fn best_target(v: &Version, targets: &[Target<'_>]) -> Option<(usize, f32)> {
    targets
        .iter()
        .enumerate()
        .filter(|(_, t)| match (t.date_ms, v.date_ms) {
            (Some(made), Some(saved)) => made <= saved,
            _ => true,
        })
        .map(|(i, t)| (i, ratio(t.text, &v.text)))
        .filter(|(_, r)| *r >= MIN_RATIO)
        .max_by(|a, b| a.1.total_cmp(&b.1))
}

fn ratio(a: &str, b: &str) -> f32 {
    if a == b {
        return 1.0;
    }
    TextDiff::from_chars(a, b).ratio()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn version(row: &str, fbid: Option<&str>, text: &str, ms: i64) -> Version {
        Version {
            row_id: row.to_string(),
            fbid: fbid.map(str::to_string),
            text: text.to_string(),
            date_ms: Some(ms),
            problems: Vec::new(),
        }
    }

    #[test]
    fn versions_go_to_the_most_alike_post_made_before_them() {
        let targets = [
            Target {
                text: "Tea, Earl Grey, hot. ☕ Evening in Ten Forward.",
                date_ms: Some(1_000),
            },
            Target {
                text: "Engage.",
                date_ms: Some(2_000),
            },
        ];
        let got = histories(
            vec![
                version(
                    "e2",
                    Some("9"),
                    "Tea, Earl Grey, hot. ☕ Evening in Ten Forward.",
                    1_013,
                ),
                version(
                    "e1",
                    Some("9"),
                    "Tea, Earl Grey, hot. Evening in Ten Forward",
                    1_000,
                ),
                version("e3", None, "Something nobody can place at all", 5_000),
            ],
            &targets,
        );
        assert_eq!(got.len(), 2);
        let tea = got.iter().find(|h| h.target == Some(0)).unwrap();
        assert_eq!(
            tea.versions
                .iter()
                .map(|v| v.row_id.as_str())
                .collect::<Vec<_>>(),
            ["e1", "e2"],
            "oldest first"
        );
        let earlier: Vec<&str> = tea
            .earlier(targets[0].text)
            .map(|v| v.row_id.as_str())
            .collect();
        assert_eq!(earlier, ["e1"], "the version it has now is not shown twice");
        assert!(
            got.iter().any(|h| h.target.is_none()),
            "an edit of nothing in the export"
        );
    }

    #[test]
    fn a_post_made_after_the_version_cannot_be_its_target() {
        let targets = [Target {
            text: "Make it so.",
            date_ms: Some(10_000),
        }];
        let got = histories(vec![version("e", None, "Make it so.", 5_000)], &targets);
        assert_eq!(got[0].target, None);
    }
}
