//! Pure functions over what Google answered: putting the listing's
//! unordered arrays in an order, finding the images a native file points
//! at, and packing the native JSON for the CAS.

use std::io::Write as _;

use serde_json::Value;

/// A listing entry with its sets in a fixed order. Drive returns
/// `permissionIds`, `permissions` and `owners` in a different order from
/// one read to the next, and an unedited file must store the same bytes.
pub fn order_file_entry(entry: &Value) -> Value {
    let mut out = entry.clone();
    let Some(obj) = out.as_object_mut() else {
        return out;
    };
    for key in ["permissionIds", "parents", "spaces"] {
        if let Some(Value::Array(items)) = obj.get_mut(key) {
            items.sort_by_key(|v| v.to_string());
        }
    }
    for (key, by) in [("permissions", "id"), ("owners", "permissionId")] {
        if let Some(Value::Array(items)) = obj.get_mut(key) {
            items.sort_by(|a, b| {
                let ka = (a.get(by).map(Value::to_string), a.to_string());
                let kb = (b.get(by).map(Value::to_string), b.to_string());
                ka.cmp(&kb)
            });
        }
    }
    out
}

/// Every image link in a native file, as `(JSON pointer, URL)`, in the
/// order they appear: a Doc's `contentUri`s, a deck's `contentUrl`s.
/// Each is a short-lived signed URL, so it is fetched with the capture
/// and stored by where it was found, never by the URL.
pub fn image_links(native: &Value) -> Vec<(String, String)> {
    let mut out = Vec::new();
    walk(native, &mut String::new(), &mut out);
    out
}

fn walk(v: &Value, at: &mut String, out: &mut Vec<(String, String)>) {
    match v {
        Value::Object(map) => {
            for (key, child) in map {
                let len = at.len();
                at.push('/');
                at.push_str(&key.replace('~', "~0").replace('/', "~1"));
                match child {
                    Value::String(url)
                        if (key == "contentUri" || key == "contentUrl")
                            && url.starts_with("https://") =>
                    {
                        out.push((at.clone(), url.clone()));
                    }
                    _ => walk(child, at, out),
                }
                at.truncate(len);
            }
        }
        Value::Array(items) => {
            for (i, child) in items.iter().enumerate() {
                let len = at.len();
                at.push('/');
                at.push_str(&i.to_string());
                walk(child, at, out);
                at.truncate(len);
            }
        }
        _ => {}
    }
}

/// The native file's own revision token (Docs and Slides).
pub fn native_revision_id(native: &Value) -> Option<String> {
    native
        .get("revisionId")
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// `bytes` gzipped. The header's mtime is zero and the level fixed, so
/// the same input always packs to the same blob.
pub fn gzip(bytes: &[u8]) -> std::io::Result<Vec<u8>> {
    let mut enc = flate2::GzBuilder::new()
        .mtime(0)
        .write(Vec::new(), flate2::Compression::best());
    enc.write_all(bytes)?;
    enc.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::Read as _;

    #[test]
    fn sets_are_ordered_whatever_order_they_came_in() {
        let a = json!({"permissionIds": ["b", "a"],
                       "permissions": [{"id": "2", "role": "reader"}, {"id": "1", "role": "owner"}],
                       "name": "Ten Forward menu"});
        let b = json!({"permissionIds": ["a", "b"],
                       "permissions": [{"id": "1", "role": "owner"}, {"id": "2", "role": "reader"}],
                       "name": "Ten Forward menu"});
        assert_eq!(order_file_entry(&a), order_file_entry(&b));
        assert_eq!(order_file_entry(&a)["permissions"][0]["id"], "1");
    }

    #[test]
    fn images_are_found_by_where_they_are() {
        let doc = json!({"tabs": [{"documentTab": {"inlineObjects": {"kix.a/b": {
            "inlineObjectProperties": {"embeddedObject": {"imageProperties": {
                "contentUri": "https://docs.google.com/docs-images-rt/one",
                "sourceUri": "https://example.com/not-ours.png"}}}}}}}],
            "slides": [{"pageElements": [{"image": {"contentUrl": "https://docs.google.com/slides-images-rt/two"}}]}]});
        let mut found = image_links(&doc);
        found.sort();
        assert_eq!(
            found,
            vec![
                (
                    "/slides/0/pageElements/0/image/contentUrl".to_string(),
                    "https://docs.google.com/slides-images-rt/two".to_string()
                ),
                (
                    "/tabs/0/documentTab/inlineObjects/kix.a~1b/inlineObjectProperties/\
                     embeddedObject/imageProperties/contentUri"
                        .to_string(),
                    "https://docs.google.com/docs-images-rt/one".to_string()
                ),
            ]
        );
    }

    #[test]
    fn gzip_is_deterministic_and_round_trips() {
        let body = br#"{"documentId":"1enterprise","title":"Captain's log"}"#;
        let a = gzip(body).unwrap();
        assert_eq!(a, gzip(body).unwrap());
        let mut back = Vec::new();
        flate2::read::GzDecoder::new(&a[..])
            .read_to_end(&mut back)
            .unwrap();
        assert_eq!(back, body);
    }
}
