// What render could not do, kept beside what it did.
//
// One row per *item* the render stage had something to say about, in
// the same `indexed_markdown.doltlite_db` as the rows that item
// produced. Co-located deliberately: a document's rows and the record
// of what was dropped or nulled getting them there commit in one
// transaction, so they can never disagree about which run they came
// from.
//
// The rules this implements are R1-R4 in
// `docs/dev/data_architecture_parse_and_render.md` §4. The shape it
// implements is §4 of
// `docs/dev/data_lib_as_a_library/render_audit_2026_09_03.md`, which
// measured why the render stage had nowhere to put a problem:
// `RunCtx::for_render` drops both the metrics and the diagnostics
// buffer, so a renderer that noticed a bad record could crash the step
// or silently substitute a plausible value, and nothing else.
//
// Hand-written row struct; the `CREATE TABLE` DDL + column metadata are
// derived from it by `#[derive(PortableTable)]`.

use datalib_etl_macros::PortableTable;
use serde::{Deserialize, Serialize};

/// What happened to the record this row is about.
///
/// **Rendering and reporting are not alternatives.** The common case is
/// not "the row was dropped" — it is "the row is in the grid, and one
/// of its fields was discarded getting it there". A design that only
/// reported on failure could not count a rule like pdf's
/// `strip_repeated_chrome`, which produces a perfectly good document
/// *and* throws source content away, and never fails.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
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
        match self {
            Outcome::Dropped => "dropped",
            Outcome::Nulled => "nulled",
            Outcome::Ok => "ok",
        }
    }
}

/// Why one field or record is being reported.
///
/// The first four are R1's taxonomy verbatim, so this enum *is* the
/// taxonomy rather than a place where it is re-described. The last two
/// are the cases R1 does not cover: a deliberate lossy rule (R3), and
/// an observation that discarded nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
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
        match self {
            Reason::Undeserializable => "undeserializable",
            Reason::NoIdentity => "no_identity",
            Reason::CoercionFailed => "coercion_failed",
            Reason::UncoveredType => "uncovered_type",
            Reason::DeliberateLoss => "deliberate_loss",
            Reason::Noted => "noted",
        }
    }
}

/// The first 80 characters of an offending value.
///
/// R1: "never a count without a reason, never a reason without a
/// sample." Truncated on a char boundary so the result is still valid
/// UTF-8, and marked when cut so a reader can tell a short value from a
/// clipped one.
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
    ///
    /// ```sql
    /// SELECT rule, COUNT(*) FROM render_problems, json_each(problems)
    ///  WHERE rule IS NOT NULL GROUP BY rule;
    /// ```
    ///
    /// R3's condition for a lossy rule being allowed at all is that we
    /// can generate its count. This is how.
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

    /// A record-level problem: nothing could be keyed or deserialized,
    /// so no single field is to blame.
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

    /// Attach a JSON pointer into the stored payload.
    pub fn at(mut self, path: impl Into<String>) -> Self {
        self.path = Some(path.into());
        self
    }
}

/// One item the render stage had something to say about.
///
/// Keyed on the item's own `uuid` — the same value its `grid_rows` row
/// carries — so the two can be joined: "show me every row that lost a
/// field" is one query, and the grid can mark a cell degraded. A schema
/// keyed on run-and-sequence could not answer that.
///
/// Rows are **overwritten or removed** as the underlying problem
/// changes, rather than accumulated. See
/// `datalib_etl::indexed_markdown::IndexedMarkdownStore` for the sweep
/// rule, which is the subtle part: render is incremental, so "not re-emitted this
/// run" overwhelmingly means "not looked at", not "fixed".
#[derive(Debug, Clone, Serialize, Deserialize, PortableTable)]
#[portable_table(table = "render_problems", primary_key = "uuid")]
pub struct RenderProblemRow {
    /// The item this is about — normally the `grid_rows.uuid` the
    /// record would have produced.
    ///
    /// A record with **no usable identity** has no uuid to key on, so
    /// it gets a content-derived surrogate, `"noid:" ||
    /// blake3(source_name ‖ stage ‖ payload)[..16]`. The prefix keeps
    /// those sortable-apart and obvious in the UI, and the content hash
    /// means the same bad record does not accumulate a new row every
    /// run.
    #[col(sql = "VARCHAR(96)")]
    pub uuid: String,
    /// What must be reprocessed for this row to be re-evaluated: the
    /// `markdown_uuid` of the document the record belongs to, or — when
    /// the failure happened before we knew that — the raw-store entity
    /// id. This is the sweep key.
    #[col(sql = "VARCHAR(96)")]
    pub scope_key: String,
    /// `markdown` or `entity`, saying which of the two `scope_key` is.
    #[col(sql = "VARCHAR(16)")]
    pub scope_kind: String,
    /// The source that produced this, matching `markdowns.source_name`.
    #[col(sql = "VARCHAR(64)")]
    pub source_name: String,
    /// `parse`, `render`, or `grid_row` — which half of the projection
    /// noticed. Distinguishes "the payload would not deserialize" from
    /// "the row would not validate".
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
    ///
    /// **Stamped by the store, not by the renderer.** A renderer
    /// building a row has no way to know whether this problem is new —
    /// it would have to set `first_seen_at = last_seen_at = now` every
    /// run, which quietly destroys the only thing the column is for.
    /// `IndexedMarkdownStore::sweep_problems` carries the existing
    /// value forward when a uuid comes back, so leave this empty and
    /// let the write path fill it.
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

    #[test]
    fn a_lossy_rule_carries_the_name_r3_groups_by() {
        let p = Problem::lossy("pdf.strip_repeated_chrome", None, "Page 3 of 7");
        assert_eq!(p.reason, Reason::DeliberateLoss);
        assert_eq!(p.rule.as_deref(), Some("pdf.strip_repeated_chrome"));
    }
}
