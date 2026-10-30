//! The environment variables datalib's processes hand each other: the
//! runner to a step, the gateway to an applet, anything to latchkey. One
//! spelling per name, so the side that sets a variable and the side that
//! reads it cannot drift apart silently. No dependencies, so every binary
//! can link it. What each variable carries is in
//! `docs/dev/step_protocol.md` and `docs/dev/applets.md`.

macro_rules! names {
    ($list:ident: $($name:ident),* $(,)?) => {
        $(pub const $name: &str = stringify!($name);)*
        pub const $list: &[&str] = &[$($name),*];
    };
}

names! { STEP_PROTOCOL:
    DATALIB_DAG_STEP,
    DATALIB_DAG_RUN_ID,
    DATALIB_DAG_ATTEMPT,
    DATALIB_DAG_GROUP,
    DATALIB_DAG_GROUP_TYPE,
    DATALIB_DAG_SOURCE_GROUP,
    DATALIB_DAG_SOURCE_GROUP_TYPE,
    DATALIB_DAG_FUNCTION,
    DATALIB_DAG_DATA_ROOT,
    DATALIB_DAG_INPUTS,
    DATALIB_DAG_CHANGED_INPUTS,
    DATALIB_READS,
    DATALIB_DAG_NOW,
    DATALIB_DAG_CHECKPOINT_CADENCE,
    DATALIB_DAG_RENDER_PAGES,
}

names! { APPLET_PROTOCOL:
    DATALIB_APPLET_ID,
    DATALIB_APPLET_BASE,
    DATALIB_APPLET_SECRET,
}

names! { LATCHKEY:
    LATCHKEY_GATEWAY,
}

/// macOS's allocator setting the runner turns to `0` for every step:
/// otherwise it keeps freed large blocks for reuse, and a step that churns
/// big buffers holds gigabytes it no longer uses.
pub const MALLOC_LARGE_CACHE: &str = "MallocLargeCache";

#[cfg(test)]
mod tests {
    use super::*;

    fn step_protocol_doc() -> String {
        const REL: &str = "docs/dev/step_protocol.md";
        let r = runfiles::Runfiles::create().expect("runfiles tree");
        let path = r
            .rlocation(format!("_main/{REL}"))
            .unwrap_or_else(|| panic!("rlocation for {REL}"));
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
    }

    /// The step protocol doc is the contract a third-party step is
    /// written against, so every variable the runner sets is in its
    /// table — and the table names nothing the runner does not set.
    #[test]
    fn the_step_protocol_doc_and_the_runner_name_the_same_variables() {
        let doc = step_protocol_doc();
        let table: std::collections::BTreeSet<&str> = doc
            .lines()
            .filter(|l| l.starts_with("| `DATALIB_"))
            .filter_map(|l| l.trim_start_matches("| `").split('`').next())
            .collect();
        let ours: std::collections::BTreeSet<&str> = STEP_PROTOCOL.iter().copied().collect();
        assert_eq!(
            table, ours,
            "docs/dev/step_protocol.md's variable table and datalib_env::STEP_PROTOCOL disagree"
        );
    }

    /// The macro spells the value from the identifier; a typo in one is a
    /// typo in both, which is the point.
    #[test]
    fn a_name_is_its_own_value() {
        assert_eq!(DATALIB_DAG_STEP, "DATALIB_DAG_STEP");
        assert_eq!(LATCHKEY_GATEWAY, "LATCHKEY_GATEWAY");
        for list in [STEP_PROTOCOL, APPLET_PROTOCOL, LATCHKEY] {
            for name in list {
                assert!(
                    name.bytes().all(|b| b.is_ascii_uppercase() || b == b'_'),
                    "{name}"
                );
            }
        }
    }
}
