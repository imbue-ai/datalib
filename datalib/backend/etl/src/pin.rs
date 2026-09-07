//! Which commit a doltlite store is read at.
//!
//! A plain `SELECT` reads doltlite's working set, which lives in the file and
//! is shared across processes, so it can return rows a writer has not
//! committed yet. A consumer reading a store whose producer is still running
//! must name a commit in every content query instead — `dolt_at_<table>(…)`
//! — and a [`Pin`] is that commit. Query text goes through
//! [`Pin::sql`], which is the one place the substitution happens and the one
//! place the SQL-safety assertion is made.
//!
//! See `docs/dev/streaming_steps_plan.md` for what this is for.

use anyhow::{bail, Result};

/// The commit a store is read at.
///
/// **Holds a full commit hash and nothing else.** Not `HEAD`, deliberately:
/// `HEAD` resolves when the query runs rather than when the pin was taken, so
/// a pin that could carry it would let one pass's diff and its content reads
/// name two different commits — which is the exact race streaming introduces.
/// Making that unrepresentable is most of what this type is for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Pin {
    /// Read the working set. Correct only when nothing else writes the file,
    /// which is every consumer today and no streaming consumer.
    Unpinned,
    At(String),
}

/// Doltlite commit hashes are 40 lowercase hex characters, and the engine
/// rejects a shortened prefix (`ref not found`), so there is no shorter form
/// to accept.
const HASH_LEN: usize = 40;

impl Pin {
    pub fn at(commit: impl Into<String>) -> Result<Pin> {
        let commit = commit.into();
        if commit.len() != HASH_LEN
            || !commit
                .bytes()
                .all(|b| b.is_ascii_digit() || b.is_ascii_lowercase() && b <= b'f')
        {
            bail!(
                "not a doltlite commit hash: {commit:?} \
                 (want {HASH_LEN} lowercase hex characters)"
            );
        }
        Ok(Pin::At(commit))
    }

    /// The pin a scan produced, where `None` is a store the scan could not
    /// name a commit for — no commits yet, or no dolt extensions at all.
    pub fn from_scan(commit: Option<&str>) -> Result<Pin> {
        match commit {
            Some(c) => Pin::at(c),
            None => Ok(Pin::Unpinned),
        }
    }

    pub fn commit(&self) -> Option<&str> {
        match self {
            Pin::Unpinned => None,
            Pin::At(c) => Some(c),
        }
    }

    pub fn is_pinned(&self) -> bool {
        matches!(self, Pin::At(_))
    }

    /// Expand `{table}` placeholders in a query template and vouch for the
    /// result.
    ///
    /// The `AssertSqlSafe` is made here, once, rather than at every callsite,
    /// because this is where it can be justified: `template` is a `&'static
    /// str` at every callsite, the placeholder must be a plain table name (or
    /// this panics), and the only interpolated value is a hash
    /// [`Pin::at`] already validated as 40 hex characters. Nothing that
    /// reached this function came from upstream data.
    pub fn sql(&self, template: &'static str) -> sqlx::AssertSqlSafe<String> {
        sqlx::AssertSqlSafe(self.expand(template))
    }

    fn expand(&self, template: &'static str) -> String {
        let mut out = String::with_capacity(template.len());
        let mut rest = template;
        while let Some(open) = rest.find('{') {
            out.push_str(&rest[..open]);
            let after = &rest[open + 1..];
            let Some(close) = after.find('}') else {
                panic!("unclosed {{ in query template: {template:?}");
            };
            let table = &after[..close];
            assert!(
                is_table_name(table),
                "{{{table}}} is not a table name, in query template: {template:?}"
            );
            match self {
                Pin::Unpinned => out.push_str(table),
                Pin::At(commit) => {
                    out.push_str("dolt_at_");
                    out.push_str(table);
                    out.push_str("('");
                    out.push_str(commit);
                    out.push_str("')");
                }
            }
            rest = &after[close + 1..];
        }
        out.push_str(rest);
        out
    }
}

