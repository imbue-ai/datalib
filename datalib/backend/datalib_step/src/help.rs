//! `datalib-step help`: what an agent setting datalib up from a shell reads
//! in place of the app's wizard. Every page is built here as a string from
//! what this binary already knows (its source types, their ingest methods,
//! its functions) and from the docs compiled into it; `main` prints them.

use datalib_cli_help::Example;
use datalib_source_common::Reach;
use strum::VariantArray;

use crate::function::Function;
use crate::methods::ingest_methods;
use crate::source_type::SourceType;

const AGENT_GUIDE: &str = include_str!("../../../../docs/agent_user.md");
const AGENT_GUIDE_PATH: &str = "docs/agent_user.md";
const GETTING_YOUR_DATA: &str = include_str!("../../../../docs/user/getting_your_data.md");
const GETTING_YOUR_DATA_PATH: &str = "docs/user/getting_your_data.md";
const REPO: &str = "https://github.com/imbue-ai/datalib";

/// One step of setting a source up: what to do, and the commands that do
/// it. A command here is one a test runs through its binary's parser.
struct SetupStep {
    say: &'static str,
    commands: &'static [Example],
}

const SETUP: &[SetupStep] = &[
    SetupStep {
        say: "Make a data root: one directory holding config.toml and everything synced \
              into it. Start from the commented example config (`configs/dag_example.toml` \
              in the repo), or run `datalib-http <data_root> --init` once, which writes a \
              starter config with the index steps and no sources.",
        commands: &[],
    },
    SetupStep {
        say: "Add a source: a [[groups]] entry with its `type`, and its steps. The source's \
              page says what it mirrors, how it signs in and which ingest methods it has.",
        commands: &[Example {
            what: "what the Slack source needs",
            command: "datalib-step help slack",
        }],
    },
    SetupStep {
        say: "Sign it in. A web source signs in through latchkey, which ships beside these \
              binaries; its page has the command (`latchkey auth browser slack` for Slack). \
              A source that reads files needs only their path in its params.",
        commands: &[],
    },
    SetupStep {
        say: "Check the config. Every problem is reported at once, each with its line.",
        commands: &[Example {
            what: "check the config",
            command: "datalib-dag --check ~/datalib/config.toml",
        }],
    },
    SetupStep {
        say: "Check the sign-in: ask the service which account the credentials reach. \
              Writes nothing.",
        commands: &[Example {
            what: "check the sign-in",
            command: "datalib-step probe slack",
        }],
    },
    SetupStep {
        say: "Sync the source, and everything that reads it.",
        commands: &[Example {
            what: "sync it",
            command: "datalib-dag ~/datalib/config.toml --sync slack/ingest --by claude",
        }],
    },
    SetupStep {
        say: "Read what came in: `datalib-doltlite -readonly \
              <data_root>/unified_document_index/grid_index/db.doltlite_db \"SELECT provider, \
              count(*) FROM grid_rows GROUP BY 1\"` (stock sqlite3 cannot open it), or serve \
              the app with `datalib-http <data_root>`. `datalib-step help agent` has the rest.",
        commands: &[],
    },
];

pub fn page(topic: &[String], commands: &[String], git_ref: &str) -> Result<String, String> {
    let base = format!("{REPO}/blob/{git_ref}");
    match topic {
        [] => Ok(overview(commands)),
        [word] if word == "agent" => Ok(absolute_links(AGENT_GUIDE, AGENT_GUIDE_PATH, &base)),
        [word] => {
            if let Some(t) = SourceType::parse(word) {
                return Ok(source_page(t, &base));
            }
            if let Some(f) = Function::parse(word) {
                return Ok(function_page(f));
            }
            Err(format!(
                "no help on {word:?}. Try `datalib-step help` for the topics: \
                 `agent`, a source type, a function or a command."
            ))
        }
        _ => Err(format!(
            "`datalib-step help` takes one topic, not {}",
            topic.join(" ")
        )),
    }
}

