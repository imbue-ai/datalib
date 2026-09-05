//! What is wrong with a config file, and how much of the file it costs.
//!
//! A list rather than an `Err`, because first-problem-wins meant one stray
//! key in one step took down the grid, search, the document view and every
//! applet — they are all declared in the same file. Severity is blast
//! radius, not mood; the crate README has the table.
//!
//! Locations are byte spans, because the terminal wants `file:line:col` and
//! an excerpt while the UI editor wants a selection range. A span yields
//! both exactly; a line number yields neither.

use std::fmt;
use std::path::Path;

use serde::Serialize;

/// How much of the config one problem costs. See the module docs for
/// why these four and not two.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    /// Valid, and probably a mistake. Nothing is dropped.
    Warning,
    /// Well-formed, but something it depends on is missing or was
    /// itself dropped. This entry is dropped; the fix is elsewhere.
    Blocked,
    /// This entry is unusable and was dropped. Every other entry loads.
    Rejected,
    /// The file is not a config. Nothing loads.
    Fatal,
}

impl Severity {
    /// Whether a diagnostic at this level means an entry did not reach
    /// the graph. `Warning` is the only one that doesn't.
    pub fn drops_the_entry(self) -> bool {
        !matches!(self, Severity::Warning)
    }

    pub fn label(self) -> &'static str {
        match self {
            Severity::Fatal => "fatal",
            Severity::Rejected => "rejected",
            Severity::Blocked => "blocked",
            Severity::Warning => "warning",
        }
    }
}

/// Which kind of `[[…]]` array an entry came from. Steps and applets
/// are separate namespaces — an applet writes no artifacts, so an
/// applet id and a step id never collide and are never checked against
/// each other. (The scaffold relies on this: its `unified_index`
/// applet sits beside its `unified_index/grid` step.)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EntryKind {
    Step,
    Applet,
}

impl EntryKind {
    pub fn label(self) -> &'static str {
        match self {
            EntryKind::Step => "step",
            EntryKind::Applet => "applet",
        }
    }
}

/// The entry a diagnostic is about.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct EntryRef {
    pub kind: EntryKind,
    /// Position in the `[[steps]]` / `[[applets]]` array, 0-based —
    /// the only identity a malformed entry has.
    pub index: Option<usize>,
    pub id: Option<String>,
}

impl EntryRef {
    pub fn step(index: usize, id: Option<String>) -> Self {
        EntryRef {
            kind: EntryKind::Step,
            index: Some(index),
            id,
        }
    }
    pub fn step_id(id: impl Into<String>) -> Self {
        EntryRef {
            kind: EntryKind::Step,
            index: None,
            id: Some(id.into()),
        }
    }
    pub fn applet(index: usize, id: Option<String>) -> Self {
        EntryRef {
            kind: EntryKind::Applet,
            index: Some(index),
            id,
        }
    }
}

impl fmt::Display for EntryRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match (&self.id, self.index) {
            (Some(id), _) => write!(f, "{} {id:?}", self.kind.label()),
            (None, Some(i)) => write!(f, "{} #{i}", self.kind.label()),
            (None, None) => write!(f, "{}", self.kind.label()),
        }
    }
}

/// One problem with a config file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Diagnostic {
    pub severity: Severity,
    /// The entry this is about; `None` for whole-file problems.
    pub entry: Option<EntryRef>,
    /// What is wrong. One sentence, naming the thing. Never carries a
    /// location — that is what `span` is for.
    pub message: String,
    /// What to do about it. Kept out of `message` so the CLI can render
    /// it as its own `help:` line and the UI as secondary text, rather
    /// than either having to split a paragraph back apart.
    pub help: Option<String>,
    /// Byte range in the config text, when we know one. For an entry
    /// this is its `[[steps]]` / `[[applets]]` header, which is where a
    /// reader wants to be taken even when the offending key is a few
    /// lines below it; for a parse error it is the exact token.
    pub span: Option<(usize, usize)>,
    /// 1-based line of `span.start`. Derived at construction, while the
    /// text is in hand, so consumers that only want to print a location
    /// don't have to hold the file.
    pub line: Option<usize>,
    /// 1-based column of `span.start`, in bytes from the line start.
    pub column: Option<usize>,
}

