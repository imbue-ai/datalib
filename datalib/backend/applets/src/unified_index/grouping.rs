//! How the grid names a grouping on the wire: the columns it groups by
//! (`by=kind,author`), and the group whose rows it wants
//! (`within=[["kind","Chat"],["author",null]]`). Both name grid columns
//! by id; one no `grid_rows` column holds (Score) cannot group.

use datalib_unified_index::group::Within;
use datalib_unified_index::sort::GridColumn;

fn column(id: &str) -> Result<&'static str, String> {
    GridColumn::parse(id)
        .and_then(GridColumn::sql)
        .ok_or_else(|| format!("rows cannot be grouped by {id:?}"))
}

pub fn parse_by(spelled: &str) -> Result<Vec<&'static str>, String> {
    spelled.split(',').map(column).collect()
}

pub fn parse_within(spelled: &str) -> Result<Vec<Within>, String> {
    let steps: Vec<(String, Option<String>)> = serde_json::from_str(spelled)
        .map_err(|e| format!("a group is [[column, value], …]: {e}"))?;
    steps
        .into_iter()
        .map(|(id, value)| {
            Ok(Within {
                column: column(&id)?,
                value,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn columns_are_named_by_the_grids_ids() {
        assert_eq!(parse_by("source_ref,kind"), Ok(vec!["source_id", "kind"]));
        assert!(parse_by("score").unwrap_err().contains("\"score\""));
        assert!(parse_by("no_such_column").is_err());
    }

    #[test]
    fn a_group_is_its_columns_values_and_null_is_none() {
        assert_eq!(
            parse_within(r#"[["kind","Chat"],["author",null]]"#),
            Ok(vec![
                Within {
                    column: "kind",
                    value: Some("Chat".into())
                },
                Within {
                    column: "author",
                    value: None
                },
            ])
        );
        assert!(parse_within(r#"[["score","1"]]"#).is_err());
        assert!(parse_within("kind:Chat").is_err());
    }
}
