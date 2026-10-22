//! An OAuth client a person pasted into the wizard, turned into the JSON
//! `latchkey auth prepare` takes. Google's services sign in with a client
//! of the person's own (`docs/dev/latchkey.md` §"An OAuth client of your
//! own"), and the file Google Cloud downloads for one is not the shape
//! latchkey reads.

use serde_json::{json, Map, Value};

/// The `{"clientId", "clientSecret"}` latchkey takes, from either the
/// `client_secret.json` Google Cloud downloads or that shape itself. The
/// error never repeats what was pasted: it holds a secret.
pub fn latchkey_client_json(pasted: &str) -> Result<String, String> {
    let value: Value = serde_json::from_str(pasted.trim()).map_err(|_| {
        "that is not JSON: paste the whole client_secret.json Google Cloud downloads".to_string()
    })?;
    let Some(object) = value.as_object() else {
        return Err(NOT_A_CLIENT.to_string());
    };
    if object.contains_key("clientId") {
        // latchkey's own shape, passed on whole: its schema refuses a
        // misspelt key, and says which.
        return Ok(value.to_string());
    }
    if object.contains_key("web") {
        return Err(
            "that is a Web application client, and latchkey signs in through a \
                    Desktop app client: create one of type Desktop app and paste its file"
                .to_string(),
        );
    }
    let Some(installed) = object.get("installed").and_then(Value::as_object) else {
        return Err(NOT_A_CLIENT.to_string());
    };
    let id = field(installed, "client_id")?;
    let secret = field(installed, "client_secret")?;
    Ok(json!({ "clientId": id, "clientSecret": secret }).to_string())
}

const NOT_A_CLIENT: &str = "that is not an OAuth client: paste the client_secret.json Google \
                            Cloud downloads for a Desktop app client";

fn field<'a>(installed: &'a Map<String, Value>, key: &str) -> Result<&'a str, String> {
    installed
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| format!("the client file has no {key}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shape Google Cloud's "Download JSON" gives a Desktop app
    /// client, with made-up values.
    const DOWNLOADED: &str = r#"{"installed":{"client_id":"1234-abc.apps.googleusercontent.com","project_id":"latchkey-enterprise","auth_uri":"https://accounts.google.com/o/oauth2/auth","token_uri":"https://oauth2.googleapis.com/token","client_secret":"GOCSPX-picard","redirect_uris":["http://localhost"]}}"#;

    fn parsed(s: &str) -> Value {
        serde_json::from_str(&latchkey_client_json(s).unwrap()).unwrap()
    }

    #[test]
    fn a_downloaded_desktop_client_becomes_latchkeys_shape() {
        assert_eq!(
            parsed(DOWNLOADED),
            json!({ "clientId": "1234-abc.apps.googleusercontent.com", "clientSecret": "GOCSPX-picard" })
        );
    }

    #[test]
    fn latchkeys_own_shape_passes_through_whole() {
        let own = r#" {"clientId":"a","clientSecret":"b","redirectUri":"https://example.com/cb"} "#;
        assert_eq!(
            parsed(own),
            serde_json::from_str::<Value>(own.trim()).unwrap()
        );
    }

    #[test]
    fn a_web_client_is_refused_with_what_to_make_instead() {
        let web = DOWNLOADED.replace("installed", "web");
        let e = latchkey_client_json(&web).unwrap_err();
        assert!(e.contains("Desktop app"), "{e}");
    }

    /// The secret must not come back in an error that ends up on screen
    /// and in the log.
    #[test]
    fn no_error_repeats_the_secret() {
        let cases = [
            "GOCSPX-picard".to_string(),
            r#"["GOCSPX-picard"]"#.to_string(),
            r#"{"installed":{"client_secret":"GOCSPX-picard"}}"#.to_string(),
            r#"{"other":{"client_secret":"GOCSPX-picard"}}"#.to_string(),
            DOWNLOADED.replace("installed", "web"),
        ];
        for case in cases {
            let e = latchkey_client_json(&case).unwrap_err();
            assert!(!e.contains("picard"), "{e}");
        }
    }

    #[test]
    fn a_client_without_an_id_says_which_field_is_missing() {
        let e = latchkey_client_json(r#"{"installed":{"client_secret":"s"}}"#).unwrap_err();
        assert!(e.contains("client_id"), "{e}");
    }
}
