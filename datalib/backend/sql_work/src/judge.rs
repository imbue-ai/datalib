//! What a finished statement did, and whether that is worth a line.

/// The counters SQLite keeps for one run of one statement, and the rows it
/// answered with.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Work {
    /// Every instruction SQLite's engine ran: the one number that grows
    /// with whatever the statement read, whether by a scan or by an index.
    pub vm_steps: u64,
    /// Rows walked by a loop with no constraint: a whole table or index.
    pub fullscan_steps: u64,
    /// Rows put into an index SQLite built for this run because none fit.
    pub autoindex_rows: u64,
    pub sorts: u64,
    /// Rows returned, or for a write, the rows it changed if more.
    pub rows: u64,
    pub nanos: u64,
}

/// Below this a statement is cheap whatever it answered. SQLite spends
/// about three steps on each row it walks, so this is some 30,000 rows.
pub const HEAVY_VM_STEPS: u64 = 100_000;

/// A statement spending this many steps per row it answers walked some
/// thirty rows for each one it kept.
pub const STEPS_PER_ROW: u64 = 100;

impl Work {
    /// Much work for what it answered: the shape of every missing or
    /// mispicked index, scan or search alike. A statement that answers with
    /// a million rows reads a million rows and is not heavy.
    pub fn is_heavy(&self) -> bool {
        self.vm_steps >= HEAVY_VM_STEPS && self.vm_steps / self.rows.max(1) >= STEPS_PER_ROW
    }
}

/// In a statement's text, says it walks many rows for few because that is
/// its job (a retention cap has to count what it keeps), so its work is no
/// sign of a missing index. Only for what runs once per open, never for an
/// answer somebody waits for.
pub const BY_DESIGN: &str = "/* walks by design */";

pub fn walks_by_design(sql: &str) -> bool {
    sql.contains(BY_DESIGN)
}

/// Whether a statement's work says anything about how it reads. A
/// `CREATE INDEX` or a `PRAGMA` does its work by design.
pub fn reads_or_writes_rows(sql: &str) -> bool {
    matches!(
        first_word(sql).as_str(),
        "SELECT" | "WITH" | "INSERT" | "REPLACE" | "UPDATE" | "DELETE" | "VALUES"
    )
}

/// Whether a statement can change rows, so the connection's count of
/// changes is its own.
pub fn changes_rows(sql: &str) -> bool {
    matches!(
        first_word(sql).as_str(),
        "WITH" | "INSERT" | "REPLACE" | "UPDATE" | "DELETE"
    )
}

/// The statement's first word, after its name, uppercased.
fn first_word(sql: &str) -> String {
    datalib_sql::tag::after_name(sql)
        .trim_start_matches(|c: char| c.is_whitespace() || c == '(')
        .chars()
        .take_while(|c| c.is_ascii_alphabetic())
        .collect::<String>()
        .to_ascii_uppercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn work(vm_steps: u64, rows: u64) -> Work {
        Work {
            vm_steps,
            rows,
            ..Work::default()
        }
    }

    /// Guards the Library card's node query (#1926): an index search that
    /// walked every document of the grid, 5.8M steps, to answer nothing.
    /// It scanned no table, so only the steps per row can see it.
    #[test]
    fn a_search_that_reads_everything_for_nothing_is_heavy() {
        assert!(work(5_800_000, 0).is_heavy());
        assert!(work(5_800_000, 1).is_heavy());
        assert!(work(900_000, 1).is_heavy(), "a scan of 300k rows for one");
    }

    #[test]
    fn reading_many_rows_to_return_them_is_not_heavy() {
        assert!(!work(30_000_000, 1_000_000).is_heavy());
        assert!(!work(99_999, 0).is_heavy(), "cheap whatever it answered");
    }

    #[test]
    fn only_statements_over_rows_are_judged() {
        assert!(reads_or_writes_rows("\n  SELECT 1"));
        assert!(reads_or_writes_rows("with m as (select 1) select * from m"));
        assert!(reads_or_writes_rows("(SELECT 1) UNION SELECT 2"));
        assert!(reads_or_writes_rows("DELETE FROM t"));
        assert!(reads_or_writes_rows("/* sql:runs.count_tables */ SELECT 1"));
        assert!(!reads_or_writes_rows("CREATE INDEX i ON t (a)"));
        assert!(!reads_or_writes_rows("PRAGMA incremental_vacuum"));
        assert!(!reads_or_writes_rows("COMMIT"));
    }

    #[test]
    fn only_a_write_takes_the_connections_count_of_changes() {
        assert!(changes_rows("/* sql:a.b */ DELETE FROM t"));
        assert!(changes_rows("INSERT INTO t VALUES (1)"));
        assert!(!changes_rows(
            "/* sql:a.b */ SELECT 1 FROM sqlite_master LIMIT 1"
        ));
        assert!(!changes_rows("PRAGMA user_version"));
    }

    #[test]
    fn only_a_marked_statement_walks_by_design() {
        assert!(walks_by_design(
            "DELETE FROM log /* walks by design */ WHERE a"
        ));
        assert!(!walks_by_design("DELETE FROM log WHERE a"));
        assert!(!walks_by_design("DELETE FROM log /* keep the newest */"));
    }
}
