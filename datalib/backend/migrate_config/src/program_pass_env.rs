//! A `[[programs]]` entry for `datalib-step` that names no `pass_env`.
//! The runner hands a step only the variables every program needs, the
//! ones its params name, and the ones its program's `pass_env` names
//! (`docs/dev/step_protocol.md` § "What your command receives"), so
//! `datalib-step`'s list is written into its entry: without it, the
//! fingerprint cache, a Minds workspace's URL prefix and the embedder's
//! thread cap would stop reaching it.
//!
//! A text edit: every comment and key the file holds survives. An entry
//! not written under a `[[programs]]` header is refused, naming the
//! program, so a person adds the line by hand.

use anyhow::{bail, Context as _, Result};

use crate::retired::is_datalib_step;

/// The `command` of each `datalib-step` entry that names no `pass_env`.
fn unpassed(text: &str) -> Result<Vec<String>> {
    let doc: toml::Value = toml::from_str(text).context("parse the config")?;
    let programs = doc
        .get("programs")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    Ok(programs
        .iter()
        .filter(|p| p.get("pass_env").is_none())
        .filter_map(|p| p.get("command").and_then(|c| c.as_str()))
        .filter(|c| is_datalib_step(c.trim()))
        .map(str::to_string)
        .collect())
}

pub fn is_retired(text: &str) -> Result<bool> {
    Ok(!unpassed(text)?.is_empty())
}

/// The line that names `datalib-step`'s variables in its entry.
pub(crate) fn pass_env_line() -> String {
    format!(
        "pass_env = [{}]",
        datalib_env::DATALIB_STEP_PASS_ENV
            .iter()
            .map(|n| quote(n))
            .collect::<Vec<_>>()
            .join(", ")
    )
}

pub fn rewrite(text: &str) -> Result<String> {
    let line = pass_env_line();
    let mut out = text.to_string();
    let mut by_hand = Vec::new();
    for command in unpassed(text)? {
        match entry_end(&out, &command) {
            Some(at) if out[..at].ends_with('\n') => out.insert_str(at, &format!("{line}\n")),
            Some(at) => out.insert_str(at, &format!("\n{line}\n")),
            None => by_hand.push(command),
        }
    }
    if !by_hand.is_empty() {
        bail!(
            "these `[[programs]]` entries for datalib-step name no `pass_env`, and are not \
             written under a `[[programs]]` header this rewrite can add to: {}; add `{line}` \
             to each",
            by_hand.join(", ")
        );
    }
    Ok(out)
}

fn quote(s: &str) -> String {
    toml::Value::String(s.to_string()).to_string()
}

/// Just past the last `key = value` line of the `[[programs]]` entry for
/// `command`, when the entry is written under its own header.
fn entry_end(text: &str, command: &str) -> Option<usize> {
    entry_lines(text, command).map(|lines| lines.end)
}

/// From the start of the first `key = value` line to just past the last
/// one, of the `[[programs]]` entry for `command`, when the entry is
/// written under its own header.
pub(crate) fn entry_lines(text: &str, command: &str) -> Option<std::ops::Range<usize>> {
    let mut start = None;
    let mut in_programs = false;
    let mut found = false;
    let mut end = None;
    let mut at = 0;
    for line in text.split_inclusive('\n') {
        at += line.len();
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            if found {
                break;
            }
            in_programs = trimmed == "[[programs]]";
            (start, end) = (None, None);
            continue;
        }
        if !in_programs || trimmed.starts_with('#') {
            continue;
        }
        let Some((key, value)) = trimmed.split_once('=') else {
            continue;
        };
        start.get_or_insert(at - line.len());
        end = Some(at);
        if key.trim() == "command" && string_value(value).as_deref() == Some(command) {
            found = true;
        }
    }
    found.then_some(start?..end?)
}

fn string_value(toml_value: &str) -> Option<String> {
    toml::from_str::<toml::Value>(&format!("v = {}", toml_value.trim()))
        .ok()?
        .get("v")?
        .as_str()
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pass_env_of(text: &str, command: &str) -> Option<Vec<String>> {
        let doc: toml::Value = toml::from_str(text).unwrap();
        doc.get("programs")?
            .as_array()?
            .iter()
            .find(|p| p.get("command").and_then(|c| c.as_str()) == Some(command))?
            .get("pass_env")?
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
    }

    #[test]
    fn a_datalib_step_entry_gets_its_list_and_keeps_its_comments() {
        let text = "# mine\n[[programs]]\n# the step program\ncommand = \"datalib-step\"\n\
                    verbs = [\"reset\"]\n\n[[programs]]\ncommand = \"./bin/mine\"\n";
        assert!(is_retired(text).unwrap());
        let out = rewrite(text).unwrap();
        assert!(out.contains("# the step program\n"), "{out}");
        assert_eq!(
            pass_env_of(&out, "datalib-step").unwrap(),
            datalib_env::DATALIB_STEP_PASS_ENV
                .iter()
                .map(|s| s.to_string())
                .collect::<Vec<_>>()
        );
        assert_eq!(
            pass_env_of(&out, "./bin/mine"),
            None,
            "another program is left alone"
        );
        assert!(!is_retired(&out).unwrap());
    }

    #[test]
    fn every_spelling_of_the_program_is_given_one() {
        let text = "[[programs]]\ncommand = \"datalib-step\"\n\n\
                    [[programs]]\ncommand = \"/opt/bin/datalib-step\"\nverbs = []\n";
        let out = rewrite(text).unwrap();
        assert!(pass_env_of(&out, "datalib-step").is_some());
        assert!(pass_env_of(&out, "/opt/bin/datalib-step").is_some());
    }

    #[test]
    fn the_line_goes_after_the_entry_s_last_key_even_with_no_final_newline() {
        let text = "[[programs]]\ncommand = \"datalib-step\"\nverbs = [\"reset\"]";
        let out = rewrite(text).unwrap();
        assert_eq!(
            out,
            format!(
                "[[programs]]\ncommand = \"datalib-step\"\nverbs = [\"reset\"]\n{}\n",
                pass_env_line()
            )
        );
    }

    #[test]
    fn a_list_a_person_wrote_is_kept_as_written() {
        let text = "[[programs]]\ncommand = \"datalib-step\"\npass_env = []\n";
        assert!(!is_retired(text).unwrap());
    }

    #[test]
    fn an_inline_entry_is_refused_by_name() {
        let text = "programs = [{ command = \"datalib-step\" }]\n";
        let err = rewrite(text).unwrap_err().to_string();
        assert!(err.contains("datalib-step"), "{err}");
    }
}
