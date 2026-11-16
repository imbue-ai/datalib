//! What the signed-in home page carries that the download needs: the
//! token a list request sends, and the account's children. Both sit in
//! the page's inline script, as `__srf_token__ = '…'` and a `"myKids":
//! [...]` member of a JSON object.

use serde_json::Value;

#[derive(Debug, Clone, PartialEq)]
pub struct Home {
    pub token: String,
    pub kids: Vec<Value>,
}

/// `None` when the page carries no token or no children, which is the
/// login page a signed-out session is shown.
pub fn parse(html: &str) -> Option<Home> {
    Some(Home {
        token: token(html)?,
        kids: kids(html)?,
    })
}

fn token(html: &str) -> Option<String> {
    const NAME: &str = "__srf_token__";
    html.match_indices(NAME).find_map(|(at, _)| {
        let rest = html[at + NAME.len()..].trim_start();
        let rest = rest.strip_prefix('=')?.trim_start();
        let quote = rest.chars().next().filter(|c| *c == '\'' || *c == '"')?;
        let rest = &rest[1..];
        let end = rest.find(quote)?;
        Some(rest[..end].to_string()).filter(|t| !t.is_empty())
    })
}

fn kids(html: &str) -> Option<Vec<Value>> {
    const KEY: &str = "\"myKids\":";
    let at = html.find(KEY)? + KEY.len();
    let mut stream = serde_json::Deserializer::from_str(&html[at..]).into_iter::<Value>();
    match stream.next()?.ok()? {
        Value::Array(kids) => Some(kids),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PAGE: &str = r#"<html><script>
        var x = localStorage.getItem('__srf_token__');
        __srf_token__ = 'AbC+/9=';
        var _uinfo = {"Name":"Beverly","myKids":[{"Id":5629499534213121,"Name":"Wesley",
            "BDay":"10/12/2348","Note":"a ] in a string"}],"other":1};
    </script></html>"#;

    #[test]
    fn the_token_and_the_children_are_read_from_the_script() {
        let home = parse(PAGE).unwrap();
        assert_eq!(home.token, "AbC+/9=");
        assert_eq!(home.kids.len(), 1);
        assert_eq!(home.kids[0]["Name"], "Wesley");
        assert_eq!(home.kids[0]["Note"], "a ] in a string");
        assert_eq!(home.kids[0]["Id"].to_string(), "5629499534213121");
    }

    /// The login page has a form field named for the token but no value
    /// in a script, and no children.
    #[test]
    fn the_login_page_is_not_a_home_page() {
        let login = r#"<input type="hidden" id="__srf_token__" name="__srf_token__">"#;
        assert_eq!(parse(login), None);
    }
}
