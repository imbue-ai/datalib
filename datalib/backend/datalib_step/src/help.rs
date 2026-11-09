//! `datalib-step help`: what an agent setting datalib up from a shell reads
//! in place of the app's wizard. Every page is built here as a string from
//! what this binary already knows (its source types, their ingest methods,
//! its functions) and from the docs compiled into it
//! (`datalib_agent_docs`); `main` prints them.

use datalib_agent_docs::{Doc, DOCS, GETTING_YOUR_DATA};
use datalib_cli_help::Example;
use datalib_source_common::Reach;
use strum::VariantArray;

use datalib_source_common::ExampleKind;

use crate::function::Function;
use crate::methods::ingest_methods;
use crate::probe::probe_command;
use crate::source_docs::{fields, source_docs, Field};
use crate::source_type::SourceType;

/// The topic that lists every doc compiled in.
const DOCS_TOPIC: &str = "docs";

/// One step of setting a source up: what to do, and the commands that do
/// it. A command here is one a test runs through its binary's parser.
struct SetupStep {
    say: &'static str,
    commands: &'static [Example],
}

const SETUP: &[SetupStep] = &[
    SetupStep {
        say: "Make a data root: one directory holding config.toml and everything synced \
              into it. `datalib-dag init` writes a starter config with the index steps and \
              no sources, and nothing else. The commented example config, \
              `configs/dag_example.toml`, is in the repo and beside the binaries in a release.",
        commands: &[Example {
            what: "start a data root",
            command: "datalib-dag init ~/datalib",
        }],
    },
    SetupStep {
        say: "Add a source: a [[groups]] entry with its `type`, and its steps. The source's \
              page says what it mirrors, how it signs in and which ingest methods it has. \
              `examples --append` adds its least config and puts its steps in the index \
              steps' `inputs`; name the method with `--method` when it has more than one, \
              and give it an id of its own with `--id` to add a second account. Then set \
              its options in the config; `examples` without `--append` shows every one.",
        commands: &[
            Example {
                what: "what the Slack source needs",
                command: "datalib-step help slack",
            },
            Example {
                what: "add Slack to the config",
                command: "datalib-step examples slack --append ~/datalib/config.toml",
            },
        ],
    },
    SetupStep {
        say: "Sign it in. A web source signs in through latchkey, which ships beside these \
              binaries; its page has the command (`latchkey auth browser slack` for Slack), \
              and what to do instead when a latchkey gateway holds the credentials. A source \
              that reads files needs only their path in its params.",
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
              Writes nothing. A source with more than one way in names it: \
              `datalib-step probe email --method gmail`.",
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
        say: "Read what came in. Search it as the app's search box does, with no server, \
              and get JSON back: `datalib-applet search <data_root> 'from:picard \
              after:2026-10-01'` (`''` lists every row; `datalib-applet search --help` has \
              the flags). For SQL, `datalib-doltlite -readonly \
              <data_root>/unified_document_index/grid_index/db.sqlite \"SELECT provider, \
              count(*) FROM grid_rows GROUP BY 1\"` (stock sqlite3 opens it read-only only \
              while its writer is running). Or serve \
              the app with `datalib-http <data_root>`. `datalib-step help search` has the \
              search's query language, and `datalib-step help agent` the rest.",
        commands: &[],
    },
    SetupStep {
        say: "Say who is who. Sources know a person only by handles: an address here, a \
              number there. The contacts app links them into one contact, which every chip \
              and `with:contact:<id>` search then follows; you propose the links, and the \
              person confirms them. `datalib-step help contacts-app` says how.",
        commands: &[],
    },
];

pub fn page(topic: &[String], commands: &[String], git_ref: &str) -> Result<String, String> {
    if let [word] = topic {
        if word == DOCS_TOPIC {
            return Ok(docs_page());
        }
        if let Some(doc) = datalib_agent_docs::doc(word) {
            return Ok(datalib_agent_docs::page(doc, git_ref));
        }
    }
    let source = topic.first().and_then(|w| SourceType::parse(w));
    match (topic, source) {
        ([], _) => Ok(overview(commands)),
        ([_], Some(t)) => Ok(source_page(t, git_ref)),
        ([_, method], Some(t)) => Ok(method_page(t, known_method(t, method)?)),
        ([word], None) => match Function::parse(word) {
            Some(f) => Ok(function_page(f)),
            None => match applet_doc(word) {
                Some(doc) => Ok(datalib_agent_docs::page(doc, git_ref)),
                None => Err(format!(
                    "no help on {word:?}. Try `datalib-step help` for the topics: `agent`, \
                     `applets`, `docs` for every guide and reference, a source type, a \
                     function or a command."
                )),
            },
        },
        _ => Err(format!(
            "no help on `{}`: a topic is one word, or a source type and one of its methods",
            topic.join(" ")
        )),
    }
}

