//! How far a PDF may expand as it is read. A person may be handed a PDF, and
//! two kilobytes of one can ask a parser for gigabytes, so this measures a
//! file before `lopdf` or `pdf-inspector` reads it. `pdf-inspector` takes
//! bytes and no limit, so no setting reaches the streams it inflates itself.
//! `INGEST.md` § "A PDF can be made by anyone" has the rules.

use std::io::Read;

use anyhow::{bail, Result};
use lopdf::xref::XrefEntry;
use lopdf::{
    dictionary, DecompressError, Dictionary, Document, Error, LoadOptions, Object, ObjectId,
    ObjectStream, Stream,
};

/// What one document may cost.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// One stream, decoded. Far past page content or a font; a Flate image
    /// bigger than this is a colour sheet about 80 cm square at 600 dpi.
    pub stream_bytes: usize,
    /// Every stream together, which bounds the time spent measuring.
    pub document_bytes: u64,
    /// Every object in the object streams a loader expands. An object costs
    /// about 70 bytes in memory, and two bytes of a stream can be one.
    pub objects: u64,
}

pub const LIMITS: Limits = Limits {
    stream_bytes: 1 << 30,
    document_bytes: 64 << 30,
    objects: 64_000_000,
};

/// An object stream past this is left out of the document, with what it
/// holds. `pdf-inspector` loads with this bound and cannot be told another,
/// so ours matches it: a tagged PDF's structure tree can reach hundreds of
/// MB, and a loader that made objects of it would hold gigabytes.
pub const EXPANDED_OBJECT_STREAM_BYTES: usize = 8 << 20;

/// What the measuring found out about a document it let through.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Checked {
    /// A loader will leave out at least one object stream, so what is
    /// reachable in the document is not everything it holds.
    pub left_out_object_streams: bool,
}

/// The document, with the limit [`check`] assumes.
pub fn load(bytes: &[u8]) -> lopdf::Result<Document> {
    Document::load_mem_with_options(
        bytes,
        LoadOptions {
            max_decompressed_size: Some(EXPANDED_OBJECT_STREAM_BYTES),
            ..Default::default()
        },
    )
}

pub fn check(bytes: &[u8]) -> Result<Checked> {
    check_within(bytes, &LIMITS)
}

pub fn check_within(bytes: &[u8], limits: &Limits) -> Result<Checked> {
    // A file that does not open has no stream to measure, and the parser
    // that refuses it next says what is wrong with it.
    let Ok(doc) = Document::load_mem_with_options(
        bytes,
        LoadOptions {
            filter: Some(leave_object_stream_unexpanded),
            max_decompressed_size: Some(EXPANDED_OBJECT_STREAM_BYTES),
            ..Default::default()
        },
    ) else {
        return Ok(Checked::default());
    };

    let mut checked = Checked::default();
    let mut budget = Budget::new(limits);
    for (id, object) in &doc.objects {
        if let Object::Stream(stream) = object {
            measure_stream(*id, stream, &mut budget, &mut checked)?;
        }
    }
    for (id, span) in spans_of_unparsed_objects(bytes, &doc) {
        if let Some(stream) = stream_written_in(span) {
            measure_stream(id, &stream, &mut budget, &mut checked)?;
        }
    }
    Ok(checked)
}

fn measure_stream(
    id: ObjectId,
    stream: &Stream,
    budget: &mut Budget,
    checked: &mut Checked,
) -> Result<()> {
    let limit = budget.limits.stream_bytes;
    let decoded = match decoded_len(stream, limit) {
        Ok(decoded) => decoded,
        Err(Error::Decompress(DecompressError::MemoryLimitExceeded { limit })) => {
            bail!(
                "object {} {} expands to more than {limit} bytes, too far for a real PDF, \
                 so the file was not read",
                id.0,
                id.1
            )
        }
        // A filter that does not decode here (a JPEG, say) does not decode
        // for the parsers either, and has cost nothing.
        Err(_) => return Ok(()),
    };
    budget.spend_bytes(decoded)?;

    if !is_object_stream(&stream.dict) {
        return Ok(());
    }
    if decoded > EXPANDED_OBJECT_STREAM_BYTES {
        checked.left_out_object_streams = true;
    } else if let Ok(content) = stream.get_plain_content_with_limit(limit) {
        measure_object_stream(id, &stream.dict, &content, budget)?;
    }
    Ok(())
}

