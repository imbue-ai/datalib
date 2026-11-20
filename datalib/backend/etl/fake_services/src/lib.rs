//! The fake Slack, Linear and Google of <https://github.com/imbue-ai/fake-services>,
//! run for one test with datalib's own latchkey signed in to them.
//!
//! [`Fakes::start`] stages the pinned launcher and its packages out of the
//! runfiles, serves the seeds it is given on loopback ports, stores each
//! seed's first token in a latchkey store of its own, and points
//! `LATCHKEY_CURL` at the fake's `curl_shim.sh` with routes for the served
//! hosts. From then on every `latchkey curl` a downloader makes reaches a
//! fake at the service's real URL, credential injected by the real latchkey.
//!
//! latchkey reads all of that from the process environment, so a `Fakes`
//! holds a process-wide lock for as long as it lives, and the test binary
//! runs its tests one at a time.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use datalib_runtime::node_runtime::{
    latchkey_command, LATCHKEY_ENTRY_REL, LATCHKEY_VERSION, NODE_REL, RUNTIME_DIR_ENV,
};
use serde_json::Value;
use tempfile::TempDir;

/// What one fake is called, which is also its seed's file name and the
/// `<NAME>_PORT` the launcher reads.
pub const SLACK: &str = "slack";
pub const LINEAR: &str = "linear";
pub const GOOGLE: &str = "google";
const SERVICES: &[&str] = &[SLACK, LINEAR, GOOGLE];

/// Every host of a service the shim sends to its fake, as the shim's
/// patterns; the first is the API host.
fn hosts(service: &str) -> &'static [&'static str] {
    match service {
        SLACK => &["slack.com", "*.slack.com"],
        LINEAR => &["api.linear.app", "uploads.linear.app"],
        GOOGLE => &[
            "gmail.googleapis.com",
            "www.googleapis.com",
            "*.googleapis.com",
        ],
        other => panic!("no such fake service: {other}"),
    }
}

/// The latchkey services a fake's token is stored under, and how each
/// carries it. latchkey knows Google by product, so one Google token is
/// stored twice.
fn latchkey_services(service: &str) -> &'static [(&'static str, &'static str)] {
    match service {
        SLACK => &[("slack", "Authorization: Bearer ")],
        LINEAR => &[("linear", "Authorization: ")],
        GOOGLE => &[
            ("google-gmail", "Authorization: Bearer "),
            ("google-calendar", "Authorization: Bearer "),
        ],
        other => panic!("no such fake service: {other}"),
    }
}

/// Any 32 bytes, base64: the key that opens the test's latchkey store, so
/// no keychain is asked.
const ENCRYPTION_KEY: &str = "a2tra2tra2tra2tra2tra2tra2tra2tra2tra2tra2s=";
const READY_TIMEOUT: Duration = Duration::from_secs(30);
/// How far ahead of the fake's clock an update's newest instant may be
/// before posting it is a mistake rather than a short wait.
const UPDATE_WAIT_LIMIT: Duration = Duration::from_secs(30);

/// How the launcher is told to deviate from the seeds, for a test.
#[derive(Debug, Default, Clone)]
pub struct Launch {
    /// `{"<service>:<bucket>": limit}`: each named rate limit bucket holds
    /// this many in place of the real service's limit, so a test fills it
    /// in a few requests.
    pub rate_limit_by_bucket: BTreeMap<String, u64>,
    /// Every rate limit bucket's window lasts this long, so a test can wait
    /// one out.
    pub rate_limit_window_seconds: Option<u64>,
}

static PROCESS_ENV: Mutex<()> = Mutex::new(());

/// `seed` with the real service's rate limits kept, for a test of them. A
/// test seed otherwise states `"rate_limits": "none"`, since every refusal
/// costs a wait.
pub fn with_real_rate_limits(mut seed: Value) -> Value {
    seed["rate_limits"] = Value::String("real".to_string());
    seed
}

fn runfile(var: &str) -> PathBuf {
    let rel = std::env::var(var).unwrap_or_else(|_| panic!("{var} unset: is it on the test rule?"));
    runfiles::Runfiles::create()
        .expect("runfiles")
        .rlocation(&rel)
        .unwrap_or_else(|| panic!("{var}={rel} not in runfiles"))
}

