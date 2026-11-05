//! The docs `datalib-step help <topic>` prints whole, compiled in, and how
//! their links are printed: a link to another doc here becomes the
//! `datalib-step help` command that prints it, so nothing an agent reads
//! sends it to GitHub for a page it already has. Also what the tests need
//! to check the docs' examples: their fenced blocks and the command lines
//! in them.

mod commands;
mod markdown;

pub use commands::{commands, Command, CHECKED_PROGRAMS, PROGRAMS, UNCHECKED_PROGRAMS};
pub use markdown::{fenced_blocks, headings, links, slug, Block, Link};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Written for someone using datalib; every doc it links to is here
    /// too, or named in the test that holds it to that.
    Guide,
    /// Written for contributors as well, compiled in because a guide sends
    /// an agent to it; its links to the rest of the tree go to the raw file.
    Reference,
    /// A config file, printed as it is.
    Config,
}

pub struct Doc {
    pub topic: &'static str,
    pub path: &'static str,
    pub kind: Kind,
    pub about: &'static str,
    pub text: &'static str,
}

pub const GETTING_YOUR_DATA: &str = "docs/user/getting_your_data.md";

/// One applet, as `datalib-step help` lists it. `datalib-applet`'s tests
/// hold this list to its subcommands.
pub struct Applet {
    pub name: &'static str,
    pub does: &'static str,
    /// The doc that says more, which `datalib-step help <name>` prints.
    pub topic: &'static str,
}

pub const APPLETS: &[Applet] = &[
    Applet {
        name: "unified_document_index",
        does: "search, the grid and the documents, and what each source knows about a handle",
        topic: "applets",
    },
    Applet {
        name: "datalib_contacts",
        does: "the contacts app: link handles to contacts, label people and senders, and \
               review what an agent proposed",
        topic: "contacts-app",
    },
    Applet {
        name: "unified_time_series_index",
        does: "the time series, and their points over a range",
        topic: "applets",
    },
    Applet {
        name: "photos",
        does: "photos and videos: a page of them, one's details, its pictures",
        topic: "applets",
    },
    Applet {
        name: "slack",
        does: "one Slack mirror's channels and their threads",
        topic: "applets",
    },
];

