//! Slack's standard emoji by the names Slack gives them, from the table
//! in `third-party/emoji-data` (its README says where it comes from). A
//! workspace's custom emoji are not in it and stay as `:name:`.

use std::collections::HashMap;
use std::sync::LazyLock;

use regex::{Captures, Regex};

static TABLE: LazyLock<HashMap<&'static str, String>> = LazyLock::new(|| {
    include_str!(env!("SLACK_SHORTCODES_TSV"))
        .lines()
        .filter_map(|line| {
            let (name, codepoints) = line.split_once('\t')?;
            Some((name, decode(codepoints)?))
        })
        .collect()
});

fn decode(codepoints: &str) -> Option<String> {
    codepoints
        .split('-')
        .map(|hex| char::from_u32(u32::from_str_radix(hex, 16).ok()?))
        .collect()
}

/// The emoji a shortcode names, without its colons: `thinking_face`, or
/// a skin tone as Slack appends it to a reaction's name,
/// `+1::skin-tone-2`. An emoji with no such tone shows untoned, followed
/// by the tone's swatch, which is what those codepoints are.
pub fn lookup(name: &str) -> Option<String> {
    if let Some(e) = TABLE.get(name) {
        return Some(e.clone());
    }
    let (base, tone) = name.split_once("::")?;
    Some(format!("{}{}", TABLE.get(base)?, TABLE.get(tone)?))
}

// A tone follows its emoji as a second shortcode, `:+1::skin-tone-2:`.
static SHORTCODE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r":([a-z0-9_+\-]+(?:::skin-tone-[2-6](?:-[2-6])?)?):").unwrap());

/// Every `:shortcode:` in `text` Slack would draw as a standard emoji,
/// as that emoji; anything else as written.
pub fn emojize(text: &str) -> String {
    SHORTCODE
        .replace_all(text, |caps: &Captures<'_>| {
            lookup(&caps[1]).unwrap_or_else(|| caps[0].to_string())
        })
        .into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The regression: these are Slack's names, and GitHub's table
    /// (the `emojis` crate we used) knows none of them.
    #[test]
    fn slack_names_that_github_does_not_use() {
        assert_eq!(emojize(":thinking_face:"), "🤔");
        assert_eq!(emojize(":rolling_on_the_floor_laughing:"), "🤣");
        assert_eq!(emojize(":star-struck:"), "🤩");
        assert_eq!(emojize(":flag-ca:"), "🇨🇦");
        assert_eq!(emojize(":woman-facepalming:"), "🤦\u{200d}♀\u{fe0f}");
        assert_eq!(emojize(":+1: :thumbsup: :tada:"), "👍 👍 🎉");
    }

    /// The regression: a toned emoji showed as `👍:skin-tone-2:`.
    #[test]
    fn a_skin_tone_joins_its_emoji() {
        assert_eq!(emojize(":+1::skin-tone-2:"), "👍🏻");
        assert_eq!(lookup("+1::skin-tone-6").as_deref(), Some("👍🏿"));
        assert_eq!(
            emojize(":woman-facepalming::skin-tone-4:"),
            "🤦🏽\u{200d}♀\u{fe0f}"
        );
        assert_eq!(
            lookup("handshake::skin-tone-2-3").as_deref(),
            Some("🫱🏻\u{200d}🫲🏼")
        );
        // An emoji that takes no tone keeps the swatch Slack drew.
        assert_eq!(emojize(":tada::skin-tone-3:"), "🎉🏼");
        assert_eq!(emojize(":skin-tone-3:"), "🏼");
    }

    #[test]
    fn a_name_slack_does_not_know_is_left_as_written() {
        for text in [
            ":notarealemoji:",
            ":my-team-parrot:",
            ":my-team-parrot::skin-tone-2:",
            ":Thinking_Face:",
            "at 10:30:00",
        ] {
            assert_eq!(emojize(text), text);
        }
        assert_eq!(lookup("my-team-parrot::skin-tone-2"), None);
    }

    #[test]
    fn the_table_is_whole() {
        assert!(TABLE.len() > 3900, "{} shortcodes", TABLE.len());
        let lines = include_str!(env!("SLACK_SHORTCODES_TSV")).lines().count();
        assert_eq!(TABLE.len(), lines, "a line the table could not read");
    }
}
