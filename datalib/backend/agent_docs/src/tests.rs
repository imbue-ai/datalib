use super::*;

const REF: &str = "abc123";

/// The files a guide links to that are not compiled in, each printed as
/// its raw file at the build's commit. Contributor material only: a doc an
/// agent needs to use datalib goes in `DOCS` instead.
const GUIDE_LINKS_SERVED_RAW: &[&str] = &[
    "AGENTS.md",
    "docs/dev/docker.md",
    "datalib/backend/runtime/src/node_runtime.rs",
    "datalib/backend/etl/providers/calendar/INGEST.md",
    "datalib/backend/etl/providers/linear/INGEST.md",
    "datalib/backend/etl/providers/todoist/INGEST.md",
    "datalib/backend/etl/providers/whatsapp/INGEST.md",
];

fn markdown_docs() -> impl Iterator<Item = &'static Doc> {
    DOCS.iter().filter(|d| d.kind != Kind::Config)
}

#[test]
fn links_resolve_against_the_docs_own_directory() {
    let repo = |path: &str, fragment: Option<&str>| Target::Repo {
        path: path.to_string(),
        fragment: fragment.map(str::to_string),
    };
    assert_eq!(
        resolve("../AGENTS.md", "docs/guide.md"),
        repo("AGENTS.md", None)
    );
    assert_eq!(
        resolve("dev/x.md#sec", "docs/guide.md"),
        repo("docs/dev/x.md", Some("sec"))
    );
    assert_eq!(
        resolve("#here", "docs/guide.md"),
        repo("docs/guide.md", Some("here"))
    );
    assert_eq!(
        resolve("/datalib/backend/etl/README.md", "docs/dev/doltlite.md"),
        repo("datalib/backend/etl/README.md", None)
    );
    assert_eq!(
        resolve("https://e.org/f", "docs/guide.md"),
        Target::External
    );
    assert_eq!(
        resolve("mailto:a@b.org \"A <a@b.org>\"", "docs/guide.md"),
        Target::External
    );
}

#[test]
fn a_link_to_an_embedded_doc_prints_as_the_command_that_prints_it() {
    let md = "See [the guide](../agent_user.md), [windows](../dev/config_model.md#time-windows-since-and-until), \
              [Gmail](getting_your_data.md#gmail), [the code](../../datalib/backend/etl/README.md), \
              [the web](https://e.org/x), ![a picture](/data/x/ingest/blob/ab) and `[code](x.md)`.";
    assert_eq!(
        print_links(md, "docs/user/searching.md", REF, Reader::WholeDoc),
        "See the guide (`datalib-step help agent`), windows (`datalib-step help config-model`, \
         § \"Time windows: `since` and `until`\"), Gmail (`datalib-step help email`), \
         [the code](https://raw.githubusercontent.com/imbue-ai/datalib/abc123/datalib/backend/etl/README.md), \
         [the web](https://e.org/x), ![a picture](/data/x/ingest/blob/ab) and `[code](x.md)`."
    );
}

#[test]
fn an_anchor_in_the_doc_being_printed_names_its_heading() {
    let md = "Read [the windows](#time-windows-since-and-until) first.";
    assert_eq!(
        print_links(md, "docs/dev/config_model.md", REF, Reader::WholeDoc),
        "Read the windows (§ \"Time windows: `since` and `until`\") first."
    );
    assert_eq!(
        print_links(
            md,
            "docs/dev/config_model.md",
            REF,
            Reader::SourcePage("slack")
        ),
        "Read the windows (`datalib-step help config-model`, § \"Time windows: `since` and \
         `until`\") first."
    );
}

/// A source's page holds every section of its type, so a link to one of
/// them must not print as the command for the page already being read.
#[test]
fn a_section_on_the_page_being_read_is_named_not_commanded() {
    let md = "[Gmail](#gmail) and [Slack](#slack).";
    assert_eq!(
        print_links(md, GETTING_YOUR_DATA, REF, Reader::SourcePage("email")),
        "Gmail (§ \"Gmail\") and Slack (`datalib-step help slack`)."
    );
}

#[test]
fn a_link_inside_a_code_block_is_left_alone() {
    let md = "```sh\necho '[a](b.md)'\n```\n[c](../agent_user.md)\n";
    assert_eq!(
        print_links(md, "docs/user/x.md", REF, Reader::WholeDoc),
        "```sh\necho '[a](b.md)'\n```\nc (`datalib-step help agent`)\n"
    );
}

#[test]
fn a_link_whose_text_runs_over_a_line_is_found() {
    let found = links("and [running in\nDocker](user/docker.md) is");
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].text, "running in\nDocker");
    assert_eq!(found[0].line, 2);
}

#[test]
fn slugs_match_githubs() {
    assert_eq!(slug("Calendar (CalDAV)"), "calendar-caldav");
    assert_eq!(slug("3. Configuration"), "3-configuration");
    assert_eq!(
        slug("Time windows: `since` and `until`"),
        "time-windows-since-and-until"
    );
    assert_eq!(
        slug("datalib — agent user guide"),
        "datalib--agent-user-guide"
    );
}

#[test]
fn topics_and_paths_are_unique() {
    for (i, a) in DOCS.iter().enumerate() {
        for b in &DOCS[i + 1..] {
            assert_ne!(a.topic, b.topic);
            assert_ne!(a.path, b.path);
        }
    }
}

