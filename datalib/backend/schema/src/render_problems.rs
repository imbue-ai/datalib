// What render could not do, kept beside what it did.

use datalib_etl_macros::PortableTable;
use serde::{Deserialize, Serialize};

/// What happened to the record this row is about.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Serialize,
    Deserialize,
    strum::EnumString,
    strum::IntoStaticStr,
    strum::VariantArray,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum Outcome {
    /// The record did not reach the index at all.
    Dropped,
    /// It did, with at least one field discarded.
    Nulled,
    /// It did, intact. These problems are observations — a finding
    /// worth publishing, claiming nothing was lost. R6 in miniature.
    Ok,
}

impl Outcome {
    pub fn as_str(self) -> &'static str {
        self.into()
    }
}

/// Why one field or record is being reported.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Serialize,
    Deserialize,
    strum::EnumString,
    strum::IntoStaticStr,
    strum::VariantArray,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum Reason {
    /// The stored payload would not deserialize. → drop the record.
    Undeserializable,
    /// No usable identity, so nothing could be keyed on. → drop.
    NoIdentity,
    /// A field failed its declared coercion. → null that field, keep
    /// the record.
    CoercionFailed,
    /// A value whose type the contract does not cover. → null that
    /// field; never pass it through untyped.
    UncoveredType,
    /// A deliberate lossy rule fired (truncation, chrome-stripping).
    /// These are the rows R3's judgment-call table is generated from,
    /// which is why `Problem::rule` exists.
    DeliberateLoss,
    /// Nothing was lost; this is a finding worth publishing.
    Noted,
}

impl Reason {
    pub fn as_str(self) -> &'static str {
        self.into()
    }
}

/// Which of the two things [`RenderProblemRow::scope_key`] holds — the
/// sweep key's type, and so which `DELETE … WHERE scope_kind = ?` clears
/// this row when its owner is reprocessed.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, strum::EnumString, strum::IntoStaticStr, strum::VariantArray,
)]
#[strum(serialize_all = "snake_case")]
pub enum ScopeKind {
    /// `scope_key` is the `markdown_uuid` of the document the record
    /// belongs to. The usual case.
    Markdown,
    /// `scope_key` is the raw-store entity id, because the failure
    /// happened before we knew which document the record was for.
    Entity,
}

impl ScopeKind {
    pub fn as_str(self) -> &'static str {
        self.into()
    }

    pub fn parse(s: &str) -> Option<ScopeKind> {
        s.parse().ok()
    }
}

/// Which half of the projection noticed the problem. Distinguishes "the
/// stored payload would not deserialize" from "the row would not
/// validate", which are fixed in different places.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, strum::EnumString, strum::IntoStaticStr, strum::VariantArray,
)]
#[strum(serialize_all = "snake_case")]
pub enum Stage {
    /// Deserializing the stored payload.
    Parse,
    /// Projecting it to markdown.
    Render,
    /// Building the `grid_rows` row.
    GridRow,
}

impl Stage {
    pub fn as_str(self) -> &'static str {
        self.into()
    }

    pub fn parse_str(s: &str) -> Option<Stage> {
        s.parse().ok()
    }
}

pub fn sample_of(s: &str) -> String {
    const MAX: usize = 80;
    if s.len() <= MAX {
        return s.to_string();
    }
    let mut cut = MAX;
    while cut > 0 && !s.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}…", &s[..cut])
}

/// One thing that went wrong with one record. Serialized as a list into
/// [`RenderProblemRow::problems`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Problem {
    /// The field this is about; `None` for a record-level problem
    /// (undeserializable, no identity).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub field: Option<String>,
    /// Where in the stored payload, as a JSON pointer, when we know it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    pub reason: Reason,
    /// The R3 judgment-call rule that fired, when this was a deliberate
    /// lossy rule rather than a defect. This is the column R3's table is
    /// generated from:
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rule: Option<String>,
    /// First 80 characters of the offending value — see [`sample_of`].
    pub sample: String,
}

impl Problem {
    /// A field-level problem: the field survived as null, or did not
    /// survive at all, and here is what it looked like.
    pub fn field(name: impl Into<String>, reason: Reason, sample: &str) -> Self {
        Self {
            field: Some(name.into()),
            path: None,
            reason,
            rule: None,
            sample: sample_of(sample),
        }
    }

    pub fn record(reason: Reason, sample: &str) -> Self {
        Self {
            field: None,
            path: None,
            reason,
            rule: None,
            sample: sample_of(sample),
        }
    }

    /// A deliberate lossy rule fired. `rule` is what R3's generated
    /// table groups by, so it must be stable across runs — name it for
    /// the rule, not for the value it happened to see.
    pub fn lossy(rule: impl Into<String>, field: Option<String>, sample: &str) -> Self {
        Self {
            field,
            path: None,
            reason: Reason::DeliberateLoss,
            rule: Some(rule.into()),
            sample: sample_of(sample),
        }
    }