fn overview(commands: &[String]) -> String {
    let mut out = String::from("datalib-step help: setting datalib up from a shell\n\n");
    out.push_str(&wrap(
        "datalib mirrors a person's data (chats, email, messages, contacts, …) into one \
         directory, the data root, and indexes it for search. Its config.toml lists the \
         sources, and datalib-dag syncs them. To set a source up:",
        0,
        0,
    ));
    out.push('\n');
    for (i, step) in SETUP.iter().enumerate() {
        out.push_str(&wrap(&format!("{}. {}", i + 1, step.say), 2, 5));
        for c in step.commands {
            out.push_str(&format!("       {}\n", c.command));
        }
        out.push('\n');
    }
    let topics = [
        (
            "datalib-step help agent",
            "the agent guide: the data root's layout, syncing, logs, reading the data, \
             troubleshooting",
        ),
        (
            "datalib-step help <type>",
            "one source: what it mirrors, how it signs in, its ingest methods",
        ),
        (
            "datalib-step help <function>",
            "one step function: what it does, where it goes, what it reads",
        ),
        (
            "datalib-step help <command>",
            "one of this binary's commands",
        ),
    ];
    out.push_str("More:\n");
    for (cmd, what) in topics {
        out.push_str(&format!("  {cmd:<30} {what}\n"));
    }
    let types: Vec<&str> = SourceType::VARIANTS.iter().map(|t| t.as_str()).collect();
    out.push_str("\nSource types:\n");
    out.push_str(&wrap(&types.join(", "), 2, 2));
    out.push_str("\nFunctions (a step's id is <group>/<function>):\n");
    for f in Function::VARIANTS {
        out.push_str(&format!(
            "  {:<20} {}\n",
            f.as_str(),
            first_sentence(f.describe())
        ));
    }
    out.push_str("\nCommands:\n");
    out.push_str(&wrap(&commands.join(", "), 2, 2));
    out.push_str("\nThe agent guide also ships beside the binaries, as docs/agent_user.md.");
    out
}

fn source_page(t: SourceType, base: &str) -> String {
    let mut out = String::new();
    for section in sections_for(t) {
        out.push_str(&absolute_links(
            section.trim_end(),
            GETTING_YOUR_DATA_PATH,
            base,
        ));
        out.push_str("\n\n");
    }
    out.push_str(&wrap(
        "Ingest methods: the `ingest` step's params hold one of these tables, \
         `[steps.params.<method>]`:",
        0,
        0,
    ));
    for m in ingest_methods(t) {
        let reach = match m.reach {
            Reach::Origin => "reaches a live service",
            Reach::Local => "reads files on disk",
        };
        out.push_str(&format!("  {:<12} {reach}\n", m.path));
    }
    out.push('\n');
    out.push_str(&wrap(
        "Its steps: `<group>/ingest`, and `<group>/render_markdown` reading it; to \
         search it, `<group>/keyword_index` and `<group>/embed` as well \
         (`datalib-step help embed`). Every source's stanza, with each option \
         commented:",
        0,
        0,
    ));
    out.push_str(&format!(
        "  {base}/docs/user/config_examples/all_sources.toml\n\n"
    ));
    out.push_str(&wrap(
        &format!(
            "Then: `datalib-dag --check <config>`; `datalib-step probe {t}` if it reaches \
             a service; `datalib-dag <config> --sync <group>/ingest --by <you>`."
        ),
        0,
        0,
    ));
    out.truncate(out.trim_end().len());
    out
}

fn function_page(f: Function) -> String {
    let id = example_step_id(f);
    let (sync, reset) = run_commands(f);
    let mut out = format!(
        "{f}\n\n{}\n{}  {sync}\n",
        wrap(f.describe(), 0, 0),
        wrap(
            "To run one, ask datalib-dag, which runs every step that reads it after it \
             and records what it published; running datalib-step by hand records \
             nothing, so the steps that read it never learn it moved:",
            0,
            0
        ),
    );
    out.push('\n');
    match (f, reset) {
        (Function::Ingest, _) => out.push_str(&wrap(
            &format!(
                "A synced ingest step always runs. `--reset {id}` empties what it \
                 downloaded, so its next sync fetches everything again."
            ),
            0,
            0,
        )),
        (_, Some(reset)) => {
            out.push_str(&wrap(
                "It runs only if what it reads moved since its last run. To rebuild it \
                 anyway, reset it in the same command:",
                0,
                0,
            ));
            out.push_str(&format!("  {reset}\n"));
        }
        (_, None) => out.push_str(&wrap(
            "It runs only if what it reads moved since its last run; it has nothing of \
             its own to reset.",
            0,
            0,
        )),
    }
    out.truncate(out.trim_end().len());
    out
}

