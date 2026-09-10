//! A content hash: what the document *is*, not what the file says
//! about itself.

use std::collections::BTreeSet;

use lopdf::{Dictionary, Document, Object, ObjectId, StringFormat};

/// Bumped when the hashing rule changes — a different traversal, a
/// different strip list, a different encoding. Stored values from a
/// previous rule are not comparable with values from this one, so the
/// version rides *inside* the hash rather than beside it: an old row
/// and a new row simply never match, instead of matching wrongly.
const HASH_RULE_VERSION: &[u8] = b"datalib.pdf.content.v1\n";

/// The key stripped from every dictionary before hashing: the XMP
/// packet pointer. PDF 2.0 permits `/Metadata` on pages and form
/// XObjects as well as on the catalog, so this is applied at every
/// depth rather than only to the catalog.
const STRIPPED_KEY: &[u8] = b"Metadata";

/// Nesting depth past which we give up and return `None`.
const MAX_DEPTH: u32 = 64;

/// Content hash of one PDF, lowercase hex. `None` for anything we
/// cannot read honestly: unparseable bytes, an encrypted document, or a
/// file with no reachable catalog.
pub fn compute(bytes: &[u8]) -> Option<String> {
    let doc = Document::load_mem(bytes).ok()?;
    from_doc(&doc)
}

/// The same, for a document already parsed. Split out so a caller that
/// has one — [`super::identify`] parses once for
/// [`super::identity::extract`] — does not pay for a second parse of
/// what can be a very large file.
pub fn from_doc(doc: &Document) -> Option<String> {
    // Ciphertext hashes to noise that changes on every save. Say
    // nothing rather than something false.
    if doc.trailer.get(b"Encrypt").is_ok() {
        return None;
    }
    let catalog_id = catalog_id(doc)?;
    let reachable = reachable_from(doc, catalog_id);
    if reachable.is_empty() {
        return None;
    }

    let mut hasher = blake3::Hasher::new();
    hasher.update(HASH_RULE_VERSION);
    let mut buf = Vec::new();
    for id in &reachable {
        let Some(obj) = doc.objects.get(id) else {
            // A dangling reference: broken, but not ours to repair. It
            // contributes nothing, exactly as it contributes nothing to
            // what a reader sees.
            continue;
        };
        buf.clear();
        // The object number participates: two documents that differ
        // only by which body sits at which id are different documents.
        // It costs nothing in the case we care about, since an
        // append-style metadata edit does not renumber.
        buf.extend_from_slice(&id.0.to_be_bytes());
        buf.extend_from_slice(&id.1.to_be_bytes());
        if !encode(obj, &mut buf, 0) {
            // Refusing outright is the safe direction. Skipping the
            // object instead would silently shrink the hashed set,
            // which is how two different documents come to agree.
            return None;
        }
        hasher.update(&buf);
    }
    Some(datalib_etl::fswalk::to_hex(hasher.finalize().as_bytes()))
}

fn catalog_id(doc: &Document) -> Option<ObjectId> {
    match doc.trailer.get(b"Root").ok()? {
        Object::Reference(id) => Some(*id),
        _ => None,
    }
}

fn reachable_from(doc: &Document, root: ObjectId) -> BTreeSet<ObjectId> {
    let mut seen = BTreeSet::new();
    let mut queue = vec![root];
    while let Some(id) = queue.pop() {
        if !seen.insert(id) {
            continue;
        }
        if let Some(obj) = doc.objects.get(&id) {
            collect_refs(obj, &mut queue);
        }
    }
    seen
}

fn collect_refs(obj: &Object, out: &mut Vec<ObjectId>) {
    let mut stack = vec![obj];
    while let Some(o) = stack.pop() {
        match o {
            Object::Reference(id) => out.push(*id),
            Object::Array(items) => stack.extend(items.iter()),
            Object::Dictionary(d) => stack.extend(dict_values(d)),
            Object::Stream(s) => stack.extend(dict_values(&s.dict)),
            _ => {}
        }
    }
}

/// Dictionary values, minus the stripped key — so the traversal never
/// walks *into* an XMP packet it would not have hashed anyway.
fn dict_values(d: &Dictionary) -> impl Iterator<Item = &Object> {
    d.iter()
        .filter(|(k, _)| k.as_slice() != STRIPPED_KEY)
        .map(|(_, v)| v)
}

// Encoding