/// The `node_modules` that links every package of the store a package dir
/// belongs to. A package dir sits inside the store's `.aspect_rules_js/`
/// (`node_modules/.aspect_rules_js/<pkg>@<v>/node_modules/<pkg>`), whose
/// own `node_modules` holds only that package's dependencies; the
/// directory above `.aspect_rules_js/` links them all.
fn store_of(package_dir: &Path) -> PathBuf {
    let mut dir = package_dir;
    while dir.file_name().is_some_and(|n| n != ".aspect_rules_js") {
        dir = dir
            .parent()
            .unwrap_or_else(|| panic!("{} is not inside a pnpm store", package_dir.display()));
    }
    dir.parent()
        .expect("the store's node_modules")
        .to_path_buf()
}

/// Stages the Node and latchkey this test binary runs, once, as
/// `datalib_runtime::node_runtime` lays a runtime out, and names it in
/// `DATALIB_RUNTIME_DIR`. The tree lives for the process.
fn stage_runtime() -> &'static Path {
    static STAGED: OnceLock<PathBuf> = OnceLock::new();
    STAGED.get_or_init(|| {
        let tree = tempfile::tempdir().expect("tempdir").keep();
        let node = tree.join(NODE_REL);
        std::fs::create_dir_all(node.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(runfile("NODE_RLOC"), &node).unwrap();
        let staged = tree.join("latchkey").join(LATCHKEY_VERSION);
        std::fs::create_dir_all(&staged).unwrap();
        std::os::unix::fs::symlink(
            store_of(&runfile("LATCHKEY_PACKAGE_RLOC")),
            staged.join("node_modules"),
        )
        .unwrap();
        assert!(
            staged.join(LATCHKEY_ENTRY_REL).is_file(),
            "latchkey's entry is not under {}",
            staged.display()
        );
        std::env::set_var(RUNTIME_DIR_ENV, &tree);
        tree
    })
}

fn copy_tree(from: &Path, to: &Path) -> Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let target = to.join(entry.file_name());
        if entry.path().is_dir() {
            copy_tree(&entry.path(), &target)?;
        } else {
            std::fs::copy(entry.path(), &target)?;
        }
    }
    Ok(())
}

/// Runs `curl` against the control port and returns the body.
fn curl(args: &[&str]) -> Result<String> {
    let out = Command::new("curl")
        .arg("-sS")
        .args(args)
        .output()
        .context("run curl")?;
    if !out.status.success() {
        bail!(
            "curl {}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr)
        );
    }
    Ok(String::from_utf8(out.stdout)?)
}

/// The fakes of one test, serving until dropped, with the process signed
/// in to them.
pub struct Fakes {
    _lock: MutexGuard<'static, ()>,
    _dir: TempDir,
    launcher: Child,
    ports: BTreeMap<String, u16>,
    access_log: PathBuf,
}

impl Fakes {
    /// Serves one fake per seed, keyed by service name ([`SLACK`],
    /// [`LINEAR`], [`GOOGLE`]), and signs in with each seed's first token.
    pub fn start(seeds: &BTreeMap<&str, Value>) -> Result<Self> {
        Self::start_with(seeds, &Launch::default())
    }