/// The doc on an applet, by the applet's name. A source type of the same
/// name (`slack`) is its own page, and the applet is on `help applets`.
fn applet_doc(name: &str) -> Option<&'static Doc> {
    let applet = datalib_agent_docs::APPLETS
        .iter()
        .find(|a| a.name == name)?;
    datalib_agent_docs::doc(applet.topic)
}

// One line per doc, `<topic> <path>`, which `scripts/release/stage_tarball.sh`
// reads to write each beside the binaries; what it is for under it.
fn docs_page() -> String {
    use datalib_agent_docs::Kind;
    let mut out = wrap(
        "The docs compiled into datalib-step. `datalib-step help <topic>` prints one whole, \
         and a release carries each beside its binaries at the path shown. A link from \
         one to another prints as the command that prints it; a link to anything else in \
         the repo, as its raw file at this build's commit.",
        0,
        0,
    );
    for (kind, heading) in [
        (Kind::Guide, "Guides, for using datalib:"),
        (
            Kind::Reference,
            "References, written for datalib's contributors too:",
        ),
        (Kind::Config, "Example configs, each one that loads:"),
    ] {
        out.push_str(&format!("\n{heading}\n"));
        for d in DOCS.iter().filter(|d| d.kind == kind) {
            out.push_str(&doc_line(d));
        }
    }
    out.truncate(out.trim_end().len());
    out
}

fn doc_line(d: &Doc) -> String {
    format!("  {:<18} {}\n{}", d.topic, d.path, wrap(d.about, 21, 21))
}

fn known_method(t: SourceType, method: &str) -> Result<&'static str, String> {
    let methods = ingest_methods(t);
    methods
        .iter()
        .find(|m| m.path == method)
        .map(|m| m.path)
        .ok_or_else(|| {
            let names: Vec<&str> = methods.iter().map(|m| m.path).collect();
            format!(
                "{t} has no method {method:?}; its methods: {}",
                names.join(", ")
            )
        })
}

pub fn examples_page(word: &str, method: Option<&str>) -> Result<String, String> {
    let t = SourceType::parse(word)
        .ok_or_else(|| format!("no source type {word:?}. `datalib-step help` lists them."))?;
    let methods = ingest_methods(t);
    if let Some(m) = method.filter(|m| !methods.iter().any(|known| known.path == *m)) {
        let names: Vec<&str> = methods.iter().map(|known| known.path).collect();
        return Err(format!(
            "{t} has no method {m:?}; its methods: {}",
            names.join(", ")
        ));
    }
    let docs = source_docs(t);
    let mut out = format!("Config stanzas for {t}\n\n");
    for paragraph in examples_intro(t) {
        out.push_str(&wrap(&paragraph, 0, 0));
        out.push('\n');
    }
    for m in methods
        .iter()
        .filter(|m| method.is_none_or(|want| want == m.path))
    {
        for &kind in ExampleKind::VARIANTS {
            if let Some(example) = docs.example(kind, m.path) {
                out.push_str(&format!(
                    "## {t} by `{}`, {}\n\n{}\n\n",
                    m.path,
                    kind.as_str(),
                    example.toml.trim_end()
                ));
            }
        }
    }
    out.truncate(out.trim_end().len());
    Ok(out)
}

