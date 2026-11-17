//! What a search hit shows in the grid's Contents cell: the lines of the
//! rendered markdown it landed on, under a `@@ -N,M @@` header
//! (`GridIndex::land`). This keeps the words a person would read: a
//! front-matter field says nothing but its title, and a body line loses
//! its markup the way a row's preview does (`datalib_schema::plain_text`).
//! An empty answer means the hit showed nothing readable, and the row
//! keeps its own preview.

pub fn display_snippet(raw: &str) -> String {
    let kept: Vec<String> = raw
        .lines()
        .filter(|line| !line.starts_with("@@ "))
        .filter_map(readable)
        .collect();
    datalib_schema::grid_rows::preview(&kept.join("\n"))
}

/// A front-matter field says only its title; any other line is the
/// markdown `preview` reads.
fn readable(line: &str) -> Option<String> {
    let line = line.trim();
    if line == "---" {
        return None;
    }
    if let Some((key, value)) = front_matter_field(line) {
        return (key == "title").then(|| unquote(value));
    }
    Some(line.to_string())
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
/// does not read as JSON loses only its quotes.
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

    /// A hit in the front matter says only its title, which loses its
    /// quotes but keeps its own colon.
    #[test]
    fn a_front_matter_hit_is_its_title() {
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

    /// Lines that are only ids leave nothing readable, so the row keeps
    /// its preview.
    #[test]
    fn a_hit_on_ids_alone_shows_nothing_of_its_own() {
        let raw = "@@ -1,3 @@ (0 before, 56 after)\n---\n\
                   markdown_uuid: 00000000-0000-8000-8000-000000000002\nsource_id: sickbay";
        assert_eq!(display_snippet(raw), "");
    }

    /// A body line carrying escaped HTML loses it; `#4` in the middle of
    /// it is text, not a heading, and a `...` the writer typed stays.
    #[test]
    fn a_body_hit_loses_its_markup_and_keeps_its_words() {
        let raw = "@@ -32,4 @@ (0 before, 0 after)\n\n\
                   Your inoculation is booked for Lieutenant Worf.&lt;br&gt;&lt;br&gt;\
                   Stardate 47988.1 at 0900&lt;br&gt;Location:&lt;br&gt;Sickbay \
                   #4 - Deck 12 (USS Enterprise)&lt;br&gt;Dr. Crusher&lt;br&gt;BRING&lt;br&gt;PADD...";
        assert_eq!(
            display_snippet(raw),
            "Your inoculation is booked for Lieutenant Worf. Stardate 47988.1 at 0900 \
             Location: Sickbay #4 - Deck 12 (USS Enterprise) Dr. Crusher BRING PADD..."
        );
    }

    /// A link target and a heading mark are markup too.
    #[test]
    fn a_link_target_and_a_heading_mark_go() {
        let raw = "@@ -51,4 @@ (0 before, 0 after)\n<br>\n\
                   If you no longer wish to receive duty rosters, you can update your \
                   preferences\u{a0}here (https://roster.enterprise.test/prefs?id=1701&amp;unsubscribe=true).\n\
                   \n## Links";
        assert_eq!(
            display_snippet(raw),
            "If you no longer wish to receive duty rosters, you can update your \
             preferences here. Links"
        );
    }

    /// A contact's fields are a `<dl>`: a label and its value were one
    /// word ("NotePlays the trombone").
    #[test]
    fn a_definition_list_reads_as_separate_words() {
        let raw = "@@ -9,3 @@ (0 before, 0 after)\n<dl>\n\
                   <dt>Note</dt><dd>Plays the trombone. Owns a beard.</dd>\n</dl>\n\n";
        assert_eq!(
            display_snippet(raw),
            "Note Plays the trombone. Owns a beard."
        );
    }

    #[test]
    fn it_is_no_longer_than_a_rows_preview() {
        let raw = format!("@@ -9,4 @@ (2 before, 5 after)\n{}", "word ".repeat(200));
        assert_eq!(display_snippet(&raw).chars().count(), 241);
    }
}