fn is_table_name(s: &str) -> bool {
    !s.is_empty()
        && s.bytes()
            .next()
            .is_some_and(|b| b.is_ascii_lowercase() || b == b'_')
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

#[cfg(test)]
mod tests {
    use super::*;

    const HASH: &str = "cb290c9a12e6e5c1053568c864ab582adbf42743";

    #[test]
    fn a_pinned_template_names_the_commit_in_every_placeholder() {
        let pin = Pin::at(HASH).unwrap();
        assert_eq!(
            pin.expand("SELECT a.v FROM {users} a JOIN {channels} b ON b.id = a.id"),
            format!(
                "SELECT a.v FROM dolt_at_users('{HASH}') a \
                 JOIN dolt_at_channels('{HASH}') b ON b.id = a.id"
            )
        );
    }

    /// The cold-start path: no commit to pin to, so the query has to name the
    /// bare table. `dolt_at_<t>` does not exist before a table's first commit
    /// — it is `no such table`, not an empty result — so substituting it
    /// anyway would turn "nothing to do" into a hard error.
    #[test]
    fn an_unpinned_template_reads_the_bare_table() {
        assert_eq!(
            Pin::Unpinned.expand("SELECT * FROM {markdowns} ORDER BY markdown_uuid"),
            "SELECT * FROM markdowns ORDER BY markdown_uuid"
        );
    }

    /// `HEAD` resolves when the query runs, so a pin holding it would let a
    /// consumer's diff and its content reads name different commits. The type
    /// refuses it, which is how that race is made unrepresentable rather than
    /// merely discouraged.
    #[test]
    fn only_a_full_commit_hash_is_a_pin() {
        assert!(Pin::at(HASH).is_ok());
        for bad in [
            "HEAD",
            "HEAD~1",
            "cb290c9a",
            "",
            "CB290C9A12E6E5C1053568C864AB582ADBF42743",
            "cb290c9a12e6e5c1053568c864ab582adbf4274g",
            "cb290c9a12e6e5c1053568c864ab582adbf427433",
        ] {
            assert!(Pin::at(bad).is_err(), "{bad:?} was accepted as a pin");
        }
    }

    #[test]
    fn a_scan_that_named_no_commit_is_unpinned() {
        assert_eq!(Pin::from_scan(None).unwrap(), Pin::Unpinned);
        assert!(Pin::from_scan(Some(HASH)).unwrap().is_pinned());
        assert!(Pin::from_scan(Some("nonsense")).is_err());
    }

    /// A template with no placeholders is left exactly alone, so a query that
    /// reads only `dolt_*` vtabs can go through the same helper.
    #[test]
    fn a_template_without_placeholders_is_unchanged() {
        let sql = "SELECT commit_hash FROM dolt_log() ORDER BY date DESC LIMIT 1";
        assert_eq!(Pin::at(HASH).unwrap().expand(sql), sql);
    }

    #[test]
    #[should_panic(expected = "is not a table name")]
    fn a_placeholder_that_is_not_a_table_name_panics() {
        Pin::Unpinned.expand("SELECT * FROM {users; DROP TABLE x}");
    }

    /// The whole premise, end to end through sqlx rather than through the
    /// doltlite shell: against a store with a dirty working set, an unpinned
    /// read sees the uncommitted rows and a pinned one does not. If this ever
    /// fails, streaming consumers are reading torn data and the scheduler
    /// change has to come back out.
    #[tokio::test]
    async fn a_pinned_read_ignores_the_working_set() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pin_test.doltlite_db");
        let pool = crate::doltlite_raw::open(
            &path,
            &["CREATE TABLE IF NOT EXISTS notes (id TEXT PRIMARY KEY, body TEXT)"],
        )
        .await
        .unwrap();
        if !crate::doltlite_raw::has_dolt_extensions(&pool).await {
            return; // stock libsqlite3 dev build: no dolt_at_ to test.
        }

        for id in ["a", "b"] {
            sqlx::query("INSERT INTO notes (id, body) VALUES (?, 'x')")
                .bind(id)
                .execute(&pool)
                .await
                .unwrap();
        }
        let committed = crate::doltlite_raw::commit_run(&pool, "two notes")
            .await
            .unwrap()
            .expect("a commit hash");
        // Dirty the working set the way a producer mid-run would.
        sqlx::query("INSERT INTO notes (id, body) VALUES ('c', 'x')")
            .execute(&pool)
            .await
            .unwrap();

        let pin = Pin::at(&committed).expect("dolt_log's hash is a valid pin");
        let count = |p: Pin| {
            let pool = pool.clone();
            async move {
                sqlx::query_scalar::<_, i64>(p.sql("SELECT COUNT(*) FROM {notes}"))
                    .fetch_one(&pool)
                    .await
                    .unwrap()
            }
        };
        assert_eq!(count(Pin::Unpinned).await, 3, "the working set holds three");
        assert_eq!(count(pin).await, 2, "the pin holds only what was committed");
    }
}