    pub fn at(mut self, path: impl Into<String>) -> Self {
        self.path = Some(path.into());
        self
    }
}

/// One item the render stage had something to say about.
#[derive(Debug, Clone, Serialize, Deserialize, PortableTable)]
#[portable_table(table = "render_problems", primary_key = "uuid")]
pub struct RenderProblemRow {
    /// The item this is about — normally the `grid_rows.uuid` the
    /// record would have produced.
    #[col(sql = "VARCHAR(96)")]
    pub uuid: String,
    /// What must be reprocessed for this row to be re-evaluated: the
    /// `markdown_uuid` of the document the record belongs to, or — when
    /// the failure happened before we knew that — the raw-store entity
    /// id. This is the sweep key.
    #[col(sql = "VARCHAR(96)")]
    pub scope_key: String,
    /// A [`ScopeKind`], as its `as_str`, saying which of the two
    /// `scope_key` is.
    #[col(sql = "VARCHAR(16)")]
    pub scope_kind: String,
    /// The source that produced this, matching `markdowns.source_name`.
    #[col(sql = "VARCHAR(64)")]
    pub source_name: String,
    /// A [`Stage`], as its `as_str`.
    #[col(sql = "VARCHAR(16)")]
    pub stage: String,
    /// [`Outcome`], as its `as_str`.
    #[col(sql = "VARCHAR(16)")]
    pub outcome: String,
    /// `serde_json` of `Vec<Problem>` — one row per item, holding all of
    /// that item's problems together, so an upsert replaces the item's
    /// whole state atomically.
    #[col(sql = "JSONB")]
    pub problems: String,
    /// When this problem was first recorded for this uuid (ISO-8601
    /// with explicit offset, per AGENTS.md).
    #[col(sql = "VARCHAR(40)")]
    pub first_seen_at: String,
    /// When it was last re-recorded. Equal to `first_seen_at` on a
    /// problem seen once. Also stamped by the store.
    #[col(sql = "VARCHAR(40)")]
    pub last_seen_at: String,
    /// The `RENDER_VERSION` of the renderer that recorded it, so a row
    /// left by an older renderer is identifiable.
    #[col(sql = "INT")]
    pub render_version: i64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use strum::VariantArray;

    #[test]
    fn sample_truncates_on_a_char_boundary_and_marks_the_cut() {
        assert_eq!(sample_of("short"), "short");
        // 40 two-byte chars = 80 bytes: exactly at the limit, uncut.
        let exact = "é".repeat(40);
        assert_eq!(sample_of(&exact), exact);
        // One more char pushes it over; the result must still be valid
        // UTF-8 (the naive `&s[..80]` would split the last `é`).
        let over = "é".repeat(41);
        let got = sample_of(&over);
        assert!(got.ends_with('…'), "{got}");
        assert!(got.len() <= 80 + '…'.len_utf8());
    }

    #[test]
    fn problems_serialize_without_their_empty_options() {
        let p = Problem::field("when_ts", Reason::CoercionFailed, "not-a-date");
        let j = serde_json::to_string(&p).unwrap();
        assert!(j.contains(r#""reason":"coercion_failed""#), "{j}");
        assert!(
            !j.contains("path"),
            "absent options stay out of the blob: {j}"
        );
        assert!(!j.contains("rule"), "{j}");
    }

    /// `Outcome` and `Reason` are written to SQL and to the problems
    /// blob through two independent derives — strum's `as_str` and
    /// serde. Two spellings of one value would make a sweep miss the
    /// rows it is supposed to clear.
    #[test]
    fn as_str_matches_the_serde_spelling() {
        for &v in Outcome::VARIANTS {
            let json = serde_json::to_string(&v).unwrap();
            assert_eq!(json, format!("\"{}\"", v.as_str()), "{v:?}");
        }
        for &v in Reason::VARIANTS {
            let json = serde_json::to_string(&v).unwrap();
            assert_eq!(json, format!("\"{}\"", v.as_str()), "{v:?}");
        }
        for &v in ScopeKind::VARIANTS {
            assert_eq!(ScopeKind::parse(v.as_str()), Some(v));
        }
        for &v in Stage::VARIANTS {
            assert_eq!(Stage::parse_str(v.as_str()), Some(v));
        }
    }

    #[test]
    fn a_lossy_rule_carries_the_name_r3_groups_by() {
        let p = Problem::lossy("pdf.strip_repeated_chrome", None, "Page 3 of 7");
        assert_eq!(p.reason, Reason::DeliberateLoss);
        assert_eq!(p.rule.as_deref(), Some("pdf.strip_repeated_chrome"));
    }
}