impl Diagnostic {
    pub fn new(severity: Severity, message: impl Into<String>) -> Self {
        Diagnostic {
            severity,
            entry: None,
            message: message.into(),
            help: None,
            span: None,
            line: None,
            column: None,
        }
    }

    pub fn fatal(message: impl Into<String>) -> Self {
        Self::new(Severity::Fatal, message)
    }

    pub fn at_entry(mut self, entry: EntryRef) -> Self {
        self.entry = Some(entry);
        self
    }

    pub fn with_help(mut self, help: impl Into<String>) -> Self {
        self.help = Some(help.into());
        self
    }

    pub fn at_span(mut self, text: &str, span: std::ops::Range<usize>) -> Self {
        self.set_span(text, span);
        self
    }

    pub fn id(&self) -> Option<&str> {
        self.entry.as_ref().and_then(|e| e.id.as_deref())
    }

    pub fn set_span(&mut self, text: &str, span: std::ops::Range<usize>) {
        self.line = Some(line_of(text, span.start));
        self.column = Some(column_of(text, span.start));
        self.span = Some((span.start, span.end));
    }

    /// One line, no file and no excerpt: what a strict caller puts in
    /// its `Err`.
    pub fn describe(&self) -> String {
        let mut s = String::new();
        if let Some(line) = self.line {
            s.push_str(&format!("line {line}: "));
        }
        if let Some(e) = &self.entry {
            s.push_str(&format!("{e}: "));
        }
        s.push_str(self.message.trim_end());
        if let Some(h) = &self.help {
            s.push_str(&format!(" — {}", h.trim_end()));
        }
        s
    }

    pub fn render(&self, file: &Path, text: &str) -> String {
        let mut s = file.display().to_string();
        if let Some(line) = self.line {
            s.push_str(&format!(":{line}"));
            if let Some(col) = self.column {
                s.push_str(&format!(":{col}"));
            }
        }
        s.push_str(&format!(": {}: ", self.severity.label()));
        if let Some(e) = &self.entry {
            s.push_str(&format!("{e}: "));
        }
        s.push_str(self.message.trim_end());
        if let Some(excerpt) = self.excerpt(text) {
            s.push('\n');
            s.push_str(&excerpt);
        }
        if let Some(h) = &self.help {
            s.push_str(&format!("\n        help: {}", h.trim_end()));
        }
        s
    }

    fn excerpt(&self, text: &str) -> Option<String> {
        let (start, end) = self.span?;
        let line_no = self.line?;
        let col = self.column?;
        let line_start = text[..start.min(text.len())]
            .rfind('\n')
            .map(|i| i + 1)
            .unwrap_or(0);
        let line_end = text[line_start..]
            .find('\n')
            .map(|i| line_start + i)
            .unwrap_or(text.len());
        let src = &text[line_start..line_end];
        let gutter = line_no.to_string();
        let pad = " ".repeat(gutter.len());
        // The span can run past this line (an entry header plus its
        // body); a caret longer than the line is noise, so clamp it.
        let width = end
            .saturating_sub(start)
            .clamp(1, src.len().saturating_sub(col - 1).max(1));
        Some(format!(
            "  {gutter} | {src}\n  {pad} | {}{}",
            " ".repeat(col - 1),
            "^".repeat(width)
        ))
    }
}

pub fn line_of(text: &str, offset: usize) -> usize {
    let end = offset.min(text.len());
    text[..end].bytes().filter(|&b| b == b'\n').count() + 1
}