    pub fn start_with(seeds: &BTreeMap<&str, Value>, launch: &Launch) -> Result<Self> {
        // A poisoned lock is a test that panicked, not a reason for the next
        // one to fail.
        let lock = PROCESS_ENV.lock().unwrap_or_else(|e| e.into_inner());
        let runtime = stage_runtime();
        let dir = tempfile::tempdir()?;

        // The launcher, as the box installs it: its directory beside Linear's
        // schema, in a directory whose node_modules is the package store.
        let stage = dir.path().join("fake-services");
        copy_tree(
            runfile("FAKE_SERVICES_LAUNCHER_RLOC").parent().unwrap(),
            &stage.join("launcher"),
        )?;
        std::fs::copy(
            runfile("FAKE_SERVICES_SCHEMA_RLOC"),
            stage.join("linear_schema.graphql.gz"),
        )?;
        std::os::unix::fs::symlink(
            store_of(&runfile("FAKE_SERVICES_PACKAGE_RLOC")),
            stage.join("node_modules"),
        )?;
        let seed_dir = dir.path().join("seeds");
        std::fs::create_dir(&seed_dir)?;
        for (service, seed) in seeds {
            assert!(
                SERVICES.contains(service),
                "no such fake service: {service}"
            );
            std::fs::write(
                seed_dir.join(format!("{service}.json")),
                serde_json::to_vec_pretty(seed)?,
            )?;
        }
        let access_log = dir.path().join("access_log.jsonl");
        let mut command = Command::new(runtime.join(NODE_REL));
        command
            .arg(stage.join("launcher/launcher.mjs"))
            .env("SEED_DIR", &seed_dir)
            .env("CONTROL_PORT", "0")
            .env("ACCESS_LOG", &access_log)
            .env(
                "RATE_LIMIT_BUCKET_LIMITS",
                serde_json::to_string(&launch.rate_limit_by_bucket)?,
            )
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for service in SERVICES {
            command.env(format!("{}_PORT", service.to_uppercase()), "0");
        }
        if let Some(seconds) = launch.rate_limit_window_seconds {
            command.env("RATE_LIMIT_WINDOW_SECONDS", seconds.to_string());
        }
        let mut launcher = command
            .spawn()
            .context("start the fake-services launcher")?;
        let ports = match ready_ports(&mut launcher) {
            Ok(ports) => ports,
            Err(e) => {
                let _ = launcher.kill();
                let out = launcher.wait_with_output()?;
                bail!("{e}: {}", String::from_utf8_lossy(&out.stderr));
            }
        };

        // The shim, with routes for every served service's hosts and no
        // router: the system curl makes the request.
        let shim_dir = dir.path().join("shim");
        std::fs::create_dir(&shim_dir)?;
        let shim = shim_dir.join("curl_shim.sh");
        std::fs::copy(runfile("FAKE_SERVICES_SHIM_RLOC"), &shim)?;
        let mut routes = String::new();
        for service in seeds.keys() {
            for host in hosts(service) {
                routes.push_str(&format!("{host} {}\n", ports[*service]));
            }
        }
        std::fs::write(shim_dir.join("curl_routes"), routes)?;

        // A latchkey store of this test's own, and a HOME so nothing of the
        // developer's is read or written.
        let store = dir.path().join("latchkey");
        let home = dir.path().join("home");
        std::fs::create_dir_all(&store)?;
        std::fs::create_dir_all(&home)?;
        std::env::set_var("HOME", &home);
        std::env::set_var("LATCHKEY_DIRECTORY", &store);
        std::env::set_var("LATCHKEY_ENCRYPTION_KEY", ENCRYPTION_KEY);
        std::env::set_var("LATCHKEY_CURL", &shim);
        for (service, seed) in seeds {
            let token = seed["tokens"][0]["token"]
                .as_str()
                .ok_or_else(|| anyhow!("the {service} seed names no token"))?;
            for (latchkey_service, prefix) in latchkey_services(service) {
                let out = latchkey_command()
                    .map_err(|e| anyhow!("{e}"))?
                    .args(["auth", "set", latchkey_service, "-H"])
                    .arg(format!("{prefix}{token}"))
                    .output()
                    .context("run latchkey auth set")?;
                if !out.status.success() {
                    bail!(
                        "latchkey auth set {latchkey_service} {}: {}",
                        out.status,
                        String::from_utf8_lossy(&out.stderr)
                    );
                }
            }
        }
        Ok(Self {
            _lock: lock,
            _dir: dir,
            launcher,
            ports,
            access_log,
        })
    }

    fn control_url(&self, route: &str) -> String {
        format!(
            "http://127.0.0.1:{}/_control/{route}",
            self.ports["control"]
        )
    }

    /// The launcher's health: its record counts, `counts.requests` per
    /// service, `unsupported_calls`, `rate_limited` and `faulted`.
    pub fn health(&self) -> Value {
        serde_json::from_str(&curl(&[&self.control_url("health")]).expect("health")).expect("JSON")
    }

    /// How many requests have reached the fake of `service`.
    pub fn requests(&self, service: &str) -> u64 {
        self.health()["counts"]["requests"][service]
            .as_u64()
            .unwrap_or(0)
    }

    /// Every call the fakes did not support, by what was asked.
    pub fn unsupported_calls(&self) -> BTreeMap<String, u64> {
        serde_json::from_value(self.health()["unsupported_calls"].clone()).expect("counts")
    }