/// What the cross-reference table lists and the loader could not parse, with
/// the bytes it spans. A dictionary with a numeral no parser holds is one, and
/// `pdf-inspector` repairs the bytes and loads again, so what it could not
/// parse may yet be read.
fn spans_of_unparsed_objects<'a>(
    bytes: &'a [u8],
    doc: &'a Document,
) -> impl Iterator<Item = (ObjectId, &'a [u8])> {
    // Offsets count from the header, as the loader counts them.
    let header = find(bytes, b"%PDF-").unwrap_or(0);
    let body = &bytes[header..];
    let mut listed: Vec<(usize, ObjectId)> = doc
        .reference_table
        .entries
        .iter()
        .filter_map(|(number, entry)| match entry {
            XrefEntry::Normal { offset, generation } => {
                Some((*offset as usize, (*number, *generation)))
            }
            _ => None,
        })
        .collect();
    listed.sort_unstable();
    let ends: Vec<usize> = listed
        .iter()
        .skip(1)
        .map(|(offset, _)| *offset)
        .chain([body.len()])
        .collect();
    listed
        .into_iter()
        .zip(ends)
        .filter(|((_, id), _)| !doc.objects.contains_key(id))
        .filter_map(|((start, id), end)| Some((id, body.get(start..end)?)))
}

/// The stream in one object's bytes, with the filters its dictionary names, for
/// when no parser will read the dictionary.
fn stream_written_in(span: &[u8]) -> Option<Stream> {
    let keyword = find(span, b"stream")?;
    let dict = &span[..keyword];
    let after = &span[keyword + b"stream".len()..];
    let data = after.strip_prefix(b"\r").unwrap_or(after);
    let data = data.strip_prefix(b"\n").unwrap_or(data);
    let data = find(data, b"endstream").map_or(data, |end| &data[..end]);
    Some(Stream::new(
        dictionary! { "Filter" => filters_written_in(dict) },
        data.to_vec(),
    ))
}

fn filters_written_in(dict: &[u8]) -> Vec<Object> {
    const BETWEEN_NAMES: &[u8] = b"/ \t\r\n[]<>";
    let Some(at) = find(dict, b"/Filter") else {
        return Vec::new();
    };
    let value = dict[at + b"/Filter".len()..].trim_ascii_start();
    let names = match value.strip_prefix(b"[") {
        Some(list) => list.split(|b| *b == b']').next().unwrap_or(list),
        None => {
            let name = value.strip_prefix(b"/").unwrap_or(value);
            let end = name.iter().position(|b| BETWEEN_NAMES.contains(b));
            &name[..end.unwrap_or(name.len())]
        }
    };
    names
        .split(|b| BETWEEN_NAMES.contains(b))
        .filter(|name| !name.is_empty())
        .map(|name| Object::Name(name.to_vec()))
        .collect()
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// What `stream` decodes to, or `lopdf`'s refusal of it.
fn decoded_len(stream: &Stream, limit: usize) -> lopdf::Result<usize> {
    let one_flate_layer = stream
        .filters()
        .is_ok_and(|filters| filters == [b"FlateDecode".as_slice()]);
    if one_flate_layer {
        if let Some(decoded) = flate_decoded_len(&stream.content, limit) {
            if decoded > limit {
                return Err(DecompressError::MemoryLimitExceeded { limit }.into());
            }
            return Ok(decoded);
        }
    }
    Ok(stream.get_plain_content_with_limit(limit)?.len())
}

/// How much one layer of Flate comes to, up to just past `limit`, counted as
/// it comes out so that a real document's big image costs what it did when
/// nothing inflated it. A reader keeps what came out before an error, so that
/// counts. Where nothing comes out it tries again another way, so `None`
/// leaves it to `lopdf`.
fn flate_decoded_len(input: &[u8], limit: usize) -> Option<usize> {
    let mut decoder = flate2::read::ZlibDecoder::new(input);
    let mut buffer = vec![0; 64 << 10];
    let mut decoded = 0usize;
    while decoded <= limit {
        match decoder.read(&mut buffer) {
            Ok(0) => break,
            Ok(n) => decoded += n,
            Err(_) if decoded > 0 => break,
            Err(_) => return None,
        }
    }
    Some(decoded)
}

/// Reads each object in the span its index gives it, one at a time, so what
/// a loader would hold all at once is counted without being held. An object
/// that only parses past the end of its span overlaps the next, and a loader
/// would read what they share again for each of them.
fn measure_object_stream(
    id: ObjectId,
    dict: &Dictionary,
    content: &[u8],
    budget: &mut Budget,
) -> Result<()> {
    let starts = object_starts(dict, content);
    let ends = starts.iter().skip(1).copied().chain([content.len()]);
    for (&start, end) in starts.iter().zip(ends) {
        if let Some(object) = parse_object(&content[start..end]) {
            budget.spend_objects(objects_in(&object))?;
            continue;
        }
        let rest = &content[start..];
        budget.spend_bytes(rest.len())?;
        if parse_object(rest).is_some() {
            bail!(
                "object stream {} {} has entries that overlap, which reads one object \
                 over and over and which no PDF writer produces, so the file was not read",
                id.0,
                id.1
            );
        }
        // Malformed however far it is read: the loaders skip it too.
    }
    Ok(())
}

/// What is left of what one document may cost.
struct Budget<'a> {
    limits: &'a Limits,
    bytes_left: u64,
    objects_left: u64,
}