pub fn column_of(text: &str, offset: usize) -> usize {
    let end = offset.min(text.len());
    match text[..end].rfind('\n') {
        Some(nl) => end - nl,
        None => end + 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn severity_orders_by_blast_radius() {
        assert!(Severity::Fatal > Severity::Rejected);
        assert!(Severity::Rejected > Severity::Blocked);
        assert!(Severity::Blocked > Severity::Warning);
    }

    /// Only `Warning` leaves the entry in the graph. The other three
    /// differ in what the user is told, not in what runs.
    #[test]
    fn every_severity_but_warning_drops_the_entry() {
        assert!(Severity::Fatal.drops_the_entry());
        assert!(Severity::Rejected.drops_the_entry());
        assert!(Severity::Blocked.drops_the_entry());
        assert!(!Severity::Warning.drops_the_entry());
    }

    #[test]
    fn renders_file_line_col_severity_entry_message() {
        let text = "[[steps]]\nid = \"slack/raw\"\ntitle = \"x\"\n";
        let span = text.find("title").unwrap()..text.find("title").unwrap() + 5;
        let d = Diagnostic::new(Severity::Rejected, "unknown field `title`")
            .at_entry(EntryRef::step(1, Some("slack/raw".into())))
            .at_span(text, span)
            .with_help("remove it");
        assert_eq!(
            d.render(Path::new("config.toml"), text),
            "config.toml:3:1: rejected: step \"slack/raw\": unknown field `title`\n  \
             3 | title = \"x\"\n    | ^^^^^\n        help: remove it"
        );
    }

    /// An entry whose own `id` is the broken thing still has to be
    /// nameable, and its array position is the only name it has.
    #[test]
    fn an_entry_with_no_id_is_named_by_its_index() {
        let d = Diagnostic::new(Severity::Rejected, "missing field `id`")
            .at_entry(EntryRef::step(3, None));
        assert!(
            d.render(Path::new("c.toml"), "").contains("step #3"),
            "{d:?}"
        );
    }

    /// A diagnostic with no span renders as one line — no excerpt, no
    /// invented location.
    #[test]
    fn an_unlocated_diagnostic_renders_without_an_excerpt() {
        let d = Diagnostic::new(Severity::Warning, "nothing reads this");
        assert_eq!(
            d.render(Path::new("c.toml"), "irrelevant"),
            "c.toml: warning: nothing reads this"
        );
    }

    #[test]
    fn line_and_column_count_from_one() {
        let text = "aa\nbbbb\ncc\n";
        assert_eq!(line_of(text, 0), 1);
        assert_eq!(column_of(text, 0), 1);
        // First byte of "bbbb".
        assert_eq!(line_of(text, 3), 2);
        assert_eq!(column_of(text, 3), 1);
        // Third byte of "bbbb".
        assert_eq!(line_of(text, 5), 2);
        assert_eq!(column_of(text, 5), 3);
    }

    /// A bad offset must not take a load down with it.
    #[test]
    fn an_offset_past_the_end_clamps() {
        let text = "aa\nbb\n";
        assert_eq!(line_of(text, 9_999), 3);
        assert_eq!(column_of(text, 9_999), 1);
    }

    /// A span covering a whole entry (header + body) must not draw a
    /// caret running off the end of the header line.
    #[test]
    fn a_caret_never_outruns_its_line() {
        let text = "[[steps]]\nid = \"a\"\ncommand = \"x\"\n";
        let d = Diagnostic::new(Severity::Blocked, "blocked").at_span(text, 0..text.len());
        let out = d.render(Path::new("c.toml"), text);
        for line in out.lines() {
            assert!(line.len() < 40, "runaway caret: {out}");
        }
    }

    /// Graph diagnostics arrive with no location; the loader lends them
    /// one rather than threading the file text through graph assembly.
    #[test]
    fn set_span_locates_a_bare_diagnostic() {
        let text = "[[steps]]\nid = \"a\"\n";
        let mut bare = Diagnostic::new(Severity::Blocked, "y");
        assert_eq!(bare.line, None);
        bare.set_span(text, 10..12);
        assert_eq!(bare.line, Some(2));
        assert_eq!(bare.column, Some(1));
        assert_eq!(bare.span, Some((10, 12)));
    }
}
