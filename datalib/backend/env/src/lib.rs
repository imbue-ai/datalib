//! The environment variables datalib's processes hand each other: the
//! runner to a step, the gateway to an applet, anything to latchkey. One
//! spelling per name, so the side that sets a variable and the side that
//! reads it cannot drift apart silently; and the short list of what a
//! child is handed of its parent's own environment ([`inherit_only`]).
//! No dependencies, so every binary can link it. What each variable
//! carries is in `docs/dev/step_protocol.md` and `docs/dev/applets.md`.

pub mod secret_file;

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
    DATALIB_DAG_FAILED_INPUTS,
    DATALIB_READS,
    DATALIB_DAG_NOW,
    DATALIB_DAG_CHECKPOINT_CADENCE,
    DATALIB_DAG_RENDER_PAGES,
}

names! { APPLET_PROTOCOL:
    DATALIB_APPLET_ID,
    DATALIB_APPLET_BASE,
    DATALIB_APPLET_SECRET_FILE,
}

names! { LATCHKEY:
    LATCHKEY_GATEWAY,
    LATCHKEY_ENCRYPTION_KEY,
    LATCHKEY_KEYRING_SERVICE_NAME,
}

/// macOS's allocator setting the runner turns to `0` for every step:
/// otherwise it keeps freed large blocks for reuse, and a step that churns
/// big buffers holds gigabytes it no longer uses.
pub const MALLOC_LARGE_CACHE: &str = "MallocLargeCache";

names! { BUILT_IN_SOURCE_KEYS:
    WHATSAPP_BACKUP_DECRYPTION_KEY,
    SIGNAL_BACKUP_PASSPHRASE,
}

/// Set by the desktop app on the server of a library in the protected
/// folder, whose latchkey store a person's terminal cannot reach. Children
/// are not handed it.
pub const DATALIB_PROTECTED_LIBRARY: &str = "DATALIB_PROTECTED_LIBRARY";

/// Which index the search applet reads Words and Meaning from: `qmd`, the
/// default, or `search_index`, while the two are compared
/// (`docs/dev/plans/search_index_from_ram.md`). Set in the applet's entry.
pub const DATALIB_SEARCH_READS: &str = "DATALIB_SEARCH_READS";

/// Names of more variables every child is handed, comma-separated: for a
/// custom step or applet that reads one of its own, or a test harness.
pub const DATALIB_CHILD_ENV: &str = "DATALIB_CHILD_ENV";

/// A child process, for what of this process's environment it is handed
/// ([`hands_down`]). Nothing else is: the server's API token, a latchkey
/// key meant for latchkey, a key one source reads, a variable a person
/// exported for something else entirely.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Child {
    /// A step: the built-in step program, a package's or a custom one.
    /// Any of them may run `latchkey curl`.
    Step,
    /// An applet the gateway serves, a package's included.
    Applet,
    /// latchkey itself, run by the server's sign-in wizard.
    Latchkey,
}

/// What every child needs to run at all: where programs, home and
/// scratch space are, the locale and zone, the proxy and certificates a
/// network may require, and how this build logs and finds its runtime.
const EVERY_CHILD: &[&str] = &[
    "PATH",
    "HOME",
    "USER",
    "LOGNAME",
    "USERNAME",
    "USERPROFILE",
    "SystemRoot",
    "TMPDIR",
    "TMP",
    "TEMP",
    "TZ",
    "LANG",
    "LANGUAGE",
    "XDG_CACHE_HOME",
    "XDG_CONFIG_HOME",
    "XDG_DATA_HOME",
    "XDG_RUNTIME_DIR",
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "NO_PROXY",
    "ALL_PROXY",
    "http_proxy",
    "https_proxy",
    "no_proxy",
    "all_proxy",
    "SSL_CERT_FILE",
    "SSL_CERT_DIR",
    "CURL_CA_BUNDLE",
    "NODE_EXTRA_CA_CERTS",
    "RUST_LOG",
    "RUST_BACKTRACE",
    "RUST_LIB_BACKTRACE",
    "DATALIB_RUNTIME_DIR",
    "DATALIB_ALLOW_NPX",
    "NPX_BIN",
    "DATALIB_GIT_HASH",
    "DATALIB_POOL_ACQUIRE_SECS",
    "DATALIB_QMD_MODELS_NO_FETCH",
    "DATALIB_NO_FETCH",
    "QMD_LLAMA_GPU",
    // Bazel's, so a child of a test finds its runfiles and keeps out of
    // the host's caches.
    "RUNFILES_DIR",
    "RUNFILES_MANIFEST_FILE",
    "TEST_SRCDIR",
    "TEST_TMPDIR",
];

