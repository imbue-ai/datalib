//! A content hash: what the document *is*, not what the file says
//! about itself.
//!
//! # Why this exists next to `blake3`
//!
//! `pdf_documents.blake3` is the hash of the file's bytes, and it is
//! the primary key for good reasons ([`super::schema_raw`] §"Why two
//! tables"). But it moves for changes that alter nothing a reader would
//! see. Retitle a PDF, add an XMP tag, let a tool regenerate the
//! trailer `/ID`, and the byte hash says "new document" — a fresh row,
//! a fresh `document_uuid`, a fresh markdown file, and (because
//! `pdf_documents` is never truncated) the old row left behind forever.
//!
//! [`compute`] gives a second, coarser identity that survives exactly
//! those edits, so "the same document, re-annotated" is answerable:
//!
//! ```sql
//! SELECT blake3, title, doc_modified_at FROM pdf_documents
//!  WHERE content_blake3 = ? ORDER BY doc_modified_at;
//! ```
//!
//! Unlike `xmp_document_id` — which DOWNLOAD.md measured at 3/20
//! populated and which `cp` happily duplicates — this is computed by
//! us, so it is present for every parseable document and cannot be
//! forged by copying. It is still **a hint, not a key**, for the
//! reasons in §"What it does not survive".
//!
//! # How it works
//!
//! A PDF is not header-then-body: there is no offset where metadata
//! ends and content begins. It is a flat bag of numbered objects plus
//! an `xref` index recording where each one starts. Metadata is not a
//! *region* — it is objects that the trailer and catalog point at by
//! number. So instead of slicing a byte range, we pick objects:
//!
//! **Hash every object reachable from the document catalog, in object-id
//! order, with the `/Metadata` key stripped wherever it appears.**
//!
//! Reachability from the catalog is doing most of the work, and it
//! excludes three things for free:
//!
//! * **The Info dictionary** — `/Title`, `/Author`, `/CreationDate`,
//!   `/ModDate`, `/Producer`. It hangs off the *trailer*, not the
//!   catalog, so nothing reaches it. (Pinned by
//!   `info_dictionary_is_unreachable_from_the_catalog`.)
//! * **The trailer `/ID` array and the xref table**, which are not
//!   objects at all.
//! * **Orphans.** A tool that writes a metadata edit as an incremental
//!   update with a *fresh* object number leaves the superseded Info
//!   dict in the file, still live in the xref. Hashing "every object
//!   except the metadata ones" would fold that corpse in as content;
//!   reachability drops it.
//!
//! The one metadata object the catalog *does* point at is the XMP
//! packet (`/Metadata`), so that key is stripped from every dictionary
//! we serialize — which also means adding XMP to a file that had none
//! is a no-op rather than a catalog change.
//!
//! Nothing is decompressed. [`encode`] writes `stream.content`
//! verbatim, so page content streams, embedded font programs and image
//! XObjects are hashed as the compressed bytes they already are. The
//! only inflation is whatever `Document::load_mem` must do to read
//! object streams, where PDF 1.5+ writers pack the catalog, the page
//! tree and the Info dict together into one Flate blob — there, the
//! metadata genuinely cannot be separated from the structure without
//! inflating it, and lopdf does that as part of parsing regardless.
//!
//! # What it does not survive
//!
//! **Renumbering.** Object bodies carry literal cross-references
//! (`/Contents 4 0 R`), so a writer that renumbers objects changes
//! those bytes even when nothing visual moves. Acrobat "Save As",
//! `qpdf --linearize` and Ghostscript all rewrite this way. The hash
//! holds for append-style editors (`exiftool`, `pdftk update_info`),
//! which is the common shape of "I edited the metadata".
//!
//! **Re-compression.** Streams are hashed compressed, so re-deflating
//! at a different level changes the hash for identical pixels. Fixing
//! that means inflating every image and font in the corpus; the cost is
//! real and the win is narrow, since a tool that re-compresses is
//! almost always one that also renumbers.
//!
//! Both failures are one-directional and that is the direction we want:
//! it can report "different" for documents that look the same, never
//! "same" for documents that differ. A false split costs a duplicate
//! row. A false merge would hide a document.
//!
//! **Annotations are deliberately included.** Acrobat highlights and
//! sticky notes live in each page's `/Annots`, which the catalog
//! reaches, so marking a PDF up changes its content hash. That is the
//! conservative reading: those marks are visible, and a hash that
//! called a highlighted document identical to a clean one would be
//! claiming something false about what a reader sees. Excluding them
//! would be a defensible different choice — it would make the hash mean
//! "the same underlying document, however marked up" — but it should be
//! a decision, not an accident, so it is written down here.
//!
//! Encrypted documents get `None`: strings and streams are ciphertext
//! keyed off the very `/ID` we are trying to ignore, so the bytes churn
//! on every save. Unparseable ones get `None` too, matching
//! [`super::identity`]'s policy — conversion is the job, lineage is a
//! bonus.

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
///
/// Real documents nest single digits deep. A file that exceeds this is
/// malformed or hostile, and the safe answer is "no opinion" rather
/// than either a blown stack or a hash over a truncated object.
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

/// The catalog's object id, via the trailer `/Root`.
fn catalog_id(doc: &Document) -> Option<ObjectId> {
    match doc.trailer.get(b"Root").ok()? {
        Object::Reference(id) => Some(*id),
        _ => None,
    }
}

/// Every object id reachable from the catalog, as a sorted set.
///
/// The set is collected by traversal but *hashed* in id order, so the
/// result cannot depend on the order edges happen to be walked. The
/// `seen` set doubles as cycle detection, which is not optional: every
/// real PDF has a cycle, since each page's `/Parent` points back at the
/// page-tree node that lists it in `/Kids`.
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

/// Push every `Reference` inside one object onto `out`.
///
/// Iterative rather than recursive: nesting depth is producer-
/// controlled, and a deeply nested array in a malformed file should not
/// take the process down with a blown stack.
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

// ─────────────────────────────────────────────────────────────────────
// Encoding
// ─────────────────────────────────────────────────────────────────────

/// Serialize one object into `out`. Returns `false` if it nests deeper
/// than [`MAX_DEPTH`], which the caller turns into `None`.
///
/// This is **our** encoding, not PDF syntax, for two reasons. Borrowing
/// `lopdf`'s writer would tie every stored hash to that crate's output
/// bytes, so a routine version bump could silently make every row
/// incomparable with every new scan. And PDF syntax is not canonical:
/// dictionary key order is an artifact of how the producer happened to
/// write the file, so [`encode_dict`] sorts keys and two files that
/// differ only in key order hash alike.
///
/// It only has to be injective, not readable: every branch writes a
/// distinct type tag, and every variable-length field is
/// length-prefixed, so no two distinct objects can encode to the same
/// bytes.
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

/// Tag 7. Keys are sorted and [`STRIPPED_KEY`] is dropped.
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

/// Length-prefixed bytes. The prefix is what makes the encoding
/// injective: without it `/AB` followed by `/C` and `/A` followed by
/// `/BC` would produce the same bytes.
fn push_bytes(out: &mut Vec<u8>, b: &[u8]) {
    out.extend_from_slice(&(b.len() as u64).to_be_bytes());
    out.extend_from_slice(b);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Assemble numbered objects into a parseable PDF with a correct
    /// xref table — the same shape `//tests/fixtures/make_pdf_fixtures.py`
    /// builds, in Rust so these tests need no fixture data dep.
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

    /// A one-page document with parameterised metadata. Object numbers:
    /// 1 catalog, 2 page tree, 3 page, 4 content stream, 5 XMP, 6 Info.
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
