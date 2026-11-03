//! The worked examples at the end of a binary's `--help`, kept as data so
//! a test can run every one through that binary's own parser, and the same
//! check for the commands the docs show (`datalib_agent_docs`). An example
//! that no longer parses fails the build instead of teaching an agent a
//! command that does not work.

pub struct Example {
    /// What running it does, in a few words.
    pub what: &'static str,
    /// The whole command line, binary name first, as a person would type it.
    pub command: &'static str,
}

/// The line every binary's help ends with: where to begin when setting
/// datalib up, whichever binary an agent happened to ask first.
pub const START_HERE: &str = "Setting datalib up? Start with `datalib-step help`.";

pub fn render(examples: &[Example]) -> String {
    let mut out = String::from("Examples:\n");
    for e in examples {
        out.push_str(&format!("  # {}\n  {}\n\n", e.what, e.command));
    }
    out.push_str(START_HERE);
    out
}

pub fn assert_examples_parse(cmd: clap::Command, examples: &[Example]) {
    assert!(!examples.is_empty(), "{} shows no examples", cmd.get_name());
    let mut failures = Vec::new();
    for e in examples {
        let Some(words) = shlex::split(e.command) else {
            failures.push(format!("{:?}: not a shell command line", e.command));
            continue;
        };
        let argv: Vec<String> = words
            .into_iter()
            .skip_while(|w| is_env_assignment(w))
            .collect();
        if argv.first().map(String::as_str) != Some(cmd.get_name()) {
            failures.push(format!(
                "{:?}: does not start with {:?}",
                e.command,
                cmd.get_name()
            ));
            continue;
        }
        if let Err(err) = cmd.clone().try_get_matches_from(&argv) {
            failures.push(format!("{:?}:\n{err}", e.command));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// The command lines a doc shows, each with where it is, put through the
/// binary's parser. `--help` and `--version` count as parsing.
pub fn assert_argvs_parse(cmd: clap::Command, commands: &[(String, Vec<String>)]) {
    use clap::error::ErrorKind;
    let mut failures = Vec::new();
    for (place, argv) in commands {
        if argv.first().map(String::as_str) != Some(cmd.get_name()) {
            failures.push(format!(
                "{place}: {argv:?} does not run {:?}",
                cmd.get_name()
            ));
            continue;
        }
        match cmd.clone().try_get_matches_from(argv) {
            Ok(_) => {}
            Err(e) if matches!(e.kind(), ErrorKind::DisplayHelp | ErrorKind::DisplayVersion) => {}
            Err(e) => failures.push(format!(
                "{place}: {}\n{e}",
                shlex::try_join(argv.iter().map(String::as_str)).unwrap_or_default()
            )),
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

fn is_env_assignment(word: &str) -> bool {
    word.split_once('=').is_some_and(|(name, _)| {
        !name.is_empty()
            && !name.starts_with(|c: char| c.is_ascii_digit())
            && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cmd() -> clap::Command {
        clap::Command::new("tool")
            .arg(clap::Arg::new("path").required(true))
            .arg(clap::Arg::new("fast").long("fast").num_args(0))
    }

    /// The check has to fail for a flag the parser does not know, or it
    /// vouches for every example whatever it says.
    #[test]
    #[should_panic(expected = "--slow")]
    fn an_unknown_flag_fails() {
        assert_examples_parse(
            cmd(),
            &[Example {
                what: "x",
                command: "tool a --slow",
            }],
        );
    }

    #[test]
    #[should_panic(expected = "does not start with")]
    fn another_binarys_command_fails() {
        assert_examples_parse(
            cmd(),
            &[Example {
                what: "x",
                command: "other a",
            }],
        );
    }

    #[test]
    fn environment_assignments_before_the_binary_are_skipped() {
        assert_examples_parse(
            cmd(),
            &[Example {
                what: "x",
                command: "A_B=1 C=x tool a",
            }],
        );
    }

    /// A doc's command with a flag the binary does not take fails, named
    /// by where it is.
    #[test]
    #[should_panic(expected = "guide.md:3")]
    fn a_doc_command_with_an_unknown_flag_fails() {
        let argv = ["tool", "a", "--slow"].map(String::from).to_vec();
        assert_argvs_parse(cmd(), &[("guide.md:3".to_string(), argv)]);
    }

    #[test]
    fn a_doc_command_asking_for_help_parses() {
        let argv = ["tool", "--help"].map(String::from).to_vec();
        assert_argvs_parse(cmd(), &[("guide.md:3".to_string(), argv)]);
    }

    #[test]
    fn a_good_example_passes_and_renders() {
        let examples = [Example {
            what: "do it fast",
            command: "tool '~/my dir' --fast",
        }];
        assert_examples_parse(cmd(), &examples);
        assert_eq!(
            render(&examples),
            format!("Examples:\n  # do it fast\n  tool '~/my dir' --fast\n\n{START_HERE}")
        );
    }
}
