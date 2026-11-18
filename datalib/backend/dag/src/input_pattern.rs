//! An input that names every step of one function, `*/render_markdown`,
//! instead of each by id. The loader expands it to the ids it matches, so
//! everything past the loader sees ids only; this file is the one place
//! that knows the spelling.

/// What an input that is a pattern stands for: `*/<function>`, every
/// step whose id is `<group>/<function>`.
pub const WILDCARD_GROUP: &str = "*";

/// The function a pattern input names, or `None` for an input that is a
/// step id.
pub fn pattern_function(input: &str) -> Option<&str> {
    let function = input.strip_prefix(WILDCARD_GROUP)?.strip_prefix('/')?;
    (!function.is_empty() && !function.contains('/') && !function.contains('*')).then_some(function)
}

/// Whether `input`, an id or a pattern, names the step `id`.
pub fn names(input: &str, id: &str) -> bool {
    match pattern_function(input) {
        Some(function) => id
            .split_once('/')
            .is_some_and(|(group, f)| !group.is_empty() && f == function),
        None => input == id,
    }
}

/// One pattern a step wrote, and the ids it stood for in this load.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Expansion {
    pub step: String,
    pub pattern: String,
    pub ids: Vec<String>,
}

/// `written` with each pattern replaced by the steps of `universe` it
/// names, in `universe`'s order, never `own` and never an id twice. An id
/// written out keeps its place; a pattern's ids go where the pattern was.
pub fn expand(own: &str, written: &[String], universe: &[&str]) -> (Vec<String>, Vec<Expansion>) {
    let mut out: Vec<String> = Vec::with_capacity(written.len());
    let mut expansions = Vec::new();
    for input in written {
        if pattern_function(input).is_none() {
            if !out.contains(input) {
                out.push(input.clone());
            }
            continue;
        }
        let ids: Vec<String> = universe
            .iter()
            .filter(|id| **id != own && names(input, id))
            .map(|id| id.to_string())
            .collect();
        for id in &ids {
            if !out.contains(id) {
                out.push(id.clone());
            }
        }
        expansions.push(Expansion {
            step: own.to_string(),
            pattern: input.clone(),
            ids,
        });
    }
    (out, expansions)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pattern_is_a_star_and_one_function() {
        assert_eq!(
            pattern_function("*/render_markdown"),
            Some("render_markdown")
        );
        for not in [
            "slack/render_markdown",
            "*",
            "*/",
            "*/a/b",
            "**/x",
            "*/x*",
            "a*/x",
        ] {
            assert_eq!(pattern_function(not), None, "{not}");
        }
    }

    #[test]
    fn a_pattern_names_every_group_with_that_function_and_nothing_deeper() {
        assert!(names("*/render_markdown", "slack/render_markdown"));
        assert!(!names("*/render_markdown", "slack/ingest"));
        assert!(!names("*/render_markdown", "render_markdown"));
        assert!(!names("*/render_markdown", "a/b/render_markdown"));
        assert!(names("slack/ingest", "slack/ingest"));
        assert!(!names("slack/ingest", "slack/ingest2"));
    }

    /// The pattern's ids come in the universe's order where the pattern
    /// stood; the step itself and a repeat are left out.
    #[test]
    fn expansion_keeps_order_and_drops_itself_and_repeats() {
        let universe = [
            "slack/ingest",
            "slack/render_markdown",
            "mail/render_markdown",
            "index/render_markdown",
        ];
        let written = vec![
            "slack/ingest".to_string(),
            "*/render_markdown".to_string(),
            "mail/render_markdown".to_string(),
        ];
        let (ids, expansions) = expand("index/render_markdown", &written, &universe);
        assert_eq!(
            ids,
            [
                "slack/ingest",
                "slack/render_markdown",
                "mail/render_markdown"
            ]
        );
        assert_eq!(
            expansions,
            [Expansion {
                step: "index/render_markdown".into(),
                pattern: "*/render_markdown".into(),
                ids: vec![
                    "slack/render_markdown".into(),
                    "mail/render_markdown".into()
                ],
            }]
        );
    }

    #[test]
    fn a_pattern_that_matches_nothing_is_no_input() {
        let (ids, expansions) = expand("index/grid", &["*/embed".to_string()], &["a/ingest"]);
        assert!(ids.is_empty());
        assert_eq!(expansions[0].ids, Vec::<String>::new());
    }
}