/// An anchor that names no heading prints without one, so a renamed
/// heading would quietly send the reader to the top of a long doc.
#[test]
fn every_anchor_into_an_embedded_doc_names_a_heading() {
    let mut broken = Vec::new();
    for d in markdown_docs() {
        for link in links(d.text) {
            let Target::Repo {
                path,
                fragment: Some(f),
            } = resolve(link.target, d.path)
            else {
                continue;
            };
            if let Some(target) = by_path(&path) {
                if heading_for(target, &f).is_none() {
                    broken.push(format!("{}:{}: {}", d.path, link.line, link.target));
                }
            }
        }
    }
    assert!(
        broken.is_empty(),
        "anchors with no heading:\n{}",
        broken.join("\n")
    );
}

/// The guides are what an agent is sent to first. A link from one to a
/// doc that is not compiled in leaves it a raw file to fetch, so each is
/// named above, on purpose; the rest go in `DOCS`.
#[test]
fn a_guide_links_only_to_embedded_docs_or_to_the_named_contributor_files() {
    let mut unlisted = Vec::new();
    let mut used = std::collections::BTreeSet::new();
    for d in DOCS.iter().filter(|d| d.kind == Kind::Guide) {
        for link in links(d.text) {
            let Target::Repo { path, .. } = resolve(link.target, d.path) else {
                continue;
            };
            if by_path(&path).is_some() {
                continue;
            }
            if GUIDE_LINKS_SERVED_RAW.contains(&path.as_str()) {
                used.insert(path);
            } else {
                unlisted.push(format!("{}:{}: {path}", d.path, link.line));
            }
        }
    }
    assert!(
        unlisted.is_empty(),
        "a guide links to a file that is not embedded; add it to DOCS, or drop the link:\n{}",
        unlisted.join("\n")
    );
    for path in GUIDE_LINKS_SERVED_RAW {
        assert!(
            used.contains(*path),
            "{path} is listed but no guide links to it"
        );
    }
}

/// A printed page is read with no repo beside it, so no link may be
/// relative, and none may lead to a GitHub page: a doc's HTML page there
/// is fifteen times the file and overflows an agent's tool output.
#[test]
fn no_printed_page_has_a_relative_link_or_a_github_page() {
    for d in DOCS {
        let page = page(d, REF);
        for link in links(&page) {
            assert!(
                resolve(link.target, d.path) == Target::External,
                "{}:{}: a relative link is left: {}",
                d.path,
                link.line,
                link.target
            );
        }
        for page_url in [
            "github.com/imbue-ai/datalib/blob/",
            "github.com/imbue-ai/datalib/tree/",
        ] {
            assert!(!page.contains(page_url), "{}: links to {page_url}", d.path);
        }
    }
}

/// A command in a doc names a program, and every program it may name is
/// one a test parses or one listed as having no parser.
#[test]
fn every_program_the_docs_run_is_checked_or_says_why_not() {
    let mut problems = Vec::new();
    for d in markdown_docs() {
        for c in commands(d.text) {
            match &c.argv {
                None => problems.push(format!(
                    "{}:{}: not a line a shell could split: {}",
                    d.path, c.line, c.text
                )),
                Some(argv) => {
                    let known = CHECKED_PROGRAMS.contains(&argv[0].as_str())
                        || UNCHECKED_PROGRAMS.iter().any(|(p, _)| *p == argv[0]);
                    if !known {
                        problems.push(format!("{}:{}: {}", d.path, c.line, argv[0]));
                    }
                }
            }
        }
    }
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}

fn argvs(md: &str) -> Vec<Vec<String>> {
    commands(md).into_iter().filter_map(|c| c.argv).collect()
}

fn words(s: &str) -> Vec<String> {
    s.split(' ').map(str::to_string).collect()
}

#[test]
fn commands_are_found_wherever_the_shell_would_run_them() {
    let md = r#"
```sh
datalib-dag <data_root>/config.toml --sync a/ingest   # one source
RUST_LOG=debug datalib-step probe slack \
  --list channels
docker run --rm -v "$D:/data" "$IMG" datalib-dag /data/config.toml
datalib-doltlite -readonly db.sqlite .dump | sqlite3 out.sqlite
id=$(datalib-step help agent) && echo "$id" > out.md
~/.local/bin/datalib-applet search ~/datalib 'warp core' 2>/dev/null
cat > config.toml <<'EOF'
command = "datalib-dag x"
EOF
datalib-dag "a b
c" --check
```

```toml
command = "datalib-step"
```
"#;
    assert_eq!(
        argvs(md),
        vec![
            words("datalib-dag <data_root>/config.toml --sync a/ingest"),
            words("datalib-step probe slack --list channels"),
            words("datalib-dag /data/config.toml"),
            words("datalib-doltlite -readonly db.sqlite .dump"),
            words("datalib-step help agent"),
            vec![
                "datalib-applet".to_string(),
                "search".to_string(),
                "~/datalib".to_string(),
                "warp core".to_string()
            ],
            vec![
                "datalib-dag".to_string(),
                "a b\nc".to_string(),
                "--check".to_string()
            ],
        ]
    );
    let lines: Vec<usize> = commands(md).iter().map(|c| c.line).collect();
    assert_eq!(lines, vec![3, 4, 6, 7, 8, 9, 13]);
}

#[test]
fn an_unclosed_quote_is_reported_rather_than_skipped() {
    let found = commands("```sh\ndatalib-dag 'x --check\n```\n");
    assert_eq!(found.len(), 1);
    assert!(found[0].argv.is_none());
}

#[test]
fn a_getting_your_data_section_names_its_type() {
    assert_eq!(section_type("Gmail"), Some("email"));
    assert_eq!(section_type("Slack"), Some("slack"));
    assert_eq!(section_type("Your own Google OAuth client"), None);
}