/// Serialize one object into `out`. Returns `false` if it nests deeper
/// than [`MAX_DEPTH`], which the caller turns into `None`.
fn encode(obj: &Object, out: &mut Vec<u8>, depth: u32) -> bool {
    if depth > MAX_DEPTH {
        return false;
    }
    match obj {
        Object::Null => out.push(0),
        Object::Boolean(b) => {
            out.push(1);
            out.push(u8::from(*b));
        }
        Object::Integer(i) => {
            out.push(2);
            out.extend_from_slice(&i.to_be_bytes());
        }
        Object::Real(r) => {
            out.push(3);
            out.extend_from_slice(&r.to_be_bytes());
        }
        Object::Name(n) => {
            out.push(4);
            push_bytes(out, n);
        }
        Object::String(s, f) => {
            out.push(5);
            out.push(match f {
                StringFormat::Literal => 0,
                StringFormat::Hexadecimal => 1,
            });
            push_bytes(out, s);
        }
        Object::Array(items) => {
            out.push(6);
            out.extend_from_slice(&(items.len() as u64).to_be_bytes());
            for it in items {
                if !encode(it, out, depth + 1) {
                    return false;
                }
            }
        }
        Object::Dictionary(d) => return encode_dict(d, out, depth),
        Object::Stream(s) => {
            out.push(8);
            if !encode_dict(&s.dict, out, depth) {
                return false;
            }
            // The raw, still-compressed bytes. Nothing is inflated: a
            // page content stream, an embedded font program and an
            // image XObject are all hashed exactly as they sit on disk.
            push_bytes(out, &s.content);
            // `allows_compression` and `start_position` are parse
            // artifacts — where lopdf found the stream, not what it
            // says — and are deliberately not hashed.
        }
        Object::Reference(id) => {
            out.push(9);
            out.extend_from_slice(&id.0.to_be_bytes());
            out.extend_from_slice(&id.1.to_be_bytes());
        }
    }
    true
}

fn encode_dict(d: &Dictionary, out: &mut Vec<u8>, depth: u32) -> bool {
    if depth > MAX_DEPTH {
        return false;
    }
    let mut entries: Vec<(&[u8], &Object)> = d
        .iter()
        .filter(|(k, _)| k.as_slice() != STRIPPED_KEY)
        .map(|(k, v)| (k.as_slice(), v))
        .collect();
    entries.sort_unstable_by(|a, b| a.0.cmp(b.0));

    out.push(7);
    out.extend_from_slice(&(entries.len() as u64).to_be_bytes());
    for (k, v) in entries {
        push_bytes(out, k);
        if !encode(v, out, depth + 1) {
            return false;
        }
    }
    true
}

