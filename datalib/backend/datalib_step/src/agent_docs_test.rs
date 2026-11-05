//! The examples in the docs `datalib-step help` prints, held to what this
//! binary and the runner accept: every TOML block loads and plans as a
//! config unless it says it is a `fragment`, every `datalib-step` and
//! `datalib-dag` command in their shell blocks parses, and every
//! `datalib-step help …` they name is a page.

use std::collections::BTreeSet;

use datalib_agent_docs::{fenced_blocks, Kind, DOCS};

/// The info-string word that marks a TOML block as part of a config (a
/// params table, a top-level setting) rather than one that loads alone.
const FRAGMENT: &str = "fragment";

fn markdown_docs() -> impl Iterator<Item = &'static datalib_agent_docs::Doc> {
    DOCS.iter().filter(|d| d.kind != Kind::Config)
}

// The config a block is, with a stand-in step for every input it names and
// does not declare, so a stanza that reads a source (`inputs =
// ["gmail/ingest"]`) loads without that source's whole stanza.
fn config_for(name: &str, block: &str) -> String {
    let table: toml::Table =
        toml::from_str(block).unwrap_or_else(|e| panic!("{name}: not TOML: {e}"));
    let steps = table
        .get("steps")
        .and_then(|s| s.as_array())
        .cloned()
        .unwrap_or_default();
    let declared: BTreeSet<String> = steps
        .iter()
        .filter_map(|s| match (s.get("id"), s.get("group"), s.get("function")) {
            (Some(id), ..) => id.as_str().map(str::to_string),
            (None, Some(g), Some(f)) => Some(format!("{}/{}", g.as_str()?, f.as_str()?)),
            _ => None,
        })
        .collect();
    let read: BTreeSet<String> = steps
        .iter()
        .filter_map(|s| s.get("inputs")?.as_array().cloned())
        .flatten()
        .filter_map(|i| i.as_str().map(str::to_string))
        .collect();
    let mut config = block.to_string();
    for input in read.difference(&declared) {
        config.push_str(&format!(
            "\n[[steps]]\nid = \"{input}\"\ncommand = \"true\"\n"
        ));
    }
    config
}

#[test]
fn every_toml_block_in_the_docs_loads_and_plans_unless_it_is_a_fragment() {
    let mut configs = 0;
    for d in markdown_docs() {
        for block in fenced_blocks(d.text) {
            let mut info = block.info.split_whitespace();
            if info.next() != Some("toml") {
                continue;
            }
            let name = format!("{}:{}", d.path, block.line);
            match info.next() {
                None => {
                    let config = config_for(&name, &block.body);
                    let dir = tempfile::tempdir().expect("tempdir");
                    let path = dir.path().join("config.toml");
                    std::fs::write(&path, &config).expect("write the block");
                    crate::config_examples_test::validate_config(&name, &path);
                    let check = datalib_dag::config::check_text(&config);
                    let dropped: Vec<_> = check
                        .diagnostics
                        .iter()
                        .filter(|d| d.severity.drops_the_entry())
                        .collect();
                    assert!(dropped.is_empty(), "{name}: {dropped:?}");
                    configs += 1;
                }
                Some(FRAGMENT) => {
                    toml::from_str::<toml::Table>(&block.body)
                        .unwrap_or_else(|e| panic!("{name}: not TOML: {e}"));
                }
                Some(other) => panic!(
                    "{name}: `toml {other}`: a TOML block is a config that loads, or \
                     `toml {FRAGMENT}`"
                ),
            }
        }
    }
    assert!(configs > 0, "no TOML block was checked");
}

#[test]
fn every_datalib_step_and_datalib_dag_command_in_the_docs_parses() {
    use clap::CommandFactory;
    for (program, cmd) in [
        ("datalib-step", crate::Cli::command()),
        // The binary adds `--version` at run time, with its stamped version.
        (
            "datalib-dag",
            datalib_dag::cli::Cli::command().version("stamped"),
        ),
    ] {
        let commands = datalib_agent_docs::commands_running(program);
        assert!(!commands.is_empty(), "no doc runs {program}");
        datalib_cli_help::assert_argvs_parse(cmd, &commands);
    }
}

/// An agent reading only the overview learns the contacts app exists and
/// what it is for, and each way it might ask for it lands on the same page.
#[test]
fn the_overview_names_every_applet_and_the_contacts_app_can_be_asked_for() {
    let overview = crate::help::page(&[], &[], "abc123").unwrap();
    for a in datalib_agent_docs::APPLETS {
        assert!(overview.contains(a.name), "{} is missing", a.name);
    }
    assert!(overview.contains("link handles to contacts"), "{overview}");
    assert!(
        overview.contains("datalib-step help contacts-app"),
        "{overview}"
    );

    let page = |word: &str| crate::help::page(&[word.to_string()], &[], "abc123").unwrap();
    assert_eq!(page("datalib_contacts"), page("contacts-app"));
    assert_eq!(page("photos"), page("applets"));
    assert!(
        page("contacts").contains("datalib-step help contacts-app"),
        "the address-book source's page says where the contacts app is"
    );
}

/// A doc that names `datalib-step help <topic>`, in a shell block or in
/// its text, names a page that exists.
#[test]
fn every_help_topic_the_docs_name_is_a_page() {
    use clap::CommandFactory;
    let commands: Vec<String> = crate::Cli::command()
        .get_subcommands()
        .map(|c| c.get_name().to_string())
        .collect();
    let is_page = |words: &[String]| {
        matches!(words, [one] if commands.contains(one))
            || crate::help::page(words, &commands, "abc123").is_ok()
    };
    let mut named: Vec<(String, Vec<String>)> = Vec::new();
    for d in markdown_docs() {
        let printed = datalib_agent_docs::page(d, "abc123");
        for (n, line) in printed.lines().enumerate() {
            if line.trim_start().starts_with("```") {
                continue;
            }
            for span in line.split('`').skip(1).step_by(2) {
                if let Some(topic) = span.strip_prefix("datalib-step help ") {
                    let words = topic.split_whitespace().map(str::to_string).collect();
                    named.push((format!("{}:{}", d.path, n + 1), words));
                }
            }
        }
    }
    for (place, argv) in datalib_agent_docs::commands_running("datalib-step") {
        if argv.get(1).map(String::as_str) == Some("help") {
            named.push((place, argv[2..].to_vec()));
        }
    }
    assert!(!named.is_empty());
    let missing: Vec<String> = named
        .iter()
        .filter(|(_, words)| {
            !words
                .iter()
                .any(|w| w.starts_with('<') || w.starts_with('-'))
        })
        .filter(|(_, words)| !is_page(words))
        .map(|(place, words)| format!("{place}: datalib-step help {}", words.join(" ")))
        .collect();
    assert!(missing.is_empty(), "no such page:\n{}", missing.join("\n"));
}
