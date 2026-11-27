//! What the loop's last tick made of each step, as `steps.state` and
//! `steps.waiting_for` store it. The tick's own `StepState` carries more
//! (which step a waiting one waits on); these are its words.

use strum::{EnumString, IntoStaticStr, VariantArray};

/// The word `system/supervisor.sqlite` stores for a step's state
/// (`steps.state`), without what it waits on or is blocked by.
#[derive(Debug, Clone, Copy, PartialEq, Eq, EnumString, IntoStaticStr, VariantArray)]
#[strum(serialize_all = "snake_case")]
pub enum StateKind {
    /// No open request wants it, and it is up to date.
    Idle,
    /// No open request wants it, and it is out of date.
    Stale,
    /// Wanted, and up to date.
    Fresh,
    Running,
    Off,
    /// Its retries ran out and nothing it reads has moved since.
    Failed,
    /// Out of date, but a producer it reads has never published
    /// anything and is not going to run, so there is nothing to read.
    Blocked,
    Waiting,
}

impl StateKind {
    pub fn as_str(self) -> &'static str {
        self.into()
    }

    /// `None` for a spelling this build does not know.
    pub fn parse(s: &str) -> Option<Self> {
        s.parse().ok()
    }
}

/// What a `waiting` step waits for, as `system/supervisor.sqlite` stores
/// it (`steps.waiting_for`) beside the sentence that names the step or
/// lock.
#[derive(Debug, Clone, Copy, PartialEq, Eq, EnumString, IntoStaticStr, VariantArray)]
#[strum(serialize_all = "snake_case")]
pub enum WaitKind {
    /// A step it reads: running, about to run, or itself waiting on a
    /// producer that is.
    Upstream,
    /// A step that reads what it writes, unpinned, is running.
    Reader,
    Lock,
    /// The data root's disk is under the config's `[disk_space]` floor.
    DiskSpace,
    /// A request names it, and the config the loop runs does not have it
    /// yet: the loop starts using the new one when the sync in progress
    /// ends.
    Config,
    /// The prepare pass at the loop's start has not yet reached it, or a
    /// step it reads.
    Prepare,
}

impl WaitKind {
    pub fn as_str(self) -> &'static str {
        self.into()
    }

    /// `None` for a spelling this build does not know.
    pub fn parse(s: &str) -> Option<Self> {
        s.parse().ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The words are what older stores hold; a rename here would make
    /// every step in them read as a state this build does not know.
    #[test]
    fn the_stored_words_are_the_ones_older_stores_hold() {
        let states: Vec<&str> = StateKind::VARIANTS.iter().map(|k| k.as_str()).collect();
        assert_eq!(
            states,
            ["idle", "stale", "fresh", "running", "off", "failed", "blocked", "waiting"]
        );
        let waits: Vec<&str> = WaitKind::VARIANTS.iter().map(|k| k.as_str()).collect();
        assert_eq!(
            waits,
            [
                "upstream",
                "reader",
                "lock",
                "disk_space",
                "config",
                "prepare"
            ]
        );
        for k in StateKind::VARIANTS {
            assert_eq!(StateKind::parse(k.as_str()), Some(*k));
        }
        assert_eq!(StateKind::parse("a state from a newer build"), None);
    }
}
