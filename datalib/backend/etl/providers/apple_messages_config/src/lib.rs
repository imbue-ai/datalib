//! Provider-owned config schema for the `apple_messages` source: Apple's
//! Messages app, read from its own `chat.db`. Schema-only (serde +
//! anyhow), so the orchestrator can name [`AppleMessagesConfig`] without
//! linking the provider.

use std::path::{Path, PathBuf};

use datalib_source_common::{LocalPath, SourceCommon};
use serde::{Deserialize, Serialize};

/// Tables folded out by [`AppleMessagesConfig::skip_churn`]: the app's
/// counters and key/value settings, the Spotlight and CloudKit work
/// queues, and the cloud-sync tombstones. None of them says anything
/// about a message, and the daemons rewrite them between runs.
pub const CHURN_TABLE_PATTERNS: &[&str] = &[
    "_SqliteDatabaseProperties",
    "kvtable",
    "index_state_metrics",
    "message_processing_task",
    "persistent_tasks",
    "scheduled_messages_pending_cloudkit_delete",
    "sync_*",
    "unsynced_*",
];

/// Columns folded out by [`AppleMessagesConfig::skip_churn`]: Spotlight's
/// per-row indexing state, which moves on every row it visits.
pub const CHURN_COLUMN_PATTERNS: &[&str] = &["*.index_state"];

/// The database's name inside the Messages folder.
pub const DATABASE_IN_FOLDER: &str = "chat.db";

/// The apple_messages-owned slice of an `apple_messages` source. The
/// `messages` table is its one way in; the rest are the mirror engine's
/// knobs, the same ones `apple_photos` exposes.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AppleMessagesConfig {
    /// Shared per-source envelope (paths + cross-source tunables),
    /// resolved by the orchestrator's `normalize()`.
    pub common: SourceCommon,
    /// The Messages folder (`~/Library/Messages`), or a `chat.db` file
    /// directly — a copy of it, or an iPhone backup's `3d0d7e5f…` file,
    /// which is the same database. See [`chat_db_path`].
    pub messages: Option<LocalPath>,
    /// Table-name globs to mirror. Default `["*"]`.
    pub include_tables: Vec<String>,
    /// Table-name globs to skip, applied after [`Self::include_tables`].
    pub exclude_tables: Vec<String>,
    /// `Table.column` globs to drop from the mirror.
    pub exclude_columns: Vec<String>,
    /// Fold [`CHURN_TABLE_PATTERNS`] and [`CHURN_COLUMN_PATTERNS`] into
    /// the exclusions. On by default: with it off, a database nobody
    /// messaged in still commits every run.
    pub skip_churn: bool,
    /// Take a `VACUUM INTO` snapshot before reading. Messages holds the
    /// file open in WAL mode whenever it is running, so this is the
    /// normal path.
    pub snapshot: bool,
    /// Collect unreachable chunks (`dolt_gc()`) at the start of each run.
    pub gc: bool,
}

impl Default for AppleMessagesConfig {
    fn default() -> Self {
        Self {
            common: SourceCommon::default(),
            messages: None,
            include_tables: vec!["*".to_string()],
            exclude_tables: Vec::new(),
            exclude_columns: Vec::new(),
            skip_churn: true,
            snapshot: true,
            gc: false,
        }
    }
}

impl AppleMessagesConfig {
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.include_tables.is_empty() {
            anyhow::bail!("include_tables is empty: nothing would be mirrored");
        }
        Ok(())
    }

    pub fn effective_excluded_tables(&self) -> Vec<String> {
        let mut out = self.exclude_tables.clone();
        if self.skip_churn {
            out.extend(CHURN_TABLE_PATTERNS.iter().map(|s| s.to_string()));
        }
        out
    }

    pub fn effective_excluded_columns(&self) -> Vec<String> {
        let mut out = self.exclude_columns.clone();
        if self.skip_churn {
            out.extend(CHURN_COLUMN_PATTERNS.iter().map(|s| s.to_string()));
        }
        out
    }
}

/// The SQLite file for a configured `messages` path: `chat.db` inside it
/// when it is a folder, the path itself when it is the database. The app
/// has the user pick the folder because on macOS picking grants access
/// to what was picked, and the snapshot has to read `chat.db-wal` and
/// `chat.db-shm` beside the database too.
pub fn chat_db_path(messages: &Path, is_dir: bool) -> PathBuf {
    if is_dir {
        messages.join(DATABASE_IN_FOLDER)
    } else {
        messages.to_path_buf()
    }
}

/// Params for the render step — no provider-specific render knobs, so
/// this is the shared bare envelope.
pub type AppleMessagesRenderConfig = datalib_source_common::BareRenderConfig;

impl datalib_source_common::IngestMethods for AppleMessagesConfig {
    const METHODS: &'static [datalib_source_common::IngestMethod] =
        &[datalib_source_common::IngestMethod::local("messages")];
}

#[cfg(test)]
mod tests {
    use super::*;
    use datalib_source_common::glob_match;

    #[test]
    fn skip_churn_keeps_the_conversation_and_drops_the_daemons() {
        let c = AppleMessagesConfig::default();
        let excluded = c.effective_excluded_tables();
        let wants = |t: &str| !excluded.iter().any(|p| glob_match(p, t));
        for kept in [
            "message",
            "chat",
            "handle",
            "attachment",
            "chat_message_join",
        ] {
            assert!(wants(kept), "{kept}");
        }
        for dropped in [
            "kvtable",
            "sync_deleted_messages",
            "message_processing_task",
        ] {
            assert!(!wants(dropped), "{dropped}");
        }
        assert_eq!(c.effective_excluded_columns(), vec!["*.index_state"]);
        assert!(AppleMessagesConfig {
            skip_churn: false,
            ..Default::default()
        }
        .effective_excluded_columns()
        .is_empty());
    }

    #[test]
    fn folder_or_database_path_both_resolve_to_the_database() {
        let folder = Path::new("/Users/x/Library/Messages");
        assert_eq!(chat_db_path(folder, true), folder.join("chat.db"));
        let backup = Path::new("/tmp/backup/3d/3d0d7e5fb2ce288813306e4d4636395e047a3d28");
        assert_eq!(chat_db_path(backup, false), backup);
    }
}
