//! The worked examples at the end of a binary's `--help`, kept as data so
//! a test can run every one through that binary's own parser. An example
//! that no longer parses fails the build instead of teaching an agent a
//! command that does not work.

pub struct Example {
    /// What running it does, in a few words.
    pub what: &'static str,
    /// The whole command line, binary name first, as a person would type it.
    pub command: &'static str,
}

pub fn render(examples: &[Example]) -> String {
    let mut out = String::from("Examples:\n");
    for e in examples {
        out.push_str(&format!("  # {}\n  {}\n\n", e.what, e.command));
    }
    out.truncate(out.trim_end().len());
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

    #[test]
    fn a_good_example_passes_and_renders() {
        let examples = [Example {
            what: "do it fast",
            command: "tool '~/my dir' --fast",
        }];
        assert_examples_parse(cmd(), &examples);
        assert_eq!(
            render(&examples),
            "Examples:\n  # do it fast\n  tool '~/my dir' --fast"
        );
    }
}