fn example_step_id(f: Function) -> String {
    let group = match f {
        Function::Ingest | Function::RenderMarkdown | Function::KeywordIndex | Function::Embed => {
            "slack"
        }
        Function::RenderTimeSeries => "garmin",
        Function::GridIndex | Function::QmdAggregator | Function::EmbeddingMap => {
            datalib_runtime::layout::UNIFIED_DOCUMENT_INDEX_DIR
        }
        Function::TimeSeriesIndex => datalib_runtime::layout::UNIFIED_TIME_SERIES_INDEX_DIR,
    };
    format!("{group}/{f}")
}

// The sync, and for a step that can be rebuilt from what it reads, the
// reset that forces it.
fn run_commands(f: Function) -> (String, Option<String>) {
    let id = example_step_id(f);
    let sync = format!("datalib-dag ~/datalib/config.toml --sync {id} --by claude");
    let reset = (f.resets() && f != Function::Ingest)
        .then(|| format!("datalib-dag ~/datalib/config.toml --reset {id} --sync {id} --by claude"));
    (sync, reset)
}

// A type reached several ways (email by JMAP, the Gmail API or an mbox)
// has one section per way. `//:lint_repo` holds the doc to a section for
// every type.
fn sections_for(t: SourceType) -> Vec<&'static str> {
    let declaration = format!("`type = \"{}\"`", t.as_str());
    let doc = GETTING_YOUR_DATA;
    let starts: Vec<usize> = doc.match_indices("\n## ").map(|(i, _)| i + 1).collect();
    starts
        .iter()
        .enumerate()
        .map(|(n, &start)| &doc[start..starts.get(n + 1).copied().unwrap_or(doc.len())])
        .filter(|s| s.lines().any(|l| l.starts_with(&declaration)))
        .collect()
}

// A copy read outside the repo (a tarball, a terminal, one section of a
// doc) has no tree for a relative link or a bare `#anchor` to lead into,
// so each points at its file under `base`.
pub fn absolute_links(markdown: &str, doc_path: &str, base: &str) -> String {
    let mut out = String::with_capacity(markdown.len());
    let mut rest = markdown;
    while let Some(i) = rest.find("](") {
        let (before, after) = rest.split_at(i + 2);
        out.push_str(before);
        let Some(end) = after.find(')') else {
            rest = after;
            break;
        };
        out.push_str(&absolute_link(&after[..end], doc_path, base));
        rest = &after[end..];
    }
    out.push_str(rest);
    out
}

fn absolute_link(target: &str, doc_path: &str, base: &str) -> String {
    if target.contains("://") || target.starts_with("mailto:") {
        return target.to_string();
    }
    if target.starts_with('#') {
        return format!("{base}/{doc_path}{target}");
    }
    let doc_dir = doc_path.rsplit_once('/').map_or("", |(dir, _)| dir);
    let (path, fragment) = match target.split_once('#') {
        Some((p, f)) => (p, Some(f)),
        None => (target, None),
    };
    let mut parts: Vec<&str> = doc_dir.split('/').filter(|s| !s.is_empty()).collect();
    for segment in path.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            s => parts.push(s),
        }
    }
    let mut url = format!("{base}/{}", parts.join("/"));
    if let Some(f) = fragment {
        url.push('#');
        url.push_str(f);
    }
    url
}

fn first_sentence(s: &str) -> &str {
    s.split_once(". ").map_or(s, |(first, _)| first)
}