/// What latchkey reads beyond its own `LATCHKEY_*`: the session bus its
/// keyring is reached on, the display a browser login opens on, the
/// browsers Playwright has installed, and the router curl we hand it.
const LATCHKEY_ALSO: &[&str] = &[
    "DBUS_SESSION_BUS_ADDRESS",
    "DISPLAY",
    "WAYLAND_DISPLAY",
    "XAUTHORITY",
    "PLAYWRIGHT_BROWSERS_PATH",
    "DATALIB_CURL_ROUTER",
];

/// What a step reads beyond what latchkey does: where the fingerprint
/// cache is, replayed HTTP in tests, and the URL prefix a Minds
/// workspace routes some requests through.
const STEP_ALSO: &[&str] = &[
    "DATALIB_CACHE_DIR",
    "DATALIB_HTTP_PLAYBACK",
    "DATALIB_HTTP_PLAYBACK_DELAY_MS",
    "DATALIB_HTTP_PLAYBACK_HOLD",
    "DATALIB_HTTP_PLAYBACK_HOLD_SEALED",
    "DATALIB_HTTP_PLAYBACK_OFFLINE",
    "MINDS_VIA_DESKTOP_URL_PREFIX",
];

/// What an applet reads beyond every child's: the search's pinned now,
/// in tests.
const APPLET_ALSO: &[&str] = &["DATALIB_SEARCH_NOW"];

/// Whether `child` is handed the variable `name` from this process's
/// environment. `also` names more, from wherever the caller learned of
/// them (a step's params, [`DATALIB_CHILD_ENV`]).
pub fn hands_down(name: &str, child: Child, also: &[String]) -> bool {
    let latchkey = |name: &str| name.starts_with("LATCHKEY_") || LATCHKEY_ALSO.contains(&name);
    EVERY_CHILD.contains(&name)
        || name.starts_with("LC_")
        || also.iter().any(|a| a == name)
        || match child {
            Child::Step => latchkey(name) || STEP_ALSO.contains(&name),
            Child::Latchkey => latchkey(name),
            Child::Applet => APPLET_ALSO.contains(&name),
        }
}

/// The names [`DATALIB_CHILD_ENV`] lists.
pub fn child_env_names(value: Option<&str>) -> Vec<String> {
    value
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .map(str::to_string)
        .collect()
}

/// Starts `cmd` from nothing but what [`hands_down`] lets `child` have
/// of this process's environment, plus `also`. What the caller set on
/// `cmd` itself is kept, and wins.
pub fn inherit_only(cmd: &mut std::process::Command, child: Child, also: &[String]) {
    let set: Vec<(std::ffi::OsString, Option<std::ffi::OsString>)> = cmd
        .get_envs()
        .map(|(k, v)| (k.to_owned(), v.map(|v| v.to_owned())))
        .collect();
    let mut also = also.to_vec();
    also.extend(child_env_names(
        std::env::var(DATALIB_CHILD_ENV).ok().as_deref(),
    ));
    cmd.env_clear();
    for (name, value) in std::env::vars_os() {
        if name
            .to_str()
            .is_some_and(|name| hands_down(name, child, &also))
        {
            cmd.env(name, value);
        }
    }
    for (name, value) in set {
        match value {
            Some(value) => cmd.env(name, value),
            None => cmd.env_remove(name),
        };
    }
}

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

    /// A child is handed only what it is listed for: the server's API
    /// token and an unrelated secret reach nobody, latchkey's key reaches
    /// latchkey and steps and never an applet.
    #[test]
    fn a_child_is_handed_only_what_it_is_listed_for() {
        for child in [Child::Step, Child::Applet, Child::Latchkey] {
            assert!(hands_down("PATH", child, &[]));
            assert!(hands_down("LC_ALL", child, &[]));
            assert!(!hands_down("DATALIB_TOKEN", child, &[]));
            assert!(!hands_down("AWS_SECRET_ACCESS_KEY", child, &[]));
            assert!(!hands_down("WHATSAPP_BACKUP_DECRYPTION_KEY", child, &[]));
        }
        assert!(hands_down("LATCHKEY_ENCRYPTION_KEY", Child::Latchkey, &[]));
        assert!(hands_down("LATCHKEY_ENCRYPTION_KEY", Child::Step, &[]));
        assert!(!hands_down("LATCHKEY_ENCRYPTION_KEY", Child::Applet, &[]));
        assert!(!hands_down("DATALIB_HTTP_PLAYBACK", Child::Latchkey, &[]));
        let named = ["MY_BACKUP_KEY".to_string()];
        assert!(hands_down("MY_BACKUP_KEY", Child::Step, &named));
        assert_eq!(
            child_env_names(Some(" A, ,B ")),
            vec!["A".to_string(), "B".to_string()]
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
