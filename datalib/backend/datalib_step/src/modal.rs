//! `embed_on_modal_from_n_chunks`: an `embed` step that owes at least that
//! many chunks sends them to the person's own Modal account, to be
//! embedded on rented GPUs (docs/dev/plans/embed_on_modal.md). The step
//! keeps everything it does locally (what is owed, the prompts, the
//! cache); only the embedding moves. A small driver on the bundled Node
//! (`modal/embed_on_modal.mjs`) runs one Modal Sandbox per slice of
//! prompts, each `datalib-step embed-prompts` in this release's `-cuda`
//! image, and hands back each slice's vectors as it returns.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{bail, Context, Result};
use serde::Deserialize;

/// The step's one parameter, as a person writes it.
pub(crate) const PARAM: &str = "embed_on_modal_from_n_chunks";

/// The driver, run with `node --input-type=module --eval`.
const DRIVER: &str = include_str!("modal/embed_on_modal.mjs");

/// An `embed` step's params.
#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct EmbedParams {
    /// Absent: nothing of the source leaves the machine. Present: a run
    /// that owes at least this many chunks sends all of them to Modal.
    pub(crate) embed_on_modal_from_n_chunks: Option<u64>,
}

pub(crate) fn params(value: serde_json::Value) -> Result<EmbedParams> {
    let value = match value {
        serde_json::Value::Null => serde_json::json!({}),
        v => v,
    };
    serde_json::from_value(value).with_context(|| {
        format!("an `embed` step takes one parameter, `{PARAM}`, a whole number of chunks")
    })
}

/// Where one run's chunks are embedded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Route {
    Here,
    Modal,
}

/// On Modal when the run owes at least the threshold; here otherwise,
/// and always when there is none.
pub(crate) fn route(threshold: Option<u64>, owed: usize) -> Route {
    match threshold {
        Some(n) if owed as u64 >= n => Route::Modal,
        _ => Route::Here,
    }
}

/// What Modal's own tools sign in with: `MODAL_TOKEN_ID` and
/// `MODAL_TOKEN_SECRET`, else the config file `modal setup` writes
/// (`MODAL_CONFIG_PATH`, else `~/.modal.toml`). The values are never read
/// here; the SDK reads them, and says so if a profile holds no token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SignIn {
    pub(crate) token_in_env: bool,
    pub(crate) config_file: Option<PathBuf>,
}

impl SignIn {
    pub(crate) fn of_this_process() -> SignIn {
        let set = |name: &str| std::env::var_os(name).is_some_and(|v| !v.is_empty());
        let config = std::env::var_os("MODAL_CONFIG_PATH")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| Path::new(&h).join(".modal.toml")));
        SignIn {
            token_in_env: set("MODAL_TOKEN_ID") && set("MODAL_TOKEN_SECRET"),
            config_file: config.filter(|p| p.is_file()),
        }
    }

    pub(crate) fn signed_in(&self) -> bool {
        self.token_in_env || self.config_file.is_some()
    }
}

/// The loud failure for a run over the threshold that cannot reach Modal:
/// what to do about it, both ways. Embedding here instead would cost the
/// hours the person chose not to spend.
pub(crate) fn not_signed_in(group: &str, owed: usize, threshold: u64, detail: &str) -> String {
    format!(
        "`{group}/embed` owes {owed} chunks, at least its `{PARAM}` of {threshold}, so they go \
         to Modal, and {detail}. Sign in with Modal's own CLI (`pip install modal`, then \
         `modal setup`, which writes ~/.modal.toml), or give the step MODAL_TOKEN_ID and \
         MODAL_TOKEN_SECRET (under a sync, also name them in DATALIB_CHILD_ENV); or raise \
         `{PARAM}`, or take it out, to embed this source here"
    )
}

/// Overrides for measuring: the image, the GPU and how many Sandboxes run
/// at once. Read by the step, never by a sync's runner.
pub(crate) const IMAGE_ENV: &str = "DATALIB_MODAL_IMAGE";
pub(crate) const GPU_ENV: &str = "DATALIB_MODAL_GPU";
pub(crate) const SANDBOXES_ENV: &str = "DATALIB_MODAL_SANDBOXES";