fn examples_intro(t: SourceType) -> Vec<String> {
    let methods: Vec<&str> = ingest_methods(t).iter().map(|m| m.path).collect();
    let sign_in = if t.uses_latchkey_account() {
        format!(" `datalib-step help {t}` says how to sign it in.")
    } else {
        String::new()
    };
    let time_series = if t.renders_time_series() {
        " `render_time_series` does the same for its numbers over time, for the charts \
         and the time-series index."
    } else if t.renders_occurrences() {
        " `render_occurrences` lists every occurrence of every event in a window of days \
         around now, one row each, the way Google's `singleEvents=true` does: \
         `<id>/render_occurrences/occurrences.doltlite_db`, table `occurrences`."
    } else {
        ""
    };
    let which = match methods.as_slice() {
        [only] => format!(
            "{t} has one ingest method, `{only}`, so there are two stanzas below. They \
             declare the same group: append one, not both."
        ),
        _ => format!(
            "{t} is reached {} ways ({}), and a group holds exactly one of them, so there \
             is a pair of stanzas for each. Append the stanza for the way that fits. To \
             mirror it two ways, or from two accounts, append two stanzas and give them \
             different ids (below).",
            methods.len(),
            methods.join(", ")
        ),
    };
    vec![
        format!(
            "Each stanza below is one source, ready to append to \
             `<data_root>/config.toml`: a `[[groups]]` entry naming the source's type, and \
             the steps that sync it. `ingest` downloads the data, or reads it from disk, \
             into the group's raw store; its params hold one ingest method's table, \
             `[steps.params.<method>]`, beside the options every method shares. \
             `render_markdown` turns what `ingest` stored into the documents the grid and \
             search read.{time_series}"
        ),
        format!(
            "There are two kinds. `minimal` is the fewest lines that sync: the method's \
             table and only what it cannot do without, so everything else takes its \
             default. `complete` sets every option the params take, each with a comment \
             saying what it does and what leaving it out means; most also add the \
             `keyword_index` and `embed` steps that put the source in free-text search. \
             Start from either: trim the complete one, or add to the minimal one. \
             `datalib-step help {t} <method>` lists every option with its type and default."
        ),
        which,
        format!(
            "Every stanza names its group after its type, `{t}`. The group's `id` is the \
             folder under the data root its data goes in, and the first half of each of \
             its step ids (`{t}/ingest`), so it must be unique in the config; choose it \
             once. To use another id, change the group's `id`, each step's `group`, and the \
             `{t}/` at the start of every step named in an `inputs` list. Leave `type` as \
             it is. `name` is free text the app shows, and can be anything."
        ),
        format!(
            "`datalib-step examples {t} --append <config>` appends the minimal stanza for \
             you, with `--method` to pick one and `--id` to rename its group, and does the \
             wiring below; it refuses an id the config already has, and a result \
             `datalib-dag --check` would refuse."
        ),
        format!(
            "A source's documents reach the grid and search only through the index groups, \
             which name the steps they read. After appending by hand, add \
             `<id>/render_markdown` to the `inputs` of `unified_document_index/grid_index`, \
             and if the stanza has `keyword_index` and `embed` steps, add both to the \
             `inputs` of `unified_document_index/qmd_aggregator`.{}",
            if t.renders_time_series() {
                " Add `<id>/render_time_series` to the `inputs` of \
                 `unified_time_series_index/time_series_index`."
            } else {
                ""
            }
        ),
        format!(
            "Then `datalib-dag --check <config>` reports every problem with the config at \
             once, each at its line; `{}` asks the service which \
             account the credentials reach, if the method reaches one; and \
             `datalib-dag <config> --sync <id>/ingest --by <you>` syncs the source and \
             everything that reads it.{sign_in}",
            probe_command(t)
        ),
    ]
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
            "datalib-step help contacts-app",
            "who a handle is: linking handles to contacts, labelling people and senders",
        ),
        (
            "datalib-step help applets",
            "the servers the app runs, listed below: what each serves, how to call them",
        ),
        (
            "datalib-step help docs",
            "every doc compiled in: raw stores, apps, search, a step of your own, …",
        ),
        (
            "datalib-step help <type>",
            "one source: what it mirrors, how it signs in, its ingest methods",
        ),
        (
            "datalib-step examples <type>",
            "one source's config stanzas, and how to add one to a config",
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
    out.push_str("\nApplets (reached at /applet/<id>/ on datalib-http once the app runs):\n");
    for a in datalib_agent_docs::APPLETS {
        out.push_str(&format!("  {:<26} {}\n", a.name, a.does));
    }
    out.push_str("\nCommands:\n");
    out.push_str(&wrap(&commands.join(", "), 2, 2));
    out.push_str(
        "\nA release also carries every doc beside the binaries, at the path \
         `datalib-step help docs` shows; the agent guide is docs/agent_user.md.",
    );
    out
}

