//! A `[[programs]]` entry for `datalib-step` whose `verbs` leave out
//! `check`. The runner sends `--check` only to a program that lists it,
//! and `datalib-step` answers it with what a sync would make of its
//! step's params (`docs/dev/step_protocol.md` § Check), so the verb is
//! added: without it, a save and `datalib-pipeline check` would let a
//! misspelled param through to the first sync.
//!
//! A text edit: every comment and key the file holds survives. An entry
//! whose `verbs` is not one line under a `[[programs]]` header is
//! refused, naming the program, so a person adds the word by hand. An
//! entry with no `verbs` at all takes none on purpose and is left alone.

use anyhow::{bail, Context as _, Result};

use crate::program_pass_env::entry_lines;
use crate::retired::is_datalib_step;

const VERB: &str = "check";

/// The `command` of each `datalib-step` entry whose `verbs` lack `check`.
fn unchecked(text: &str) -> Result<Vec<String>> {
    let doc: toml::Value = toml::from_str(text).context("parse the config")?;
    let programs = doc
        .get("programs")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    Ok(programs
        .iter()
        .filter(|p| {
            p.get("verbs")
                .and_then(|v| v.as_array())
                .is_some_and(|verbs| !verbs.iter().any(|v| v.as_str() == Some(VERB)))
        })
        .filter_map(|p| p.get("command").and_then(|c| c.as_str()))
        .filter(|c| is_datalib_step(c.trim()))
        .map(str::to_string)
        .collect())
}

pub fn is_retired(text: &str) -> Result<bool> {
    Ok(!unchecked(text)?.is_empty())
}

pub fn rewrite(text: &str) -> Result<String> {
    let mut out = text.to_string();
    let mut by_hand = Vec::new();
    for command in unchecked(text)? {
        match closing_bracket(&out, &command) {
            Some((at, empty)) => {
                let word = if empty {
                    format!("\"{VERB}\"")
                } else {
                    format!(", \"{VERB}\"")
                };
                out.insert_str(at, &word);
            }
            None => by_hand.push(command),
        }
    }
    if !by_hand.is_empty() {
        bail!(
            "these `[[programs]]` entries for datalib-step leave `{VERB}` out of `verbs`, and \
             their `verbs` is not one line this rewrite can add to: {}; add \"{VERB}\" to each",
            by_hand.join(", ")
        );
    }
    Ok(out)
}

/// Where the `]` of the entry's one-line `verbs = [...]` is, and whether
/// the array is empty.
fn closing_bracket(text: &str, command: &str) -> Option<(usize, bool)> {
    let lines = entry_lines(text, command)?;
    let mut at = lines.start;
    for line in text[lines.clone()].split_inclusive('\n') {
        let start = at;
        at += line.len();
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if key.trim() != "verbs" {
            continue;
        }
        let open = value.find('[')?;
        let close = value.rfind(']')?;
        let after = value[close + 1..].trim();
        if !value[..open].trim().is_empty() || !(after.is_empty() || after.starts_with('#')) {
            return None;
        }
        let empty = value[open + 1..close].trim().is_empty();
        return Some((start + key.len() + 1 + close, empty));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn verbs_of(text: &str, command: &str) -> Vec<String> {
        let doc: toml::Value = toml::from_str(text).unwrap();
        doc["programs"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["command"].as_str() == Some(command))
            .unwrap()["verbs"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect()
    }

    #[test]
    fn the_verb_is_added_at_the_end_and_comments_stay() {
        let text = "[[programs]]\n# mine\ncommand = \"datalib-step\"\n\
                    verbs = [\"reset\", \"prepare\"]  # the verbs\n\n\
                    [[programs]]\ncommand = \"./bin/mine\"\nverbs = [\"reset\"]\n";
        assert!(is_retired(text).unwrap());
        let out = rewrite(text).unwrap();
        assert_eq!(
            verbs_of(&out, "datalib-step"),
            ["reset", "prepare", "check"]
        );
        assert_eq!(
            verbs_of(&out, "./bin/mine"),
            ["reset"],
            "another program is left alone"
        );
        assert!(
            out.contains("# mine\n") && out.contains("# the verbs"),
            "{out}"
        );
        assert!(!is_retired(&out).unwrap());
    }

    #[test]
    fn an_empty_list_gets_the_one_word_and_no_list_is_left_alone() {
        let out = rewrite("[[programs]]\ncommand = \"datalib-step\"\nverbs = []\n").unwrap();
        assert_eq!(verbs_of(&out, "datalib-step"), ["check"]);
        assert!(!is_retired("[[programs]]\ncommand = \"datalib-step\"\n").unwrap());
    }

    #[test]
    fn a_list_over_several_lines_is_refused_by_name() {
        let text = "[[programs]]\ncommand = \"datalib-step\"\nverbs = [\n  \"reset\",\n]\n";
        let err = rewrite(text).unwrap_err().to_string();
        assert!(err.contains("datalib-step"), "{err}");
    }
}