/// What the driver is asked to do, as it reads it (JSON).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Options {
    /// This release's `-cuda` image.
    pub(crate) image: String,
    /// The cheapest per token at 8 prompts a call (§ "Measured on Modal").
    pub(crate) gpu: String,
    /// The command each Sandbox runs, in the image.
    pub(crate) command: Vec<String>,
    /// The Modal App the Sandboxes are filed under.
    pub(crate) app: String,
    /// Prompts per Sandbox: a minute or two of a T4.
    pub(crate) slice: usize,
    /// Sandboxes at once.
    pub(crate) sandboxes: usize,
    /// One Sandbox's longest life: the image's pull, the model's load,
    /// and a slice of long prompts, with room.
    pub(crate) timeout_ms: u64,
}

impl Options {
    pub(crate) fn for_this_build() -> Options {
        let var = |name: &str| std::env::var(name).ok().filter(|v| !v.is_empty());
        Options {
            image: var(IMAGE_ENV).unwrap_or_else(|| {
                format!(
                    "ghcr.io/imbue-ai/datalib:{}-cuda",
                    datalib_runtime::build_id::DATALIB_VERSION
                )
            }),
            gpu: var(GPU_ENV).unwrap_or_else(|| "T4".to_string()),
            // The model the image bakes in at build time, named rather than
            // found through `HOME`, which a Sandbox may set otherwise.
            command: [
                "/opt/datalib/datalib-step",
                "embed-prompts",
                "--models-dir",
                "/root/.cache/qmd/models",
            ]
            .map(String::from)
            .to_vec(),
            app: "datalib-embed".to_string(),
            slice: 4096,
            sandboxes: var(SANDBOXES_ENV)
                .and_then(|v| v.parse().ok())
                .filter(|n| *n > 0)
                .unwrap_or(10),
            timeout_ms: 15 * 60 * 1000,
        }
    }
}

/// The driver's Node and Modal's SDK, from a runtime tree
/// (`scripts/stage_runtime.sh`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Driver {
    pub(crate) node: PathBuf,
    pub(crate) sdk_entry: PathBuf,
    pub(crate) options: Options,
    /// More variables for the driver, on top of what it is handed: a
    /// test's, for its stand-in SDK. Empty for a sync.
    pub(crate) env: Vec<(String, String)>,
}

impl Driver {
    /// From the runtime this build runs latchkey from: beside the
    /// binaries, or fetched as a release's.
    pub(crate) fn from_runtime() -> Result<Driver> {
        use datalib_runtime::node_runtime as nr;
        // Resolving latchkey puts the runtime in place, fetching it if the
        // install is a tarball's; the tree is the same one.
        nr::latchkey_command().map_err(|e| anyhow::anyhow!("{e}"))?;
        let root = nr::runtime_root().context("no runtime root after a resolution")?;
        Driver::in_tree(&root, Options::for_this_build())
    }

    pub(crate) fn in_tree(root: &Path, options: Options) -> Result<Driver> {
        use datalib_runtime::node_runtime as nr;
        let node = root.join(nr::NODE_REL);
        let sdk_entry = root
            .join("modal")
            .join(nr::MODAL_SDK_VERSION)
            .join(nr::MODAL_SDK_ENTRY_REL);
        if !node.is_file() || !sdk_entry.is_file() {
            bail!(
                "the runtime at {} holds no Modal SDK ({}): a runtime staged before this \
                 build; stage it again with scripts/stage_runtime.sh",
                root.display(),
                sdk_entry.display()
            );
        }
        Ok(Driver {
            node,
            sdk_entry,
            options,
            env: Vec::new(),
        })
    }

    /// The driver, ready to spawn, its pipes open: stdin for prompts,
    /// stdout for vectors, stderr for what it is doing. It is handed only
    /// the environment a child needs and Modal's own sign-in variables.
    pub(crate) fn command(&self) -> Result<tokio::process::Command> {
        let mut cmd = std::process::Command::new(&self.node);
        cmd.args(["--disable-sigusr1", "--input-type=module", "--eval", DRIVER])
            .env(SDK_ENTRY_ENV, &self.sdk_entry)
            .env(
                OPTIONS_ENV,
                serde_json::to_string(&self.options).context("driver options")?,
            );
        datalib_env::inherit_only(&mut cmd, datalib_env::Child::Modal, &[]);
        cmd.envs(self.env.iter().map(|(k, v)| (k, v)));
        let mut cmd = tokio::process::Command::from(cmd);
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        Ok(cmd)
    }
}