impl<'a> Budget<'a> {
    fn new(limits: &'a Limits) -> Self {
        Self {
            limits,
            bytes_left: limits.document_bytes,
            objects_left: limits.objects,
        }
    }

    fn spend_bytes(&mut self, n: usize) -> Result<()> {
        let Some(left) = self.bytes_left.checked_sub(n as u64) else {
            bail!(
                "its streams together expand to more than {} bytes, too far for a real \
                 PDF, so the file was not read",
                self.limits.document_bytes
            );
        };
        self.bytes_left = left;
        Ok(())
    }

    fn spend_objects(&mut self, n: u64) -> Result<()> {
        let Some(left) = self.objects_left.checked_sub(n) else {
            bail!(
                "its object streams hold more than {} objects, too many for a real PDF, \
                 so the file was not read",
                self.limits.objects
            );
        };
        self.objects_left = left;
        Ok(())
    }
}

const UNEXPANDED_OBJECT_STREAM: &[u8] = b"DatalibUnexpandedObjStm";

/// A loader expands what `/Type` calls an object stream as it meets it, so
/// the name goes first. `lopdf` keeps the object this was handed, and of the
/// answer reads only whether it is `None`.
fn leave_object_stream_unexpanded(id: ObjectId, object: &mut Object) -> Option<(ObjectId, Object)> {
    if let Object::Stream(stream) = object {
        if stream.dict.has_type(b"ObjStm") {
            stream
                .dict
                .set("Type", Object::Name(UNEXPANDED_OBJECT_STREAM.to_vec()));
        }
    }
    Some((id, Object::Null))
}

/// An encrypted document's streams keep their name: `lopdf` runs no filter
/// over them.
fn is_object_stream(dict: &Dictionary) -> bool {
    dict.has_type(UNEXPANDED_OBJECT_STREAM) || dict.has_type(b"ObjStm")
}

/// Where each object begins in `content`, in order, read as the loader reads
/// an object stream's index. Empty when the loader would find no index.
fn object_starts(dict: &Dictionary, content: &[u8]) -> Vec<usize> {
    let starts = || {
        let first = usize::try_from(dict.get(b"First").ok()?.as_i64().ok()?).ok()?;
        let index = std::str::from_utf8(content.get(..first)?).ok()?;
        let numbers: Vec<Option<u32>> = index
            .split_whitespace()
            .map(|number| number.parse().ok())
            .collect();
        let mut starts: Vec<usize> = numbers
            .as_chunks::<2>()
            .0
            .iter()
            .filter_map(|[id, offset]| {
                (*id)?;
                Some(first + (*offset)? as usize)
            })
            .filter(|start| *start < content.len())
            .collect();
        starts.sort_unstable();
        Some(starts)
    };
    starts().unwrap_or_default()
}

/// `lopdf`'s parser is private, so this asks it through a stream of one.
fn parse_object(text: &[u8]) -> Option<Object> {
    const INDEX: &[u8] = b"0 0 ";
    let mut content = Vec::with_capacity(INDEX.len() + text.len());
    content.extend_from_slice(INDEX);
    content.extend_from_slice(text);
    let stream = Stream::new(
        dictionary! { "N" => 1, "First" => INDEX.len() as i64 },
        content,
    );
    ObjectStream::new(&stream).ok()?.objects.remove(&(0, 0))
}

