//! How the grid names a grouping on the wire: the columns it groups by
//! (`by=kind,author`), and the group whose rows it wants
//! (`within=[["kind","Chat"],["author",null]]`). Both name grid columns
//! by id; one no `grid_rows` column holds (Score) cannot group.

use datalib_query::table::SearchTable;
use datalib_schema::grid_rows::{GridRow, GridRowColumn};
use datalib_unified_index::grid_columns::GridColumn;
use datalib_unified_index::group::Within;
use datalib_unified_index::sort::SortBy;

fn column(id: &str) -> Result<GridRowColumn, String> {
    match GridColumn::parse(id).map(GridColumn::sorts) {
        Some(SortBy::Column(c)) => Ok(GridRow::sorts_by(c)),
        _ => Err(format!("rows cannot be grouped by {id:?}")),
    }
}

pub fn parse_by(spelled: &str) -> Result<Vec<GridRowColumn>, String> {
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
        assert_eq!(
            parse_by("source_ref,kind"),
            Ok(vec![GridRowColumn::SourceId, GridRowColumn::Kind])
        );
        assert_eq!(
            parse_by("created_at"),
            Ok(vec![GridRowColumn::CreatedAtUtc])
        );
        assert!(parse_by("score").unwrap_err().contains("\"score\""));
        assert!(parse_by("no_such_column").is_err());
    }

    #[test]
    fn a_group_is_its_columns_values_and_null_is_none() {
        assert_eq!(
            parse_within(r#"[["kind","Chat"],["author",null]]"#),
            Ok(vec![
                Within {
                    column: GridRowColumn::Kind,
                    value: Some("Chat".into())
                },
                Within {
                    column: GridRowColumn::Author,
                    value: None
                },
            ])
        );
        assert!(parse_within(r#"[["score","1"]]"#).is_err());
        assert!(parse_within("kind:Chat").is_err());
    }
}