/// How the step tells the driver where the SDK is and what to do.
const SDK_ENTRY_ENV: &str = "DATALIB_MODAL_SDK_ENTRY";
const OPTIONS_ENV: &str = "DATALIB_MODAL_OPTIONS";

/// A line of the driver's stdout.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(untagged)]
pub(crate) enum Reply {
    Vector {
        hash: String,
        vector: Vec<f32>,
        cut: bool,
    },
    /// A slice that failed twice: its prompts are still owed.
    Failed { failed: Vec<String>, error: String },
    /// Nothing could start.
    Fatal { fatal: String, sign_in: bool },
}

/// Asks the driver to stop: it ends the Sandboxes it started and exits.
pub(crate) fn interrupt(child: &tokio::process::Child) {
    if let Some(pid) = child.id().and_then(|p| i32::try_from(p).ok()) {
        // SAFETY: a pid this process spawned and has not reaped.
        unsafe { libc::kill(pid, libc::SIGTERM) };
    }
}

/// The input line for one prompt.
pub(crate) fn request_line(hash: &str, prompt: &str) -> String {
    let mut line = serde_json::json!({"hash": hash, "prompt": prompt}).to_string();
    line.push('\n');
    line
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_threshold_routes_a_run_by_what_it_owes() {
        assert_eq!(route(None, 1_000_000), Route::Here);
        assert_eq!(route(Some(5000), 4999), Route::Here);
        assert_eq!(route(Some(5000), 5000), Route::Modal);
        assert_eq!(route(Some(0), 1), Route::Modal);
    }

    #[test]
    fn params_take_a_whole_number_and_nothing_else() {
        assert_eq!(
            params(serde_json::Value::Null).unwrap(),
            EmbedParams::default()
        );
        assert_eq!(
            params(serde_json::json!({PARAM: 5000}))
                .unwrap()
                .embed_on_modal_from_n_chunks,
            Some(5000)
        );
        for bad in [
            serde_json::json!({PARAM: "lots"}),
            serde_json::json!({PARAM: -1}),
            serde_json::json!({PARAM: 2.5}),
            serde_json::json!({"embed_on_modal": 1}),
        ] {
            let err = format!("{:#}", params(bad.clone()).unwrap_err());
            assert!(err.contains(PARAM), "{bad}: {err}");
        }
    }

    #[test]
    fn the_driver_replies_parse_by_shape() {
        let v: Reply = serde_json::from_str(r#"{"hash":"h","vector":[0.5],"cut":false}"#).unwrap();
        assert_eq!(
            v,
            Reply::Vector {
                hash: "h".into(),
                vector: vec![0.5],
                cut: false
            }
        );
        let f: Reply = serde_json::from_str(r#"{"failed":["a"],"error":"boom"}"#).unwrap();
        assert!(matches!(f, Reply::Failed { .. }));
        let x: Reply = serde_json::from_str(r#"{"fatal":"no","sign_in":true}"#).unwrap();
        assert!(matches!(x, Reply::Fatal { sign_in: true, .. }));
    }

    #[test]
    fn sign_in_is_a_token_pair_or_the_config_file() {
        let none = SignIn {
            token_in_env: false,
            config_file: None,
        };
        assert!(!none.signed_in());
        assert!(SignIn {
            token_in_env: true,
            ..none.clone()
        }
        .signed_in());
        assert!(SignIn {
            config_file: Some("/home/u/.modal.toml".into()),
            ..none
        }
        .signed_in());
        let said = not_signed_in(
            "gmail",
            7000,
            5000,
            "this machine is not signed in to Modal",
        );
        for want in [
            "gmail/embed",
            "7000",
            "5000",
            "modal setup",
            "MODAL_TOKEN_ID",
            PARAM,
        ] {
            assert!(said.contains(want), "{want}: {said}");
        }
    }

    #[test]
    fn the_image_is_this_releases_cuda_variant() {
        let o = Options::for_this_build();
        if std::env::var_os(IMAGE_ENV).is_none() {
            assert_eq!(
                o.image,
                format!(
                    "ghcr.io/imbue-ai/datalib:{}-cuda",
                    datalib_runtime::build_id::DATALIB_VERSION
                )
            );
        }
        assert_eq!(
            o.command,
            [
                "/opt/datalib/datalib-step",
                "embed-prompts",
                "--models-dir",
                "/root/.cache/qmd/models"
            ]
        );
    }
}
