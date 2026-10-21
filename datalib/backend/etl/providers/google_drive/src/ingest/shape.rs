//! Pure functions over what Google answered: putting the listing's
//! unordered arrays in an order, taking the image links out of a native
//! file, and packing the native JSON for the CAS.

use std::collections::BTreeSet;
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

/// Take every image link out of a native file, as `(JSON pointer, URL)`
/// in the order they appear: a Doc's `contentUri`s, a deck's
/// `contentUrl`s. Google signs each afresh on every read and it expires
/// in about 30 minutes, so left in, it would make an unedited file's JSON
/// differ from one read to the next. The image's bytes are stored by
/// where its link was, never by the URL.
pub fn take_image_links(native: &mut Value) -> Vec<(String, String)> {
    let mut out = Vec::new();
    walk(native, &mut String::new(), &mut out);
    out
}

/// The links [`take_image_links`] took, as the capture's volatile half:
/// `{"image_links": {"<pointer>": "<url>"}}`, or `None` for none.
pub fn image_links_volatile(links: &[(String, String)]) -> Option<Value> {
    if links.is_empty() {
        return None;
    }
    let by_pointer: serde_json::Map<String, Value> = links
        .iter()
        .map(|(pointer, url)| (pointer.clone(), Value::String(url.clone())))
        .collect();
    Some(serde_json::json!({ "image_links": by_pointer }))
}

fn is_image_link(key: &str, v: &Value) -> bool {
    (key == "contentUri" || key == "contentUrl")
        && v.as_str().is_some_and(|url| url.starts_with("https://"))
}

fn walk(v: &mut Value, at: &mut String, out: &mut Vec<(String, String)>) {
    match v {
        Value::Object(map) => {
            let links: Vec<String> = map
                .iter()
                .filter(|(key, child)| is_image_link(key, child))
                .map(|(key, _)| key.clone())
                .collect();
            for key in links {
                if let Some(Value::String(url)) = map.remove(&key) {
                    out.push((format!("{at}/{}", escape(&key)), url));
                }
            }
            for (key, child) in map.iter_mut() {
                let len = at.len();
                at.push('/');
                at.push_str(&escape(key));
                walk(child, at, out);
                at.truncate(len);
            }
        }
        Value::Array(items) => {
            for (i, child) in items.iter_mut().enumerate() {
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

fn escape(key: &str) -> String {
    key.replace('~', "~0").replace('/', "~1")
}

/// Every item either listing named, keyed by `id`, oldest first by
/// `modifiedTime`, and the ids only one of them named.
pub fn union_of_listings(a: &[Value], b: &[Value]) -> (Vec<Value>, Vec<String>) {
    let id_of = |v: &Value| v.get("id").and_then(Value::as_str).map(str::to_string);
    let ids_a: BTreeSet<String> = a.iter().filter_map(id_of).collect();
    let ids_b: BTreeSet<String> = b.iter().filter_map(id_of).collect();
    let disagreed: Vec<String> = ids_a.symmetric_difference(&ids_b).cloned().collect();
    let mut seen = BTreeSet::new();
    let mut out: Vec<Value> = b
        .iter()
        .chain(a)
        .filter(|v| id_of(v).is_some_and(|id| seen.insert(id)))
        .cloned()
        .collect();
    out.sort_by(|x, y| {
        let at = |v: &Value| {
            v.get("modifiedTime")
                .and_then(Value::as_str)
                .map(str::to_string)
        };
        at(x).cmp(&at(y))
    });
    (out, disagreed)
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
    fn image_links_are_taken_out_and_kept_by_where_they_were() {
        let mut doc = json!({"tabs": [{"documentTab": {"inlineObjects": {"kix.a/b": {
            "inlineObjectProperties": {"embeddedObject": {"imageProperties": {
                "contentUri": "https://docs.google.com/docs-images-rt/one",
                "sourceUri": "https://example.com/not-ours.png"}}}}}}}],
            "slides": [{"pageElements": [{"image": {"contentUrl": "https://docs.google.com/slides-images-rt/two"}}]}]});
        let mut found = take_image_links(&mut doc);
        found.sort();
        let one = "/tabs/0/documentTab/inlineObjects/kix.a~1b/inlineObjectProperties/\
                   embeddedObject/imageProperties/contentUri";
        assert_eq!(
            found,
            vec![
                (
                    "/slides/0/pageElements/0/image/contentUrl".to_string(),
                    "https://docs.google.com/slides-images-rt/two".to_string()
                ),
                (
                    one.to_string(),
                    "https://docs.google.com/docs-images-rt/one".to_string()
                ),
            ]
        );
        let props = doc.pointer(one.trim_end_matches("/contentUri")).unwrap();
        assert_eq!(
            props,
            &json!({"sourceUri": "https://example.com/not-ours.png"}),
            "the link is gone and nothing else is"
        );
        assert!(doc.pointer("/slides/0/pageElements/0/image").unwrap()["contentUrl"].is_null());
    }

    /// The same file read twice, its image links signed afresh, must
    /// pack to the same JSON.
    #[test]
    fn two_reads_with_fresh_links_store_the_same_json() {
        let read = |sig: &str| {
            json!({"documentId": "1enterprise", "inlineObjects": {"kix.1": {"inlineObjectProperties":
                {"embeddedObject": {"imageProperties": {"contentUri": format!("https://docs.google.com/docs-images-rt/x?sig={sig}")}}}}}})
        };
        let (mut a, mut b) = (read("first"), read("second"));
        take_image_links(&mut a);
        take_image_links(&mut b);
        assert_eq!(
            serde_json::to_vec(&a).unwrap(),
            serde_json::to_vec(&b).unwrap()
        );
    }

    /// The listing Drive cut short and the one right after it: the
    /// union is the whole list, in order, and names what one left out.
    #[test]
    fn a_revision_one_listing_left_out_is_kept() {
        let rev = |id: &str, at: &str| json!({"id": id, "modifiedTime": at});
        let short = vec![rev("4007", "2364-03-02"), rev("5308", "2364-03-03")];
        let whole = vec![
            rev("1", "2364-03-01"),
            rev("4007", "2364-03-02"),
            rev("5308", "2364-03-03"),
        ];
        for (a, b) in [(&short, &whole), (&whole, &short)] {
            let (all, disagreed) = union_of_listings(a, b);
            let ids: Vec<&str> = all.iter().map(|v| v["id"].as_str().unwrap()).collect();
            assert_eq!(ids, ["1", "4007", "5308"]);
            assert_eq!(disagreed, ["1"]);
        }
        let (all, disagreed) = union_of_listings(&whole, &whole);
        assert_eq!(all.len(), 3);
        assert!(disagreed.is_empty());
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