pub const DOCS: &[Doc] = &[
    Doc {
        topic: "agent",
        path: "docs/agent_user.md",
        kind: Kind::Guide,
        about: "the agent guide: the data root, syncing, logs, reading the data, troubleshooting",
        text: include_str!("../../../../docs/agent_user.md"),
    },
    Doc {
        topic: "first-time",
        path: "docs/user/first_time_user.md",
        kind: Kind::Guide,
        about: "a person's walkthrough: install, credentials, the config, a first sync",
        text: include_str!("../../../../docs/user/first_time_user.md"),
    },
    Doc {
        topic: "getting-your-data",
        path: GETTING_YOUR_DATA,
        kind: Kind::Guide,
        about: "every source's sign-in or export; `help <type>` prints one source's part",
        text: include_str!("../../../../docs/user/getting_your_data.md"),
    },
    Doc {
        topic: "docker",
        path: "docs/user/docker.md",
        kind: Kind::Guide,
        about: "running datalib in Docker, with a demo library",
        text: include_str!("../../../../docs/user/docker.md"),
    },
    Doc {
        topic: "raw-stores",
        path: "docs/user/raw_stores.md",
        kind: Kind::Guide,
        about: "what each source's raw store holds, and how to read and follow it",
        text: include_str!("../../../../docs/user/raw_stores.md"),
    },
    Doc {
        topic: "apps",
        path: "docs/user/building_on_datalib.md",
        kind: Kind::Guide,
        about: "building an app on datalib: syncing, freshness, a step of your own",
        text: include_str!("../../../../docs/user/building_on_datalib.md"),
    },
    Doc {
        topic: "search",
        path: "docs/user/searching.md",
        kind: Kind::Guide,
        about: "the search's query language: keys, people, `me`, free text",
        text: include_str!("../../../../docs/user/searching.md"),
    },
    Doc {
        topic: "applets",
        path: "docs/user/applets.md",
        kind: Kind::Guide,
        about: "the servers the app runs: what each serves, and how to call them",
        text: include_str!("../../../../docs/user/applets.md"),
    },
    Doc {
        topic: "contacts-app",
        path: "docs/user/contacts_app.md",
        kind: Kind::Guide,
        about: "who a handle is: finding handles, proposing links and labels to a person",
        text: include_str!("../../../../docs/user/contacts_app.md"),
    },
    Doc {
        topic: "step-protocol",
        path: "docs/dev/step_protocol.md",
        kind: Kind::Guide,
        about: "writing a step of your own: what it is handed, what it reports",
        text: include_str!("../../../../docs/dev/step_protocol.md"),
    },
    Doc {
        topic: "runner",
        path: "datalib/backend/dag/README.md",
        kind: Kind::Reference,
        about: "the runner's rules: when a step runs, what it reads, what keeps steps apart",
        text: include_str!("../../dag/README.md"),
    },
    Doc {
        topic: "config-model",
        path: "docs/dev/config_model.md",
        kind: Kind::Reference,
        about: "what a config is made of: groups, steps, ingest methods, time windows",
        text: include_str!("../../../../docs/dev/config_model.md"),
    },
    Doc {
        topic: "logging",
        path: "docs/dev/logging.md",
        kind: Kind::Reference,
        about: "the run store: every log line, step state and metric, and reading it",
        text: include_str!("../../../../docs/dev/logging.md"),
    },
    Doc {
        topic: "doltlite",
        path: "docs/dev/doltlite.md",
        kind: Kind::Reference,
        about: "the store engine: reading one commit, diffs, locks, exporting to SQLite",
        text: include_str!("../../../../docs/dev/doltlite.md"),
    },
    Doc {
        topic: "grid-rows",
        path: "docs/dev/grid_rows.md",
        kind: Kind::Reference,
        about: "every column of the grid index's `grid_rows` table",
        text: include_str!("../../../../docs/dev/grid_rows.md"),
    },
    Doc {
        topic: "edges",
        path: "docs/dev/edges.md",
        kind: Kind::Reference,
        about: "the links between documents",
        text: include_str!("../../../../docs/dev/edges.md"),
    },
    Doc {
        topic: "people",
        path: "docs/dev/contacts.md",
        kind: Kind::Reference,
        about: "how handles, source contacts and the contacts app fit together, every route \
                and table",
        text: include_str!("../../../../docs/dev/contacts.md"),
    },
    Doc {
        topic: "writing-applets",
        path: "docs/dev/applets.md",
        kind: Kind::Reference,
        about: "writing an applet of your own: the contract its command honours",
        text: include_str!("../../../../docs/dev/applets.md"),
    },
    Doc {
        topic: "dag-example",
        path: "configs/dag_example.toml",
        kind: Kind::Config,
        about: "a complete config with every line commented",
        text: include_str!("../../../../configs/dag_example.toml"),
    },
    Doc {
        topic: "all-sources",
        path: "docs/user/config_examples/all_sources.toml",
        kind: Kind::Config,
        about: "every source, every way in, every option set",
        text: include_str!("../../../../docs/user/config_examples/all_sources.toml"),
    },
    Doc {
        topic: "sample-config",
        path: "docs/user/config_examples/sample_config.toml",
        kind: Kind::Config,
        about: "Slack, Claude, and email from a Takeout `.mbox`",
        text: include_str!("../../../../docs/user/config_examples/sample_config.toml"),
    },
    Doc {
        topic: "claude-only",
        path: "docs/user/config_examples/claude_only.toml",
        kind: Kind::Config,
        about: "just the Claude source and the index steps",
        text: include_str!("../../../../docs/user/config_examples/claude_only.toml"),
    },
];

const REPO_RAW: &str = "https://raw.githubusercontent.com/imbue-ai/datalib";

pub fn doc(topic: &str) -> Option<&'static Doc> {
    DOCS.iter().find(|d| d.topic == topic)
}

pub fn by_path(path: &str) -> Option<&'static Doc> {
    DOCS.iter().find(|d| d.path == path)
}

/// The doc as `datalib-step help <topic>` prints it, and as a release
/// writes it beside the binaries.
pub fn page(doc: &Doc, git_ref: &str) -> String {
    match doc.kind {
        Kind::Config => doc.text.to_string(),
        Kind::Guide | Kind::Reference => print_links(doc.text, doc.path, git_ref, true),
    }
}

