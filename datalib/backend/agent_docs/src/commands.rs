//! The command lines a doc's shell blocks run one of datalib's programs
//! with, so each program's tests can put them through its own parser.

use crate::markdown::fenced_blocks;

/// Every `datalib-*` program a release ships (`:dist` in
/// `datalib/backend/BUILD.bazel`): what a doc's command may run.
pub const PROGRAMS: &[&str] = &[
    "datalib-dag",
    "datalib-step",
    "datalib-http",
    "datalib-applet",
    "datalib-migrate-config",
    "datalib-doltlite",
    "datalib-file-inventory",
    "datalib-dirtree-diff",
];

/// The programs whose command lines in the docs a test parses, each in
/// its own crate, with the parser the binary runs.
pub const CHECKED_PROGRAMS: &[&str] = &[
    "datalib-dag",
    "datalib-step",
    "datalib-applet",
    "datalib-migrate-config",
    "datalib-http",
];

/// The programs the docs may run that no test parses, and why.
pub const UNCHECKED_PROGRAMS: &[(&str, &str)] = &[(
    "datalib-doltlite",
    "the sqlite3 shell, whose argv is sqlite3's and has no parser to call",
)];

const SHELL_INFOS: &[&str] = &["", "sh", "bash", "shell", "console", "zsh"];

pub struct Command {
    /// The line in the doc the command starts on, counting from 1.
    pub line: usize,
    /// The program's argv, its name first; `None` when the line is not one
    /// a shell could split (an unclosed quote).
    pub argv: Option<Vec<String>>,
    pub text: String,
}

/// Every command in `markdown`'s shell blocks that runs one of datalib's
/// programs, wherever on the line it starts: after `VAR=value`, after
/// `docker run … <image>`, inside `$(…)`, after `|` or `&&`.
pub fn commands(markdown: &str) -> Vec<Command> {
    let programs = PROGRAMS;
    let mut out = Vec::new();
    for block in fenced_blocks(markdown) {
        let lang = block.info.split_whitespace().next().unwrap_or("");
        if !SHELL_INFOS.contains(&lang) {
            continue;
        }
        for (offset, line) in logical_lines(&block.body) {
            let line_no = block.line + 1 + offset;
            let Some(words) = shlex::split(&line) else {
                if programs.iter().any(|p| line.contains(p)) {
                    out.push(Command {
                        line: line_no,
                        argv: None,
                        text: line,
                    });
                }
                continue;
            };
            for segment in segments(words) {
                let Some(start) = segment
                    .iter()
                    .position(|w| programs.contains(&w.rsplit('/').next().unwrap_or(w)))
                else {
                    continue;
                };
                let mut argv: Vec<String> = segment[start..].to_vec();
                argv[0] = argv[0].rsplit('/').next().unwrap_or("").to_string();
                out.push(Command {
                    line: line_no,
                    text: line.clone(),
                    argv: Some(argv),
                });
            }
        }
    }
    out
}

// A shell block's lines joined where the shell would join them: after a
// trailing `\`, and inside an open quote. A heredoc's body is data, not
// commands, and is skipped. Each comes with its first line's offset.
fn logical_lines(body: &str) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    let mut pending: Option<(usize, String)> = None;
    let mut heredoc_end: Option<String> = None;
    for (i, raw) in body.lines().enumerate() {
        if let Some(end) = &heredoc_end {
            if raw.trim() == end {
                heredoc_end = None;
            }
            continue;
        }
        let (first, text) = match pending.take() {
            Some((first, mut text)) => {
                text.push('\n');
                text.push_str(raw);
                (first, text)
            }
            None => (i, raw.trim_start().trim_start_matches("$ ").to_string()),
        };
        if let Some(stripped) = text.strip_suffix('\\') {
            pending = Some((first, stripped.to_string()));
            continue;
        }
        if shlex::split(&text).is_none() && i + 1 < body.lines().count() {
            pending = Some((first, text));
            continue;
        }
        heredoc_end = heredoc_terminator(&text);
        out.push((first, text));
    }
    if let Some(p) = pending {
        out.push(p);
    }
    out
}

fn heredoc_terminator(line: &str) -> Option<String> {
    let after = &line[line.find("<<")? + 2..];
    let word = after.trim_start_matches('-').trim_start();
    let word = word.split_whitespace().next()?;
    Some(word.trim_matches(['\'', '"']).to_string())
}

// The words split where one command ends and the next begins. A
// redirection and its file are not arguments, and are dropped.
fn segments(words: Vec<String>) -> Vec<Vec<String>> {
    const OPERATORS: &[&str] = &["|", "||", "&&", ";", "&", "then", "do"];
    const REDIRECTS: &[&str] = &[">", ">>", "<", "2>", "2>>", "&>"];
    let mut out: Vec<Vec<String>> = vec![Vec::new()];
    let mut skip_next = false;
    let mut in_substitution = false;
    for mut word in words {
        if skip_next {
            skip_next = false;
            continue;
        }
        if OPERATORS.contains(&word.as_str()) {
            out.push(Vec::new());
            continue;
        }
        if REDIRECTS.contains(&word.as_str()) {
            skip_next = true;
            continue;
        }
        if word.starts_with('>') || word.starts_with("2>") || word.starts_with("&>") {
            continue;
        }
        if let Some((_, inner)) = word.split_once("$(") {
            out.push(Vec::new());
            in_substitution = true;
            word = inner.to_string();
        }
        let ends = if in_substitution {
            word.strip_suffix(')')
        } else {
            word.strip_suffix(';')
        };
        if let Some(last) = ends {
            if !last.is_empty() {
                out.last_mut().expect("never empty").push(last.to_string());
            }
            out.push(Vec::new());
            in_substitution = false;
            continue;
        }
        if !word.is_empty() {
            out.last_mut().expect("never empty").push(word);
        }
    }
    out
}