    /// Adds an update to a served fake, following the step named by
    /// `after` (which its relative timestamps count from); the launcher's
    /// answer, or the refusal's text. An update whose newest instant is
    /// still ahead of the fake's clock is refused with how long to wait,
    /// and is posted again once that has passed.
    pub fn add_records(&self, service: &str, update: &Value, after: Option<&str>) -> Result<Value> {
        let url = match after {
            Some(step) => format!(
                "{}?after={step}",
                self.control_url(&format!("records/{service}"))
            ),
            None => self.control_url(&format!("records/{service}")),
        };
        let body = serde_json::to_string(update)?;
        let deadline = Instant::now() + UPDATE_WAIT_LIMIT;
        loop {
            let text = curl(&[
                "-X",
                "POST",
                "-H",
                "Content-Type: application/json",
                "--data-binary",
                &body,
                &url,
            ])?;
            let answer: Value = serde_json::from_str(&text).context("the launcher's answer")?;
            if answer["ok"] == Value::Bool(true) {
                return Ok(answer);
            }
            match answer["retry_after_seconds"].as_u64() {
                Some(seconds) if Instant::now() + Duration::from_secs(seconds) <= deadline => {
                    std::thread::sleep(Duration::from_secs(seconds));
                }
                _ => bail!("the {service} fake refused the update: {text}"),
            }
        }
    }

    /// Marks the phase every request from now on is logged in.
    pub fn mark_phase(&self, name: &str) -> Result<()> {
        let body = serde_json::json!({"name": name}).to_string();
        let text = curl(&[
            "-X",
            "POST",
            "-H",
            "Content-Type: application/json",
            "--data-binary",
            &body,
            &self.control_url("phase"),
        ])?;
        let answer: Value = serde_json::from_str(&text)?;
        anyhow::ensure!(
            answer["ok"] == Value::Bool(true),
            "the phase mark was refused: {text}"
        );
        Ok(())
    }

    /// Stores `header` as the process's credential for `latchkey_service`
    /// in place of the seed's token: how a test signs in wrong.
    pub fn sign_in_as(&self, latchkey_service: &str, header: &str) -> Result<()> {
        let out = latchkey_command()
            .map_err(|e| anyhow!("{e}"))?
            .args(["auth", "set", latchkey_service, "-H", header])
            .output()
            .context("run latchkey auth set")?;
        anyhow::ensure!(
            out.status.success(),
            "latchkey auth set {latchkey_service} {}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr)
        );
        Ok(())
    }

    /// The access-log lines of the requests made in `phase`.
    pub fn requests_in_phase(&self, phase: &str) -> Vec<Value> {
        self.access_log()
            .into_iter()
            .filter(|line| line["phase"] == phase)
            .collect()
    }

    /// Every request that reached a fake so far, one access-log line each.
    pub fn access_log(&self) -> Vec<Value> {
        std::fs::read_to_string(&self.access_log)
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str(line).expect("an access log line is JSON"))
            .collect()
    }

    /// The access-log lines of `service` whose URL contains `needle`.
    pub fn requests_matching(&self, service: &str, needle: &str) -> Vec<Value> {
        self.access_log()
            .into_iter()
            .filter(|line| {
                line["service"] == service
                    && line["url"].as_str().is_some_and(|u| u.contains(needle))
            })
            .collect()
    }
}

// A test harness has no progress display for a line on stderr to corrupt.
#[allow(clippy::disallowed_macros)]
impl Drop for Fakes {
    fn drop(&mut self) {
        // A failing test's diagnosis usually starts with what the fakes
        // refused or did not support, so a panic prints those lines.
        if std::thread::panicking() {
            for line in self.access_log() {
                if line["status"] != 200
                    || !line["unsupported"].is_null()
                    || !line["fault"].is_null()
                {
                    eprintln!("fake answered {line}");
                }
            }
            eprintln!("fake health {}", self.health());
        }
        let _ = self.launcher.kill();
        let _ = self.launcher.wait();
        for var in [
            "LATCHKEY_DIRECTORY",
            "LATCHKEY_ENCRYPTION_KEY",
            "LATCHKEY_CURL",
        ] {
            std::env::remove_var(var);
        }
    }
}

/// The ports the launcher names on its one stdout line, or why it never did.
fn ready_ports(launcher: &mut Child) -> Result<BTreeMap<String, u16>> {
    let stdout = launcher.stdout.take().expect("piped stdout");
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if line.trim_start().starts_with('{') {
                let _ = sender.send(line);
                return;
            }
        }
    });
    let deadline = Instant::now() + READY_TIMEOUT;
    loop {
        match receiver.recv_timeout(Duration::from_millis(50)) {
            Ok(line) => {
                let ready: Value = serde_json::from_str(&line).context("the ready line")?;
                return serde_json::from_value(ready["ports"].clone())
                    .context("the ready line's ports");
            }
            Err(_) if launcher.try_wait()?.is_some() => {
                bail!("the launcher exited before it served")
            }
            Err(_) if Instant::now() > deadline => bail!("the launcher did not serve in time"),
            Err(_) => {}
        }
    }
}
