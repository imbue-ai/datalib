//! How a grid names a grouping on the wire: the columns it groups by
//! (`by=kind,author`), and the group whose rows it wants
//! (`within=[["kind","Chat"],["author",null]]`). Both name the grid's
//! columns by id; one no table column holds (Score) cannot group.

use datalib_unified_index::group::Within;
use datalib_unified_index::view::{group_column, View};

pub fn parse_by<V: View>(spelled: &str) -> Result<Vec<V::Column>, String> {
    spelled.split(',').map(group_column::<V>).collect()
}

pub fn parse_within<V: View>(spelled: &str) -> Result<Vec<Within<V::Column>>, String> {
    let steps: Vec<(String, Option<String>)> = serde_json::from_str(spelled)
        .map_err(|e| format!("a group is [[column, value], …]: {e}"))?;
    steps
        .into_iter()
        .map(|(id, value)| {
            Ok(Within {
                column: group_column::<V>(&id)?,
                value,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use datalib_schema::grid_rows::GridRowColumn;
    use datalib_unified_index::grid_columns::GridColumn;

    #[test]
    fn columns_are_named_by_the_grids_ids() {
        assert_eq!(
            parse_by::<GridColumn>("source_ref,kind"),
            Ok(vec![GridRowColumn::SourceId, GridRowColumn::Kind])
        );
        assert_eq!(
            parse_by::<GridColumn>("created_at"),
            Ok(vec![GridRowColumn::CreatedAtUtc])
        );
        assert!(parse_by::<GridColumn>("score")
            .unwrap_err()
            .contains("\"score\""));
        assert!(parse_by::<GridColumn>("no_such_column").is_err());
    }

    #[test]
    fn a_group_is_its_columns_values_and_null_is_none() {
        assert_eq!(
            parse_within::<GridColumn>(r#"[["kind","Chat"],["author",null]]"#),
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
        assert!(parse_within::<GridColumn>(r#"[["score","1"]]"#).is_err());
        assert!(parse_within::<GridColumn>("kind:Chat").is_err());
    }
}
