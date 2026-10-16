//! What a qmd hit shows in the grid's Contents cell. qmd hands back the
//! lines of the rendered markdown around a match, under a `@@ -N,M @@`
//! header and, from the MCP daemon, with each line numbered. This keeps
//! the words a person would read: a front-matter field says nothing but
//! its title, and a body line loses its markup the way a row's preview
//! does (`datalib_schema::plain_text`). An empty answer means the
//! hit showed nothing readable, and the row keeps its own preview.
//!
//! The test samples keep the exact shapes qmd 2.8.3 returns, daemon and
//! CLI; their contents are made up.

pub fn display_snippet(raw: &str) -> String {
    let numbered = raw.lines().next().is_some_and(|first| {
        let rest = first.trim_start_matches(|c: char| c.is_ascii_digit());
        rest.len() < first.len() && rest.starts_with(": @@ ")
    });
    let lines: Vec<String> = raw
        .lines()
        .map(|line| {
            if numbered {
                without_line_number(line)
            } else {
                line
            }
        })
        .filter(|line| !line.starts_with("@@ "))
        .filter_map(readable)
        .collect();
    let mut out = datalib_schema::grid_rows::preview(&lines.join("\n"));
    if raw.trim_end().ends_with("...") && !out.is_empty() && !out.ends_with('…') {
        out.push('…');
    }
    out
}

fn without_line_number(line: &str) -> &str {
    let rest = line.trim_start_matches(|c: char| c.is_ascii_digit());
    let rest = rest.strip_prefix(':').unwrap_or(rest);
    rest.strip_prefix(' ').unwrap_or(rest)
}

/// A front-matter field says only its title; any other line is the
/// markdown `preview` reads, less qmd's own truncation mark.
fn readable(line: &str) -> Option<String> {
    let line = line.trim();
    if line == "---" {
        return None;
    }
    if let Some((key, value)) = front_matter_field(line) {
        return (key == "title").then(|| unquote(value));
    }
    Some(line.strip_suffix("...").unwrap_or(line).to_string())
}

/// `key: value` with a lowercase snake-case key: the shape every renderer's
/// front matter takes. A body line shaped like it is dropped too, which is
/// the price of reading a snippet with no idea where the front matter ends.
fn front_matter_field(line: &str) -> Option<(&str, &str)> {
    let (key, value) = line.split_once(": ")?;
    let snake = key.starts_with(|c: char| c.is_ascii_lowercase())
        && key
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
    snake.then_some((key, value))
}

/// Renderers JSON-quote a front-matter value (`yaml_scalar`); one that
/// does not read as JSON — cut short by qmd, say — loses only its quotes.
fn unquote(value: &str) -> String {
    let value = value.trim();
    serde_json::from_str::<String>(value).unwrap_or_else(|_| {
        value
            .strip_prefix('"')
            .and_then(|v| v.strip_suffix('"'))
            .unwrap_or(value)
            .to_string()
    })
}

#[cfg(test)]
mod tests {
    use super::display_snippet;

    /// The MCP daemon numbers every line, header included; a hit in the
    /// front matter says only its title.
    #[test]
    fn a_front_matter_hit_from_the_daemon_is_its_title() {
        let raw = "7: @@ -6,4 @@ (5 before, 19 after)\n8: calendar: \"picard@enterprise.test\"\n\
                   9: title: Submit the quarterly shuttle maintenance report\n\
                   10: external_id: \"0e1f2a3b-4c5d-6e7f-8091-a2b3c4d5e6f7#1701d@enterprise.test\"\n\
                   11: start: 2364-03-15T06:00:00-07:00";
        assert_eq!(
            display_snippet(raw),
            "Submit the quarterly shuttle maintenance report"
        );
    }

    /// qmd's CLI prints the same lines without numbers, and a quoted
    /// title loses its quotes but keeps its own colon.
    #[test]
    fn a_front_matter_hit_from_the_cli_is_its_title() {
        let raw = "@@ -5,4 @@ (4 before, 19 after)\nkind: Event\ncalendar: \"#BRIDGE\"\n\
                   title: \"Dinner: Ten Forward, senior staff\"\n\
                   external_id: \"0e1f2a3b-4c5d-6e7f-8091-a2b3c4d5e6f7#ncc1701d@enterprise.test\"";
        assert_eq!(display_snippet(raw), "Dinner: Ten Forward, senior staff");
    }

    /// A title's quotes and backslashes are JSON escapes, read back.
    #[test]
    fn a_json_quoted_title_reads_as_written() {
        let raw = "@@ -2,1 @@ (1 before, 9 after)\ntitle: \"The \\\"Q\\\" \\\\ continuum\"";
        assert_eq!(display_snippet(raw), "The \"Q\" \\ continuum");
    }

    /// A weak vector hit lands on the first lines of the file, which are
    /// ids; nothing is readable, so the row keeps its preview.
    #[test]
    fn a_hit_on_the_first_lines_shows_nothing_of_its_own() {
        let daemon = "1: @@ -1,3 @@ (0 before, 49 after)\n2: ---\n\
                      3: markdown_uuid: 00000000-0000-8000-8000-000000000001\n4: source_id: holodeck";
        let cli = "@@ -1,3 @@ (0 before, 56 after)\n---\n\
                   markdown_uuid: 00000000-0000-8000-8000-000000000002\nsource_id: sickbay";
        assert_eq!(display_snippet(daemon), "");
        assert_eq!(display_snippet(cli), "");
    }

    /// A body line carries escaped HTML and a truncation mark from qmd;
    /// `#4` in the middle of it is text, not a heading.
    #[test]
    fn a_body_hit_loses_its_markup_and_keeps_its_words() {
        let raw = "33: @@ -32,4 @@ (31 before, 8 after)\n34: \n\
                   35: Your inoculation is booked for Lieutenant Worf.&lt;br&gt;&lt;br&gt;\
                   Stardate 47988.1 at 0900&lt;br&gt;Location:&lt;br&gt;Sickbay \
                   #4 - Deck 12 (USS Enterprise)&lt;br&gt;Dr. Crusher&lt;br&gt;BRING&lt;br&gt;PADD&lt;br...";
        assert_eq!(
            display_snippet(raw),
            "Your inoculation is booked for Lieutenant Worf. Stardate 47988.1 at 0900 \
             Location: Sickbay #4 - Deck 12 (USS Enterprise) Dr. Crusher BRING PADD…"
        );
    }

    /// A link target and a heading mark are markup too.
    #[test]
    fn a_link_target_and_a_heading_mark_go() {
        let raw = "52: @@ -51,4 @@ (50 before, 4 after)\n53: <br>\n\
                   54: If you no longer wish to receive duty rosters, you can update your \
                   preferences\u{a0}here (https://roster.enterprise.test/prefs?id=1701&amp;unsubscribe=true).\n\
                   55: \n56: ## Links";
        assert_eq!(
            display_snippet(raw),
            "If you no longer wish to receive duty rosters, you can update your \
             preferences here. Links"
        );
    }

    #[test]
    fn it_is_no_longer_than_a_rows_preview() {
        let raw = format!("@@ -9,4 @@ (2 before, 5 after)\n{}", "word ".repeat(200));
        assert_eq!(display_snippet(&raw).chars().count(), 241);
    }
}
