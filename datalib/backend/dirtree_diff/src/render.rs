//! Projecting a [`DiffResult`] onto the viewer template.

use anyhow::{Context, Result};

use crate::model::DiffResult;

/// The viewer, baked into the binary. Shipping one self-contained
/// executable is the point — there is no template to install alongside
/// it and no path to resolve at run time.
const TEMPLATE: &str = include_str!("../viewer.html.tmpl");

const PLACEHOLDER: &str = "__DIRTREE_DIFF_DATA__";

/// Render the page. Returns the HTML rather than writing it, so the
/// caller decides where it goes and tests need no filesystem.
pub fn render(result: &DiffResult) -> Result<String> {
    let json = serde_json::to_string(result).context("serialize the result")?;
    // `</script>` inside the embedded JSON would end the script element
    // early; escaping the slash keeps it a valid JSON string.
    let safe = json.replace("</", "<\\/");
    Ok(TEMPLATE.replace(PLACEHOLDER, &safe))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::*;

    fn empty() -> DiffResult {
        DiffResult {
            left: side("/l"),
            right: side("/r"),
            summary: Summary::default(),
        }
    }

    fn side(db: &str) -> SideResult {
        SideResult {
            db: db.to_string(),
            reference: "HEAD".to_string(),
            commit: "0".repeat(40),
            dup_groups: vec![],
            dup_wasted: 0,
            nodes: vec![],
        }
    }

    #[test]
    fn the_placeholder_is_replaced() {
        let html = render(&empty()).unwrap();
        assert!(
            !html.contains(PLACEHOLDER),
            "the data placeholder survived into the output"
        );
        assert!(html.contains("<!doctype html>"));
    }

    #[test]
    fn the_template_has_exactly_one_placeholder() {
        assert_eq!(
            TEMPLATE.matches(PLACEHOLDER).count(),
            1,
            "the template must carry exactly one data slot"
        );
    }

    #[test]
    fn a_closing_script_tag_in_the_data_cannot_escape_the_script_element() {
        let mut result = empty();
        result.left.nodes.push(Node::plain(
            "</script><img src=x onerror=alert(1)>",
            "file",
            0,
            Status::Added,
        ));
        let html = render(&result).unwrap();
        assert!(
            !html.contains("</script><img"),
            "a path closed the script element: {html:.0}"
        );
        assert!(html.contains("<\\/script>"));
    }
}