fn objects_in(object: &Object) -> u64 {
    1 + match object {
        Object::Array(items) => items.iter().map(objects_in).sum(),
        Object::Dictionary(dict) => dict.iter().map(|(_, value)| objects_in(value)).sum(),
        _ => 0,
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::io::Write;

    use flate2::write::ZlibEncoder;
    use flate2::Compression;

    use super::super::content_hash::tests::build;
    use super::*;

    fn zlib(data: &[u8]) -> Vec<u8> {
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::best());
        encoder.write_all(data).unwrap();
        encoder.finish().unwrap()
    }

    fn stream_obj(dict: &str, data: &[u8]) -> Vec<u8> {
        let mut out = format!("<< /Length {} {dict} >>\nstream\n", data.len()).into_bytes();
        out.extend_from_slice(data);
        out.extend_from_slice(b"\nendstream");
        out
    }

    /// One page, and `extra` as objects 6 and on.
    fn pdf(extra: &[Vec<u8>]) -> Vec<u8> {
        let mut objects = vec![
            b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
            b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Contents 4 0 R \
              /Resources << /Font << /F1 5 0 R >> >> >>"
                .to_vec(),
            stream_obj(
                "",
                b"BT /F1 12 Tf 72 720 Td (Warp core nominal, Captain.) Tj ET",
            ),
            b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_vec(),
        ];
        objects.extend_from_slice(extra);
        build(&objects, "")
    }

    fn object_stream(index: &str, body: &[u8]) -> Vec<u8> {
        let mut content = index.as_bytes().to_vec();
        content.extend_from_slice(body);
        let n = index.split_whitespace().count() / 2;
        stream_obj(
            &format!(
                "/Type /ObjStm /N {n} /First {} /Filter /FlateDecode",
                index.len()
            ),
            &zlib(&content),
        )
    }

    /// A readable page, and an object stream that decodes to exactly `bytes`.
    pub(crate) fn with_object_stream(bytes: usize) -> Vec<u8> {
        let (index, object) = ("100 0 ", " (Ten Forward)");
        let padding = bytes - index.len() - object.len();
        let body = [object.as_bytes(), &b" ".repeat(padding)].concat();
        pdf(&[object_stream(index, &body)])
    }

    fn refusal(bytes: &[u8], limits: &Limits) -> String {
        format!("{:#}", check_within(bytes, limits).unwrap_err())
    }

    const STREAM_OF_1024: Limits = Limits {
        stream_bytes: 1024,
        ..LIMITS
    };

    #[test]
    fn a_stream_at_the_limit_is_read_and_one_past_it_is_not() {
        let flate = "/Filter /FlateDecode";
        let at = pdf(&[stream_obj(flate, &zlib(&[b' '; 1024]))]);
        assert_eq!(
            check_within(&at, &STREAM_OF_1024).unwrap(),
            Checked::default()
        );

        let past = pdf(&[stream_obj(flate, &zlib(&[b' '; 1025]))]);
        let refused = refusal(&past, &STREAM_OF_1024);
        assert!(refused.contains("object 6 0"), "{refused}");
        assert!(refused.contains("more than 1024 bytes"), "{refused}");
    }

    /// Two layers ask for a thousand times a thousand from the same bytes.
    #[test]
    fn a_stream_behind_a_second_filter_is_measured_too() {
        let twice = zlib(&zlib(&[b' '; 4096]));
        let two_layers = pdf(&[stream_obj("/Filter [/FlateDecode /FlateDecode]", &twice)]);
        assert!(refusal(&two_layers, &STREAM_OF_1024).contains("more than 1024 bytes"));

        let hex: String = zlib(&[b' '; 4096])
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        let hex_first = pdf(&[stream_obj(
            "/Filter [/ASCIIHexDecode /FlateDecode]",
            format!("{hex}>").as_bytes(),
        )]);
        assert!(refusal(&hex_first, &STREAM_OF_1024).contains("more than 1024 bytes"));
    }

    /// What is encrypted with no password reads as noise until decrypted, and
    /// `pdf-inspector` decrypts it, so measuring the noise would measure nothing.
    #[test]
    fn a_stream_in_a_document_with_no_password_is_measured_as_it_decrypts() {
        let plain = pdf(&[stream_obj("/Filter /FlateDecode", &zlib(&[b' '; 4096]))]);
        let mut doc = Document::load_mem(&plain).unwrap();
        let id = Object::string_literal("NCC-1701-D");
        doc.trailer.set("ID", vec![id.clone(), id]);
        let state = lopdf::EncryptionState::try_from(lopdf::EncryptionVersion::V2 {
            document: &doc,
            owner_password: "Picard-4-7-Alpha-Tango",
            user_password: "",
            key_length: 128,
            permissions: lopdf::Permissions::default(),
        })
        .unwrap();
        doc.encrypt(&state).unwrap();
        let mut encrypted = Vec::new();
        doc.save_to(&mut encrypted).unwrap();

        pdf_inspector::process_pdf_mem_with_options(
            &encrypted,
            pdf_inspector::PdfOptions::detect_only(),
        )
        .expect("the reader this guards opens it");
        assert!(refusal(&encrypted, &STREAM_OF_1024).contains("more than 1024 bytes"));
    }

    #[test]
    fn streams_together_have_a_limit_of_their_own() {
        let limits = Limits {
            document_bytes: 4096,
            ..LIMITS
        };
        let each = stream_obj("/Filter /FlateDecode", &zlib(&[b' '; 3000]));
        assert!(check_within(&pdf(std::slice::from_ref(&each)), &limits).is_ok());
        let refused = refusal(&pdf(&[each.clone(), each]), &limits);
        assert!(refused.contains("together"), "{refused}");
    }

    /// Two kilobytes of file, a byte limit satisfied, and a loader that holds
    /// the array once for each entry.
    #[test]
    fn entries_sharing_an_offset_are_refused() {
        let index = "100 0 101 0 102 0 ";
        let refused = refusal(&pdf(&[object_stream(index, b"[null null]")]), &LIMITS);
        assert!(refused.contains("object stream 6 0"), "{refused}");
        assert!(refused.contains("overlap"), "{refused}");
    }

    #[test]
    fn an_entry_starting_inside_the_one_before_is_refused() {
        let index = "100 0 101 1 ";
        let refused = refusal(&pdf(&[object_stream(index, b"[[null null]]")]), &LIMITS);
        assert!(refused.contains("overlap"), "{refused}");
    }

    #[test]
    fn objects_are_counted_inside_arrays_and_dictionaries() {
        let limits = Limits {
            objects: 6,
            ..LIMITS
        };
        // The array and its three, then the dictionary and its one.
        let six = object_stream("100 0 101 8 ", b"[0 0 0]  << /A 0 >>");
        assert!(check_within(&pdf(std::slice::from_ref(&six)), &limits).is_ok());
        let refused = refusal(&pdf(&[six.clone(), six]), &limits);
        assert!(refused.contains("more than 6 objects"), "{refused}");
    }

    /// `/S Code` for `/S /Code` is a real writer's bug: the loaders skip that
    /// object and read the rest, so it is no reason to refuse the file.
    #[test]
    fn a_malformed_object_is_not_taken_for_an_overlap() {
        let index = "100 0 101 15 ";
        let body = b"<< /S Code >>  (Ten Forward)";
        assert!(check_within(&pdf(&[object_stream(index, body)]), &LIMITS).is_ok());
    }

    /// The specification wants offsets in increasing order; a file that has
    /// them otherwise overlaps nothing, and the loaders read it.
    #[test]
    fn entries_out_of_order_are_not_taken_for_an_overlap() {
        let index = "100 7 101 0 ";
        let body = b"(Worf) (Troi)";
        assert!(check_within(&pdf(&[object_stream(index, body)]), &LIMITS).is_ok());
    }

    /// Nothing inflates a JPEG, whatever it decodes to, and no limit applies.
    #[test]
    fn a_stream_with_a_filter_nobody_decodes_costs_nothing() {
        let jpeg = stream_obj("/Filter /DCTDecode", &[0xff; 2048]);
        assert!(check_within(&pdf(&[jpeg]), &STREAM_OF_1024).is_ok());
    }

    fn form_with_a_numeral_no_parser_holds(inflates_to: usize) -> Vec<u8> {
        let far = "9".repeat(308);
        pdf(&[stream_obj(
            &format!(
                "/Type /XObject /Subtype /Form /BBox [-{far} -{far} {far} {far}] \
                 /Filter [/FlateDecode]"
            ),
            &zlib(&vec![b' '; inflates_to]),
        )])
    }

    /// A real re-save writes `/BBox` this way, and `pdf-inspector` repairs it,
    /// so the object the first load drops is still measured, and is no reason
    /// to refuse the file when it is an ordinary size.
    #[test]
    fn an_object_the_loader_drops_is_measured_from_its_bytes() {
        let dropped = form_with_a_numeral_no_parser_holds(512);
        assert!(
            !Document::load_mem(&dropped)
                .unwrap()
                .objects
                .contains_key(&(6, 0)),
            "the premise: a parser drops this object"
        );
        assert_eq!(
            check_within(&dropped, &STREAM_OF_1024).unwrap(),
            Checked::default()
        );

        let refused = refusal(&form_with_a_numeral_no_parser_holds(4096), &STREAM_OF_1024);
        assert!(refused.contains("object 6 0"), "{refused}");
        assert!(refused.contains("more than 1024 bytes"), "{refused}");
    }

    #[test]
    fn filters_are_read_from_a_dictionary_no_parser_will_read() {
        let named = |dict: &[u8]| -> Vec<Vec<u8>> {
            filters_written_in(dict)
                .into_iter()
                .map(|name| name.as_name().unwrap().to_vec())
                .collect()
        };
        let flate = || b"FlateDecode".to_vec();
        assert_eq!(named(b"<< /Filter /FlateDecode /Length 9 >>"), [flate()]);
        assert_eq!(named(b"<< /Filter/FlateDecode/Length 9 >>"), [flate()]);
        assert_eq!(
            named(b"<< /Filter [ /ASCIIHexDecode /FlateDecode ] >>"),
            [b"ASCIIHexDecode".to_vec(), flate()]
        );
        assert!(named(b"<< /Length 9 >>").is_empty());
    }

    /// A reader that meets a bad zlib header tries the bytes as bare deflate,
    /// so a bomb can sit behind one.
    #[test]
    fn a_stream_behind_a_broken_zlib_header_is_measured_too() {
        let mut broken = zlib(&[b' '; 4096]);
        broken[0] = 0;
        let refused = refusal(
            &pdf(&[stream_obj("/Filter /FlateDecode", &broken)]),
            &STREAM_OF_1024,
        );
        assert!(refused.contains("more than 1024 bytes"), "{refused}");
    }

    /// A reader keeps what came out before an error, so that is what counts.
    #[test]
    fn a_stream_cut_short_is_measured_by_what_it_yields() {
        let whole = zlib(&[b' '; 1 << 20]);
        let cut = &whole[..whole.len() / 2];
        let refused = refusal(
            &pdf(&[stream_obj("/Filter /FlateDecode", cut)]),
            &STREAM_OF_1024,
        );
        assert!(refused.contains("more than 1024 bytes"), "{refused}");
    }

    #[test]
    fn a_file_that_does_not_open_is_left_for_the_parser_to_refuse() {
        assert_eq!(
            check_within(b"not a pdf", &STREAM_OF_1024).unwrap(),
            Checked::default()
        );
    }

    /// `pdf-inspector` loads with a bound it does not export. If it moved,
    /// objects would reach it that nothing here counted: this fails first.
    #[test]
    fn the_bound_on_object_streams_is_the_one_pdf_inspector_loads_with() {
        let holding_the_page_tree = |bytes: usize| {
            let index = "100 0 101 48 ";
            let pages = format!("{:<48}", "<< /Type /Pages /Kids [101 0 R] /Count 1 >>");
            let page = "<< /Type /Page /Parent 100 0 R /MediaBox [0 0 612 792] >>";
            let body = [pages.as_bytes(), page.as_bytes()].concat();
            let padding = bytes - index.len() - body.len();
            let body = [body.as_slice(), &b" ".repeat(padding)].concat();
            build(
                &[
                    b"<< /Type /Catalog /Pages 100 0 R >>".to_vec(),
                    object_stream(index, &body),
                ],
                "",
            )
        };
        let classify = |pdf: &[u8]| {
            pdf_inspector::process_pdf_mem_with_options(
                pdf,
                pdf_inspector::PdfOptions::detect_only(),
            )
        };

        let at = holding_the_page_tree(EXPANDED_OBJECT_STREAM_BYTES);
        assert_eq!(check(&at).unwrap(), Checked::default());
        assert_eq!(classify(&at).unwrap().page_count, 1);

        let past = holding_the_page_tree(EXPANDED_OBJECT_STREAM_BYTES + 1);
        assert!(check(&past).unwrap().left_out_object_streams);
        assert!(classify(&past).is_err(), "it lost the page tree");
    }
}