fn push_bytes(out: &mut Vec<u8>, b: &[u8]) {
    out.extend_from_slice(&(b.len() as u64).to_be_bytes());
    out.extend_from_slice(b);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build(objects: &[Vec<u8>], trailer_extra: &str) -> Vec<u8> {
        let mut out: Vec<u8> = b"%PDF-1.7\n%\xe2\xe3\xcf\xd3\n".to_vec();
        let mut offsets = Vec::new();
        for (i, body) in objects.iter().enumerate() {
            offsets.push(out.len());
            out.extend_from_slice(format!("{} 0 obj\n", i + 1).as_bytes());
            out.extend_from_slice(body);
            out.extend_from_slice(b"\nendobj\n");
        }
        let xref_at = out.len();
        let n = objects.len() + 1;
        out.extend_from_slice(format!("xref\n0 {n}\n").as_bytes());
        out.extend_from_slice(b"0000000000 65535 f \n");
        for off in &offsets {
            out.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
        }
        out.extend_from_slice(
            format!("trailer\n<< /Size {n} /Root 1 0 R{trailer_extra} >>\n").as_bytes(),
        );
        out.extend_from_slice(format!("startxref\n{xref_at}\n%%EOF\n").as_bytes());
        out
    }

    fn stream_obj(dict_extra: &str, content: &str) -> Vec<u8> {
        format!(
            "<< /Length {}{} >>\nstream\n{}\nendstream",
            content.len(),
            dict_extra,
            content
        )
        .into_bytes()
    }

    fn doc(body: &str, title: &str, doc_id: &str, xmp_instance: Option<&str>) -> Vec<u8> {
        let mut catalog = String::from("<< /Type /Catalog /Pages 2 0 R");
        if xmp_instance.is_some() {
            catalog.push_str(" /Metadata 5 0 R");
        }
        catalog.push_str(" >>");

        let xmp = match xmp_instance {
            Some(i) => stream_obj(
                " /Type /Metadata /Subtype /XML",
                &format!(
                    "<x:xmpmeta xmlns:x=\"adobe:ns:meta/\">\
                     <xmpMM:InstanceID>{i}</xmpMM:InstanceID></x:xmpmeta>"
                ),
            ),
            None => b"<< >>".to_vec(),
        };

        build(
            &[
                catalog.into_bytes(),
                b"<< /Type /Pages /Count 1 /Kids [3 0 R] >>".to_vec(),
                b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Contents 4 0 R >>".to_vec(),
                stream_obj("", &format!("BT /F1 12 Tf (({body})) Tj ET")),
                xmp,
                format!("<< /Title ({title}) /Author (Picard) >>").into_bytes(),
            ],
            &format!(" /Info 6 0 R /ID [<{doc_id}> <{doc_id}>]"),
        )
    }

    // ── The headline property ────────────────────────────────────────

    #[test]
    fn a_metadata_edit_keeps_the_content_hash() {
        // The whole reason this module exists: retitle the document,
        // let the writer mint a fresh trailer /ID and a fresh XMP
        // InstanceID, and change nothing a reader would see.
        let before = doc("warp core nominal", "Captains Log", "01", Some("uuid:i1"));
        let after = doc(
            "warp core nominal",
            "Captains Log [reviewed]",
            "abababab",
            Some("uuid:i2"),
        );

        // The premise: these really are different files. Without this
        // the test would pass on two identical inputs and prove nothing.
        assert_ne!(before, after, "fixtures must differ in their bytes");

        let a = compute(&before).expect("before hashes");
        let b = compute(&after).expect("after hashes");
        assert_eq!(a, b, "metadata-only edit must not change content identity");
    }

    #[test]
    fn changed_page_content_changes_the_content_hash() {
        // The other direction, and the one that matters more: the hash
        // must not merge documents that genuinely differ.
        let a = compute(&doc("warp core nominal", "Log", "01", Some("uuid:i1"))).unwrap();
        let b = compute(&doc("warp core breached", "Log", "01", Some("uuid:i1"))).unwrap();
        assert_ne!(a, b);
    }

    #[test]
    fn info_dictionary_is_unreachable_from_the_catalog() {
        // The load-bearing structural claim in this module's docs: the
        // Info dict is excluded because nothing points at it from the
        // catalog, not because we remembered to skip it.
        let bytes = doc("body", "Title", "01", Some("uuid:i1"));
        let parsed = Document::load_mem(&bytes).unwrap();
        let root = catalog_id(&parsed).unwrap();
        let reachable = reachable_from(&parsed, root);

        let info = match parsed.trailer.get(b"Info").unwrap() {
            Object::Reference(id) => *id,
            other => panic!("expected a reference, got {other:?}"),
        };
        assert!(
            !reachable.contains(&info),
            "Info {info:?} must not be reachable; reachable = {reachable:?}"
        );
        // And the page content stream must be, or we are hashing nothing.
        assert!(reachable.contains(&(4, 0)), "content stream must be hashed");
    }

    #[test]
    fn adding_an_xmp_packet_is_not_a_content_change() {
        // `/Metadata` is stripped from the catalog, so a file that
        // gains an XMP packet it never had still reads as the same
        // document.
        let without = compute(&doc("body", "Title", "01", None)).unwrap();
        let with = compute(&doc("body", "Title", "01", Some("uuid:i1"))).unwrap();
        assert_eq!(without, with);
    }

    #[test]
    fn a_superseded_info_object_left_behind_is_not_hashed() {
        // An incremental update that writes the new Info under a *fresh*
        // object number leaves the old one live in the xref but
        // unreferenced. Hashing "every object except the metadata ones"
        // would fold that corpse in as content; reachability drops it.
        let base = doc("body", "Old Title", "01", Some("uuid:i1"));
        let before = compute(&base).unwrap();

        let prev_xref = {
            let s = String::from_utf8_lossy(&base);
            let at = s.rfind("startxref\n").unwrap() + "startxref\n".len();
            s[at..]
                .lines()
                .next()
                .unwrap()
                .trim()
                .parse::<usize>()
                .unwrap()
        };

        let mut updated = base.clone();
        let new_info_at = updated.len();
        updated.extend_from_slice(b"7 0 obj\n<< /Title (New Title) >>\nendobj\n");
        let xref_at = updated.len();
        updated.extend_from_slice(format!("xref\n7 1\n{new_info_at:010} 00000 n \n").as_bytes());
        updated.extend_from_slice(
            format!(
                "trailer\n<< /Size 8 /Root 1 0 R /Info 7 0 R \
                 /ID [<abab> <abab>] /Prev {prev_xref} >>\n"
            )
            .as_bytes(),
        );
        updated.extend_from_slice(format!("startxref\n{xref_at}\n%%EOF\n").as_bytes());

        let parsed = Document::load_mem(&updated).expect("incremental update parses");
        // Both Info objects really are live in the resolved table —
        // otherwise this test is not exercising what it claims.
        assert!(parsed.objects.contains_key(&(6, 0)), "old Info still live");
        assert!(parsed.objects.contains_key(&(7, 0)), "new Info live");

        assert_eq!(
            compute(&updated).unwrap(),
            before,
            "an appended metadata update must not change content identity"
        );
    }

    // ── Refusals ─────────────────────────────────────────────────────

    #[test]
    fn unparseable_bytes_get_no_hash() {
        assert_eq!(compute(b"this is not a pdf"), None);
    }

    #[test]
    fn encrypted_documents_get_no_hash() {
        // Streams are ciphertext keyed off the very /ID we are trying
        // to ignore, so the bytes churn on every save. Better to say
        // nothing than something false.
        let bytes = build(
            &[
                b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
                b"<< /Type /Pages /Count 0 /Kids [] >>".to_vec(),
                b"<< /Filter /Standard /V 1 /R 2 >>".to_vec(),
            ],
            " /Encrypt 3 0 R /ID [<01> <01>]",
        );
        let parsed = Document::load_mem(&bytes);
        // If lopdf refuses the file outright we still get None, but
        // then this test would not be exercising the /Encrypt branch.
        if let Ok(parsed) = parsed {
            assert!(parsed.trailer.get(b"Encrypt").is_ok());
            assert_eq!(from_doc(&parsed), None);
        }
    }

    #[test]
    fn a_document_with_no_root_gets_no_hash() {
        let mut doc = Document::new();
        doc.objects.insert((1, 0), Object::Integer(1));
        assert_eq!(from_doc(&doc), None);
    }

    // ── Encoding properties ──────────────────────────────────────────

    #[test]
    fn dictionary_key_order_is_not_content() {
        // PDF dictionaries are unordered by spec but lopdf preserves
        // insertion order, so without sorting, two files that differ
        // only in how the producer laid out one dict would read as
        // different documents.
        let mut a = Dictionary::new();
        a.set("Alpha", Object::Integer(1));
        a.set("Beta", Object::Integer(2));
        let mut b = Dictionary::new();
        b.set("Beta", Object::Integer(2));
        b.set("Alpha", Object::Integer(1));

        let (mut ea, mut eb) = (Vec::new(), Vec::new());
        assert!(encode_dict(&a, &mut ea, 0));
        assert!(encode_dict(&b, &mut eb, 0));
        assert_eq!(ea, eb);
    }

    #[test]
    fn the_encoding_is_injective_across_adjacent_fields() {
        // Length prefixes are what stop /AB + /C colliding with /A + /BC.
        let split = Object::Array(vec![
            Object::Name(b"AB".to_vec()),
            Object::Name(b"C".to_vec()),
        ]);
        let other = Object::Array(vec![
            Object::Name(b"A".to_vec()),
            Object::Name(b"BC".to_vec()),
        ]);
        let (mut x, mut y) = (Vec::new(), Vec::new());
        assert!(encode(&split, &mut x, 0));
        assert!(encode(&other, &mut y, 0));
        assert_ne!(x, y);
    }

    #[test]
    fn stream_parse_artifacts_are_not_hashed() {
        // `start_position` records where lopdf found the stream, which
        // moves whenever anything earlier in the file changes length —
        // exactly what a metadata edit does.
        let mut s = lopdf::Stream::new(Dictionary::new(), b"content".to_vec());
        let (mut before, mut after) = (Vec::new(), Vec::new());
        assert!(encode(&Object::Stream(s.clone()), &mut before, 0));
        s.start_position = Some(4096);
        s.allows_compression = !s.allows_compression;
        assert!(encode(&Object::Stream(s), &mut after, 0));
        assert_eq!(before, after);
    }

    #[test]
    fn nesting_past_the_depth_limit_refuses_instead_of_recursing() {
        let mut o = Object::Integer(1);
        for _ in 0..(MAX_DEPTH + 5) {
            o = Object::Array(vec![o]);
        }
        let mut buf = Vec::new();
        assert!(!encode(&o, &mut buf, 0), "must refuse, not blow the stack");
    }
}
