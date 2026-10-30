//! `datalib-http` given its data root as a relative path, as a workspace
//! that starts it from its own folder does. Every step runs with the root
//! as its working directory, so a step handed the relative path resolves
//! it a second time and writes into `<root>/<relative path>/…`, where no
//! reader looks, while the sync reports success.

mod server;

use std::path::Path;
use std::time::{Duration, Instant};

use server::start_in;

/// Writes the root and `PATH` it was handed, under the root it was handed.
const CONFIG: &str = r#"
[[groups]]
id = "g"

[[steps]]
group = "g"
function = "out"
command = '''sh -c 'mkdir -p "$DATALIB_DAG_DATA_ROOT/g/out" && printf "%s\n%s\n" "$DATALIB_DAG_DATA_ROOT" "$PATH" > "$DATALIB_DAG_DATA_ROOT/g/out/env"' '''
"#;

#[test]
fn a_relative_root_reaches_each_step_as_an_absolute_path() {
    let dir = tempfile::TempDir::with_prefix("datalib-http-relative-root-").expect("tempdir");
    let rel = Path::new("data/lib");
    let root = dir.path().join(rel);
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("config.toml"), CONFIG).unwrap();

    let server = start_in(dir.path(), rel);
    let answer = server.post_json("/api/requests", r#"{"roots":["g/out"]}"#);
    assert!(answer.starts_with("HTTP/1.1 200"), "{answer}");

    let want = root.join("g/out/env");
    let nested = root.join(rel).join("g/out/env");
    let deadline = Instant::now() + Duration::from_secs(60);
    while !want.exists() {
        assert!(
            !nested.exists(),
            "the step wrote into {}, a copy of the root nested in itself",
            nested.display()
        );
        assert!(
            Instant::now() < deadline,
            "the step never wrote {}",
            want.display()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    let env = std::fs::read_to_string(&want).unwrap();
    let mut lines = env.lines();
    let step_root = lines.next().unwrap_or_default();
    assert!(
        Path::new(step_root).is_absolute(),
        "the step's root: {step_root:?}"
    );
    let library_bin = lines
        .next()
        .unwrap_or_default()
        .split(':')
        .find(|p| p.ends_with("system/bin"))
        .unwrap_or_else(|| panic!("no library bin on the step's PATH: {env}"))
        .to_string();
    assert!(
        Path::new(&library_bin).is_absolute(),
        "the step's PATH names {library_bin:?}"
    );
}