fn wrap(text: &str, first: usize, rest: usize) -> String {
    const WIDTH: usize = 80;
    let mut out = String::new();
    let mut line = " ".repeat(first);
    let mut empty = true;
    for word in text.split_whitespace() {
        if !empty && line.len() + 1 + word.len() > WIDTH {
            out.push_str(line.trim_end());
            out.push('\n');
            line = " ".repeat(rest);
            empty = true;
        }
        if !empty {
            line.push(' ');
        }
        line.push_str(word);
        empty = false;
    }
    out.push_str(line.trim_end());
    out.push('\n');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: &str = "https://github.com/imbue-ai/datalib/blob/abc123";

    fn commands() -> Vec<String> {
        vec!["probe".into(), "pull-runtime".into()]
    }

    fn relative_links(page: &str) -> Vec<String> {
        page.split("](")
            .skip(1)
            .filter_map(|rest| rest.split(')').next())
            .filter(|t| !t.starts_with("https://"))
            .map(str::to_string)
            .collect()
    }

    #[test]
    fn links_resolve_against_the_docs_own_directory() {
        let md = "[a](../AGENTS.md) [b](dev/x.md#sec) [c](#here) [d](https://e.org/f) \
                  [g](user/config_examples/all_sources.toml)";
        assert_eq!(
            absolute_links(md, "docs/guide.md", BASE),
            format!(
                "[a]({BASE}/AGENTS.md) [b]({BASE}/docs/dev/x.md#sec) \
                 [c]({BASE}/docs/guide.md#here) [d](https://e.org/f) \
                 [g]({BASE}/docs/user/config_examples/all_sources.toml)"
            )
        );
    }

    /// The guide is read outside the repo: from a tarball, or printed in a
    /// terminal. A relative link there leads nowhere.
    #[test]
    fn the_agent_guide_has_no_relative_link_left() {
        let page = page(&["agent".into()], &commands(), "abc123").unwrap();
        assert!(page.contains("# datalib — agent user guide"));
        assert_eq!(relative_links(&page), Vec::<String>::new());
    }

    #[test]
    fn every_source_type_has_a_page_with_its_own_sections() {
        for &t in SourceType::VARIANTS {
            let page = page(&[t.as_str().into()], &commands(), "abc123").unwrap();
            assert!(
                page.contains(&format!("`type = \"{}\"`", t.as_str())),
                "{t}: getting_your_data.md has no section for it:\n{page}"
            );
            assert!(page.starts_with("## "), "{t}: {page}");
            assert_eq!(relative_links(&page), Vec::<String>::new(), "{t}");
            for m in ingest_methods(t) {
                assert!(page.contains(m.path), "{t}: method {} missing", m.path);
            }
        }
    }

    #[test]
    fn a_type_reached_several_ways_shows_every_way() {
        let page = page(&["email".into()], &commands(), "abc123").unwrap();
        for heading in ["## Email", "## Fastmail\n", "## Gmail"] {
            assert!(page.contains(heading), "{heading} missing:\n{page}");
        }
    }

    #[test]
    fn the_overview_names_every_type_function_and_command() {
        let page = page(&[], &commands(), "abc123").unwrap();
        for &t in SourceType::VARIANTS {
            assert!(page.contains(t.as_str()), "{t}");
        }
        for &f in Function::VARIANTS {
            assert!(page.contains(f.as_str()), "{f}");
        }
        assert!(page.contains("pull-runtime"));
    }

    #[test]
    fn every_function_has_a_page() {
        for &f in Function::VARIANTS {
            let page = page(&[f.as_str().into()], &commands(), "abc123").unwrap();
            assert!(page.len() > f.as_str().len() + 40, "{f}: {page}");
        }
    }

    #[test]
    fn an_unknown_topic_is_refused_by_name() {
        let err = page(&["slak".into()], &commands(), "abc123").unwrap_err();
        assert!(err.contains("\"slak\""), "{err}");
    }

    #[test]
    fn every_function_page_says_how_to_run_it_and_the_commands_parse() {
        use clap::CommandFactory;
        let mut commands: Vec<String> = Vec::new();
        for &f in Function::VARIANTS {
            let (sync, reset) = run_commands(f);
            let page = function_page(f);
            assert!(page.contains(&sync), "{f}: {page}");
            assert_eq!(reset.is_some(), f.resets() && f != Function::Ingest, "{f}");
            if let Some(reset) = reset {
                assert!(page.contains(&reset), "{f}: {page}");
                commands.push(reset);
            }
            commands.push(sync);
        }
        let examples: Vec<Example> = commands
            .iter()
            .map(|c| Example {
                what: "run a step",
                command: Box::leak(c.clone().into_boxed_str()),
            })
            .collect();
        datalib_cli_help::assert_examples_parse(datalib_dag::cli::Cli::command(), &examples);
    }

    /// The setup steps quote commands for two binaries; each has to parse
    /// as the binary it names would parse it.
    #[test]
    fn the_setup_commands_parse() {
        use clap::CommandFactory;
        let (dag, step): (Vec<Example>, Vec<Example>) = SETUP
            .iter()
            .flat_map(|s| s.commands)
            .map(|e| Example {
                what: e.what,
                command: e.command,
            })
            .partition(|e| e.command.starts_with("datalib-dag "));
        assert!(!dag.is_empty() && !step.is_empty());
        datalib_cli_help::assert_examples_parse(datalib_dag::cli::Cli::command(), &dag);
        datalib_cli_help::assert_examples_parse(crate::Cli::command(), &step);
    }
}