/// Where a link in a doc leads, once resolved against the doc's path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// A URL, left as written.
    External,
    /// A file in the repo, by its path from the root, and a heading's
    /// anchor in it.
    Repo {
        path: String,
        fragment: Option<String>,
    },
}

pub fn resolve(target: &str, doc_path: &str) -> Target {
    let target = target.split_whitespace().next().unwrap_or("");
    if target.contains("://") || target.starts_with("mailto:") {
        return Target::External;
    }
    let (path, fragment) = match target.split_once('#') {
        Some((p, f)) => (p, Some(f.to_string())),
        None => (target, None),
    };
    if path.is_empty() {
        return Target::Repo {
            path: doc_path.to_string(),
            fragment,
        };
    }
    let mut parts: Vec<&str> = match path.strip_prefix('/') {
        Some(_) => Vec::new(),
        None => doc_path
            .rsplit_once('/')
            .map_or(Vec::new(), |(dir, _)| dir.split('/').collect()),
    };
    for segment in path.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            s => parts.push(s),
        }
    }
    Target::Repo {
        path: parts.join("/"),
        fragment,
    }
}

/// Every command line in the docs that runs `program`, with where it is
/// (`docs/agent_user.md:138`), for that program's tests to parse.
pub fn commands_running(program: &str) -> Vec<(String, Vec<String>)> {
    DOCS.iter()
        .filter(|d| d.kind != Kind::Config)
        .flat_map(|d| {
            commands(d.text).into_iter().filter_map(move |c| {
                let argv = c.argv?;
                (argv[0] == program).then(|| (format!("{}:{}", d.path, c.line), argv))
            })
        })
        .collect()
}

/// The heading an anchor names in an embedded doc, by GitHub's slug.
pub fn heading_for(doc: &Doc, fragment: &str) -> Option<&'static str> {
    headings(doc.text).into_iter().find(|h| slug(h) == fragment)
}

/// The one source type a section of getting_your_data.md declares, so a
/// link to that section prints as the command for that source's page,
/// which is the section and no more.
pub fn section_type(heading: &str) -> Option<&'static str> {
    let doc = by_path(GETTING_YOUR_DATA)?;
    let start = doc.text.find(&format!("\n## {heading}\n"))? + 1;
    let rest = &doc.text[start + 3..];
    let section = &rest[..rest.find("\n## ").unwrap_or(rest.len())];
    let mut types = section.lines().filter_map(|l| {
        l.strip_prefix("`type = \"")
            .and_then(|r| r.split_once('"'))
            .map(|(t, _)| t)
    });
    let first = types.next()?;
    types.all(|t| t == first).then_some(first)
}

/// Every link in `markdown` printed for a reader with no repo: one to an
/// embedded doc as the command that prints it, one to anything else in
/// the repo as its raw file at `git_ref`. `whole` says the reader has all
/// of the doc at `doc_path`, so an anchor in it can name its heading alone.
pub fn print_links(markdown: &str, doc_path: &str, git_ref: &str, whole: bool) -> String {
    let mut out = String::with_capacity(markdown.len());
    let mut last = 0;
    for link in links(markdown) {
        out.push_str(&markdown[last..link.range.start]);
        out.push_str(&print_link(&link, doc_path, git_ref, whole));
        last = link.range.end;
    }
    out.push_str(&markdown[last..]);
    out
}

fn print_link(link: &Link, doc_path: &str, git_ref: &str, whole: bool) -> String {
    let text = link.text;
    let Target::Repo { path, fragment } = resolve(link.target, doc_path) else {
        return format!("[{text}]({})", link.target);
    };
    let Some(target) = by_path(&path) else {
        return format!("[{text}]({REPO_RAW}/{git_ref}/{path})");
    };
    let heading = fragment.as_deref().and_then(|f| heading_for(target, f));
    if let Some(h) = heading {
        if target.path == GETTING_YOUR_DATA {
            if let Some(t) = section_type(h) {
                return format!("{text} (`datalib-step help {t}`)");
            }
        }
        if whole && target.path == doc_path {
            return format!("{text} (§ \"{h}\")");
        }
        return format!("{text} (`datalib-step help {}`, § \"{h}\")", target.topic);
    }
    format!("{text} (`datalib-step help {}`)", target.topic)
}

#[cfg(test)]
mod tests;