fn source_page(t: SourceType, git_ref: &str) -> String {
    let mut out = String::new();
    let sections = sections_for(t);
    // Each section is one way in, and the first alone reads as the whole.
    if sections.len() > 1 {
        let headings: Vec<&str> = sections
            .iter()
            .filter_map(|s| s.lines().next()?.strip_prefix("## "))
            .collect();
        out.push_str(&wrap(
            &format!(
                "This type covers {} ways in, one section each: {}.",
                headings.len(),
                headings.join(", ")
            ),
            0,
            0,
        ));
        out.push('\n');
    }
    for section in sections {
        out.push_str(&datalib_agent_docs::print_links(
            section.trim_end(),
            GETTING_YOUR_DATA,
            git_ref,
            datalib_agent_docs::Reader::SourcePage(t.as_str()),
        ));
        out.push_str("\n\n");
    }
    if t.uses_latchkey_account() {
        out.push_str(&wrap(&behind_a_gateway(t), 0, 0));
        out.push('\n');
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
    let docs = source_docs(t);
    for m in ingest_methods(t) {
        if let Some(example) = docs.example(ExampleKind::Minimal, m.path) {
            out.push('\n');
            out.push_str(&wrap(
                &format!(
                    "The least a config needs to sync it by `{m}` (`datalib-step help {t} \
                     {m}` lists every option):",
                    m = m.path
                ),
                0,
                0,
            ));
            out.push_str(&format!("\n{}\n", example.toml.trim_end()));
        }
    }
    out.push('\n');
    out.push_str(&wrap(
        &format!(
            "`datalib-step examples {t}` prints each stanza above beside one that sets \
             every option, and says how to give the group an id of your own, how to put it in \
             the grid and search, and how to check and sync it."
        ),
        0,
        0,
    ));
    out.truncate(out.trim_end().len());
    out
}

// Printed whatever this process's environment says, so a page reads the
// same wherever it is printed and written beside the binaries.
fn behind_a_gateway(t: SourceType) -> String {
    format!(
        "The `latchkey auth` commands above sign in on the machine you run them on. If \
         `LATCHKEY_GATEWAY` is set in your environment, a latchkey gateway holds the \
         credentials instead, and those commands may be refused or sign in somewhere else: \
         sign in, or ask for access, wherever that gateway is managed. Either way, \
         `{}` says whether the sign-in works.",
        probe_command(t)
    )
}

fn method_page(t: SourceType, method: &'static str) -> String {
    let reach = ingest_methods(t)
        .iter()
        .find(|m| m.path == method)
        .map_or("", |m| match m.reach {
            Reach::Origin => "reaches a live service",
            Reach::Local => "reads files on disk",
        });
    let methods: Vec<&str> = ingest_methods(t).iter().map(|m| m.path).collect();
    let all = fields(&source_docs(t).schema);
    let (own, shared): (Vec<&Field>, Vec<&Field>) = all
        .iter()
        .filter(|f| {
            let top = f.path.split(['.', '[']).next().unwrap_or("");
            top == method || !methods.contains(&top)
        })
        .partition(|f| f.path.split(['.', '[']).next() == Some(method));
    let mut out = wrap(
        &format!(
            "{t} by `{method}` ({reach}): every option of the `ingest` step's params, \
             from the structs that read them. `datalib-step examples {t}` prints a stanza \
             that sets them all."
        ),
        0,
        0,
    );
    out.push_str(&field_tables(&own));
    out.push_str("\nAny method also takes:\n");
    out.push_str(&field_tables(&shared));
    out.truncate(out.trim_end().len());
    out
}

// Fields grouped under the TOML table header that holds them, so the
// reference reads like the config it describes.
fn field_tables(fields: &[&Field]) -> String {
    let mut out = String::new();
    let mut table: Option<&str> = None;
    for f in fields {
        let (parent, key) = f.path.rsplit_once('.').unwrap_or(("", f.path.as_str()));
        if table != Some(parent) {
            let header = match parent.strip_suffix("[]") {
                Some(list) => format!("[[steps.params.{list}]]"),
                None if parent.is_empty() => "[steps.params]".to_string(),
                None => format!("[steps.params.{parent}]"),
            };
            out.push_str(&format!("\n{header}\n"));
            table = Some(parent);
        }
        let default = f
            .default
            .as_ref()
            .map(|d| format!(", default {d}"))
            .unwrap_or_default();
        out.push_str(&format!("  {key}  ({}{default})\n", f.kind));
        if let Some(d) = &f.description {
            out.push_str(&wrap(d, 6, 6));
        }
    }
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
        Function::RenderOccurrences => "calendar",
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
    let doc = datalib_agent_docs::by_path(GETTING_YOUR_DATA)
        .expect("getting_your_data.md is compiled in")
        .text;
    let starts: Vec<usize> = doc.match_indices("\n## ").map(|(i, _)| i + 1).collect();
    starts
        .iter()
        .enumerate()
        .map(|(n, &start)| &doc[start..starts.get(n + 1).copied().unwrap_or(doc.len())])
        .filter(|s| s.lines().any(|l| l.starts_with(&declaration)))
        .collect()
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

    fn commands() -> Vec<String> {
        vec!["probe".into(), "pull-runtime".into()]
    }

    fn relative_links(page: &str) -> Vec<String> {
        datalib_agent_docs::links(page)
            .into_iter()
            .filter(|l| {
                datalib_agent_docs::resolve(l.target, "page.md")
                    != datalib_agent_docs::Target::External
            })
            .map(|l| l.target.to_string())
            .collect()
    }

    /// A guide is read outside the repo: from a tarball, or printed in a
    /// terminal. A relative link there leads nowhere.
    #[test]
    fn every_doc_prints_by_its_topic_with_no_relative_link_left() {
        for (word, title) in [
            ("agent", "# datalib — agent user guide"),
            ("raw-stores", "# Reading the raw stores"),
            ("apps", "# Building an app on datalib"),
            ("search", "# Searching your mirror"),
            ("step-protocol", "# Writing a step command"),
        ] {
            let page = page(&[word.into()], &commands(), "abc123").unwrap();
            assert!(page.starts_with(title), "{word}");
        }
        for d in DOCS {
            let page = page(&[d.topic.into()], &commands(), "abc123").unwrap();
            assert_eq!(page, datalib_agent_docs::page(d, "abc123"), "{}", d.topic);
            assert_eq!(relative_links(&page), Vec::<String>::new(), "{}", d.topic);
        }
    }

    /// A doc's topic is looked up before a source type or a function, and
    /// a command's own help before both, so one spelled like any of them
    /// would hide it or be hidden.
    #[test]
    fn no_doc_topic_is_a_type_a_function_or_a_command() {
        use clap::CommandFactory;
        let commands: Vec<String> = crate::Cli::command()
            .get_subcommands()
            .map(|c| c.get_name().to_string())
            .collect();
        for d in DOCS {
            assert!(SourceType::parse(d.topic).is_none(), "{}", d.topic);
            assert!(Function::parse(d.topic).is_none(), "{}", d.topic);
            assert!(!commands.iter().any(|c| c == d.topic), "{}", d.topic);
            assert_ne!(d.topic, DOCS_TOPIC);
        }
    }

    /// The release script writes `datalib-step help <topic>` to `<path>`
    /// for each line of `help docs` whose second word is a doc's path.
    #[test]
    fn the_docs_page_names_every_doc_as_the_release_script_reads_it() {
        let page = page(&[DOCS_TOPIC.into()], &commands(), "abc123").unwrap();
        let read: Vec<(&str, &str)> = page
            .lines()
            .filter_map(|l| {
                let mut words = l.split_whitespace();
                let (topic, path) = (words.next()?, words.next()?);
                (path.ends_with(".md") || path.ends_with(".toml")).then_some((topic, path))
            })
            .collect();
        let want: Vec<(&str, &str)> = DOCS.iter().map(|d| (d.topic, d.path)).collect();
        assert_eq!(read, want, "{page}");
    }

    #[test]
    fn every_source_type_has_a_page_with_its_own_sections() {
        for &t in SourceType::VARIANTS {
            let page = page(&[t.as_str().into()], &commands(), "abc123").unwrap();
            assert!(
                page.contains(&format!("`type = \"{}\"`", t.as_str())),
                "{t}: getting_your_data.md has no section for it:\n{page}"
            );
            assert!(
                page.starts_with("## ") || page.starts_with("This type covers"),
                "{t}: {page}"
            );
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

    /// `help email` said Gmail and Fastmail were at `datalib-step help
    /// email`, the page being read, when their sections are on it.
    #[test]
    fn a_source_page_never_sends_the_reader_to_itself() {
        for &t in SourceType::VARIANTS {
            let page = page(&[t.as_str().into()], &commands(), "abc123").unwrap();
            let itself = format!("(`datalib-step help {t}`)");
            assert!(!page.contains(&itself), "{t}:\n{page}");
        }
        let email = page_of(&["email"]);
        assert!(email.contains("Fastmail (§ \"Fastmail\")"), "{email}");
    }

    /// Whether `latchkey auth` works here depends on whether a gateway holds
    /// the credentials, which a page cannot know and a person can check.
    #[test]
    fn a_page_with_a_sign_in_says_what_to_do_behind_a_gateway() {
        for &t in SourceType::VARIANTS {
            let page = page(&[t.as_str().into()], &commands(), "abc123").unwrap();
            assert_eq!(
                page.contains("LATCHKEY_GATEWAY"),
                t.uses_latchkey_account(),
                "{t}:\n{page}"
            );
        }
    }

    /// The page with every line break a space, so a command `wrap` broke
    /// across lines still reads as one.
    fn one_line(page: &str) -> String {
        page.split_whitespace().collect::<Vec<_>>().join(" ")
    }

    /// `help calendar` told a person to run `datalib-step probe calendar`,
    /// which the probe refuses for a source with several ways in.
    #[test]
    fn every_probe_command_a_page_prints_is_one_the_probe_takes() {
        for &t in SourceType::VARIANTS {
            let pages = [
                page(&[t.as_str().into()], &commands(), "abc123").unwrap(),
                examples_page(t.as_str(), None).unwrap(),
            ]
            .map(|p| one_line(&p));
            let bare = format!("`datalib-step probe {t}`");
            let refused = crate::probe::live_methods(t).len() > 1;
            assert!(
                !refused || crate::probe::with_method(t, &serde_json::json!({}), None).is_err(),
                "{t}: the probe takes a bare probe after all"
            );
            for page in pages {
                assert!(
                    !(refused && page.contains(&bare)),
                    "{t}: the probe refuses {bare} with no --method:\n{page}"
                );
            }
        }
        let calendar = one_line(&page(&["calendar".into()], &commands(), "abc123").unwrap());
        assert!(
            calendar.contains("datalib-step probe calendar --method <"),
            "{calendar}"
        );
    }

    /// A stanza's comment ends up in a person's config, away from anything
    /// that could say when a sign-in command does not apply, so the command
    /// stays on the source's page.
    #[test]
    fn no_stanza_comment_names_a_sign_in_command() {
        for &t in SourceType::VARIANTS {
            let page = examples_page(t.as_str(), None).unwrap();
            for command in ["latchkey auth browser", "latchkey auth set"] {
                assert!(!page.contains(command), "{t}:\n{page}");
            }
        }
    }

    /// `datalib-dag init` writes the starter with no server to fill in a
    /// placeholder, and it told the reader to go to `<origin>/agent/config.md`.
    #[test]
    fn the_starter_config_names_a_page_that_exists() {
        let starter = datalib_dag::starter::STARTER_CONFIG;
        assert!(!starter.contains("<origin>"), "{starter}");
        let (_, rest) = starter
            .split_once("`datalib-step help ")
            .expect("the starter points at a help page");
        let (topic, _) = rest.split_once('`').unwrap();
        assert!(datalib_agent_docs::doc(topic).is_some(), "{topic}");
    }

    /// `help calendar` opened on "## Calendar (CalDAV)", so it read as if
    /// Google Calendar were not one of its ways in.
    #[test]
    fn a_type_reached_several_ways_names_them_all_first() {
        let page = page(&["calendar".into()], &commands(), "abc123").unwrap();
        let intro = page
            .split("## ")
            .next()
            .unwrap_or_default()
            .replace('\n', " ");
        for way in [
            "Calendar (CalDAV)",
            "Calendly",
            "Fastmail Calendar",
            "Google Calendar",
        ] {
            assert!(intro.contains(way), "{way} missing from {intro:?}");
        }
    }

    /// Whether the text ending at `before` is inside a link's `[…]`.
    fn in_link_text(before: &str) -> bool {
        before.rfind('[') > before.rfind(']')
    }

    /// A release has no source tree, so a path into it in a guide is a
    /// link to the raw file at this build's commit, never bare text.
    #[test]
    fn no_guide_names_a_source_tree_path_outside_a_link() {
        let mut topics: Vec<String> = SourceType::VARIANTS
            .iter()
            .map(|t| t.as_str().into())
            .collect();
        topics.extend(
            DOCS.iter()
                .filter(|d| d.kind == datalib_agent_docs::Kind::Guide)
                .map(|d| d.topic.to_string()),
        );
        for topic in topics {
            let page = page(std::slice::from_ref(&topic), &commands(), "abc123").unwrap();
            for (i, _) in page.match_indices("datalib/backend/") {
                assert!(
                    page[..i].ends_with("abc123/") || in_link_text(&page[..i]),
                    "{topic}: a bare source path: {}",
                    &page[i.saturating_sub(40)..(i + 60).min(page.len())]
                );
            }
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
    fn the_examples_page_shows_every_stanza_of_every_method() {
        for &t in SourceType::VARIANTS {
            let page = examples_page(t.as_str(), None).unwrap();
            for m in ingest_methods(t) {
                for &kind in ExampleKind::VARIANTS {
                    let Some(example) = source_docs(t).example(kind, m.path) else {
                        assert_eq!(kind, ExampleKind::Sections, "{t}/{}", m.path);
                        continue;
                    };
                    assert!(
                        page.contains(&format!("## {t} by `{}`, {}", m.path, kind.as_str())),
                        "{t}/{}: no heading:\n{page}",
                        m.path
                    );
                    assert!(page.contains(example.toml.trim_end()), "{t}/{}", m.path);
                }
            }
        }
        let email = examples_page("email", None).unwrap();
        assert!(email.contains("jmap, gmail, mbox"), "{email}");
        let garmin = examples_page("garmin", None).unwrap();
        assert!(
            garmin.contains("unified_time_series_index/time_series_index"),
            "{garmin}"
        );
        let err = examples_page("slak", None).unwrap_err();
        assert!(err.contains("\"slak\""), "{err}");
    }

    /// `--method` prints one method's stanzas, to read before appending.
    #[test]
    fn the_examples_page_narrows_to_one_method() {
        let gmail = examples_page("email", Some("gmail")).unwrap();
        assert!(gmail.contains("## email by `gmail`, minimal"), "{gmail}");
        assert!(gmail.contains("## email by `gmail`, complete"), "{gmail}");
        assert!(!gmail.contains("## email by `jmap`"), "{gmail}");
        assert!(!gmail.contains("## email by `mbox`"), "{gmail}");
        let err = examples_page("email", Some("imap")).unwrap_err();
        assert!(
            err.contains("no method \"imap\"") && err.contains("gmail"),
            "{err}"
        );
    }

    #[test]
    fn an_unknown_method_is_refused_with_the_known_ones() {
        let err = page(&["email".into(), "imap".into()], &commands(), "abc123").unwrap_err();
        assert!(err.contains("no method \"imap\""), "{err}");
        assert!(err.contains("jmap, gmail, mbox"), "{err}");
    }

    #[test]
    fn a_method_page_lists_its_own_options_and_the_shared_ones() {
        let page = page(&["slack".into(), "api".into()], &commands(), "abc123").unwrap();
        assert!(
            page.contains("[steps.params.api]\n  all_channels  (boolean"),
            "{page}"
        );
        assert!(
            page.contains("[steps.params.common.download_params]"),
            "{page}"
        );
        assert!(page.contains("[steps.params.latchkey_settings]"), "{page}");
        let email = page_of(&["email", "gmail"]);
        assert!(
            !email.contains("[steps.params.jmap]"),
            "another method's options:\n{email}"
        );
    }

    fn page_of(words: &[&str]) -> String {
        let topic: Vec<String> = words.iter().map(|w| w.to_string()).collect();
        page(&topic, &commands(), "abc123").unwrap()
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
