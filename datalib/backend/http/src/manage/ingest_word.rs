//! What an ingest step is called under its group: "Download" when the
//! method its params hold reaches a service, "Import" when it reads files
//! on this machine. The methods each source type declares are the
//! generated `datalib/ui/src/config/ingestMethods.json`
//! (`//datalib/backend/datalib_step:ingest_methods.update`, checked there
//! against the provider crates), compiled in here so the rows, the chips
//! and the group's status all name the step the same way.

use std::collections::HashMap;
use std::sync::OnceLock;

use datalib_source_common::{held, reach_of, IngestMethod, Reach};
use serde::Deserialize;

#[derive(Deserialize)]
struct Declared {
    path: String,
    reach: Reach,
}

fn declared() -> &'static HashMap<String, Vec<IngestMethod>> {
    static METHODS: OnceLock<HashMap<String, Vec<IngestMethod>>> = OnceLock::new();
    METHODS.get_or_init(|| {
        let by_type: HashMap<String, Vec<Declared>> =
            serde_json::from_str(include_str!(env!("INGEST_METHODS_JSON")))
                .expect("the generated ingestMethods.json is a map of type to methods");
        by_type
            .into_iter()
            .map(|(t, methods)| {
                let methods = methods
                    .into_iter()
                    .map(|m| IngestMethod {
                        // Read once for the process's life, and
                        // `IngestMethod` names its path for `'static`.
                        path: Box::leak(m.path.into_boxed_str()),
                        reach: m.reach,
                    })
                    .collect();
                (t, methods)
            })
            .collect()
    })
}

pub fn ingest_word(r#type: Option<&str>, params: &serde_json::Value) -> &'static str {
    let methods = r#type
        .and_then(|t| declared().get(t))
        .map_or(&[][..], Vec::as_slice);
    match reach_of(&held(params, methods)) {
        Some(Reach::Origin) => "Download",
        Some(Reach::Local) => "Import",
        None => "Ingest",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_step_is_named_by_what_its_held_method_reaches() {
        assert_eq!(ingest_word(Some("claude"), &json!({"api": {}})), "Download");
        assert_eq!(
            ingest_word(Some("pdf"), &json!({"fswalk": {"path": "~/Documents"}})),
            "Import"
        );
    }

    #[test]
    fn a_step_holding_no_declared_method_keeps_the_plain_word() {
        assert_eq!(ingest_word(Some("pdf"), &json!({})), "Ingest");
        assert_eq!(
            ingest_word(Some("no_such_type"), &json!({"api": {}})),
            "Ingest"
        );
        assert_eq!(ingest_word(None, &json!({"api": {}})), "Ingest");
    }
}
