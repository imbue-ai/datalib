//! Cross-language fixture test: parses the same JSON fixtures as the
//! TypeScript suite at frankweiler/ui/tests/deeplink-fixtures.json.

use frankweiler_core::deeplink::{parse, to_deeplink, to_hash, Route};
use std::collections::BTreeMap;

const FIXTURES: &str = include_str!("../../../ui/tests/deeplink-fixtures.json");

#[derive(serde::Deserialize)]
struct Fixture {
    name: String,
    deeplink: String,
    hash: String,
    route: RouteJson,
}

#[derive(serde::Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
enum RouteJson {
    Search { params: BTreeMap<String, String> },
    Chat {
        #[serde(rename = "conversationUuid")]
        conversation_uuid: String,
        params: BTreeMap<String, String>,
    },
    Prefs,
}

impl From<RouteJson> for Route {
    fn from(r: RouteJson) -> Self {
        match r {
            RouteJson::Search { params } => Route::Search { params },
            RouteJson::Chat { conversation_uuid, params } => {
                Route::Chat { conversation_uuid, params }
            }
            RouteJson::Prefs => Route::Prefs,
        }
    }
}

#[test]
fn parses_all_fixtures() {
    let fixtures: Vec<Fixture> = serde_json::from_str(FIXTURES).unwrap();
    assert!(!fixtures.is_empty());
    for f in fixtures {
        let want: Route = f.route.into();
        assert_eq!(parse(&f.deeplink).unwrap(), want, "deeplink: {}", f.name);
        assert_eq!(parse(&format!("#{}", f.hash)).unwrap(), want, "hash: {}", f.name);
        assert_eq!(to_hash(&want), f.hash, "to_hash: {}", f.name);
        assert_eq!(to_deeplink(&want), f.deeplink, "to_deeplink: {}", f.name);
    }
}
