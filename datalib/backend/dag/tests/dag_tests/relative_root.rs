//! A relative data root reaches every step absolute. A step runs with the
//! root as its working directory, so one handed the relative path resolves
//! it a second time and writes into `<root>/<relative path>/…`, where no
//! reader looks, while the run reports success.

use std::path::Path;
use std::process::Command;

fn dag_bin() -> std::path::PathBuf {
    std::path::absolute(std::env::var("DATALIB_DAG_BIN").expect("DATALIB_DAG_BIN is set"))
        .expect("absolute DATALIB_DAG_BIN")
}

/// Writes the root and `PATH` it was handed, under the root it was handed.
const STEP: &str = r#"
[[groups]]
id = "g"

[[steps]]
group = "g"
function = "out"
command = '''sh -c 'mkdir -p "$DATALIB_DAG_DATA_ROOT/g/out" && printf "%s\n%s\n" "$DATALIB_DAG_DATA_ROOT" "$PATH" > "$DATALIB_DAG_DATA_ROOT/g/out/env"' '''
"#;

#[test]
fn a_relative_data_root_in_the_config_reaches_each_step_absolute() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("data/lib");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(
        root.join("config.toml"),
        format!("data_root = \"data/lib\"\n{STEP}"),
    )
    .unwrap();

    let out = Command::new(dag_bin())
        .current_dir(dir.path())
        .arg("data/lib/config.toml")
        .output()
        .expect("run datalib-dag");
    assert!(
        out.status.success(),
        "datalib-dag: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let nested = root.join("data/lib/g/out/env");
    assert!(
        !nested.exists(),
        "the step wrote into {}, a copy of the root nested in itself",
        nested.display()
    );
    let env = std::fs::read_to_string(root.join("g/out/env")).expect("the step's output");
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
