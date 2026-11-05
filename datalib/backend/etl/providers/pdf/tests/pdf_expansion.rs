//! What a crafted PDF costs, in the bytes the process allocates, through the
//! calls a sync makes. Every PDF here is a few kilobytes or a few megabytes.

use std::alloc::{GlobalAlloc, Layout, System};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};

use datalib_etl_files::fingerprint_cache::FingerprintCache;
use datalib_etl_pdf::ingest::{self, RawDb};
use datalib_etl_pdf_render::render::convert;
use flate2::{Compress, Compression, FlushCompress};

struct CountingAllocator;

static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

fn grew(by: usize) {
    let live = LIVE.fetch_add(by, Ordering::Relaxed) + by;
    PEAK.fetch_max(live, Ordering::Relaxed);
}

// SAFETY: every call goes straight to `System`; the counters allocate nothing.
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        grew(layout.size());
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        grew(new_size);
        LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

fn start_measuring() -> usize {
    let live = LIVE.load(Ordering::Relaxed);
    PEAK.store(live, Ordering::Relaxed);
    live
}

fn held_since(start: usize) -> usize {
    PEAK.load(Ordering::Relaxed) - start
}

const MIB: usize = 1 << 20;

fn stream(dict: &str, data: &[u8]) -> Vec<u8> {
    let mut out = format!("<< /Length {} {dict} >>\nstream\n", data.len()).into_bytes();
    out.extend_from_slice(data);
    out.extend_from_slice(b"\nendstream");
    out
}

fn pdf(catalog: &str, extra: &[Vec<u8>]) -> Vec<u8> {
    pdf_with("", "", catalog, extra)
}

/// One page of text, with `catalog` in its catalog and `extra` as objects 6
/// and on. Assembled by hand, because no library writes the files under test.
fn pdf_with(resources: &str, draws: &str, catalog: &str, extra: &[Vec<u8>]) -> Vec<u8> {
    let page_text = format!("BT /F1 12 Tf 72 720 Td (Warp core nominal, Captain.) Tj ET {draws}");
    let mut objects = vec![
        format!("<< /Type /Catalog /Pages 2 0 R {catalog} >>").into_bytes(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        format!(
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Contents 4 0 R \
             /Resources << /Font << /F1 5 0 R >> {resources} >> >>"
        )
        .into_bytes(),
        stream("", page_text.as_bytes()),
        b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_vec(),
    ];
    objects.extend_from_slice(extra);

    let mut out = b"%PDF-1.7\n".to_vec();
    let mut offsets = Vec::new();
    for (i, body) in objects.iter().enumerate() {
        offsets.push(out.len());
        out.extend_from_slice(format!("{} 0 obj\n", i + 1).as_bytes());
        out.extend_from_slice(body);
        out.extend_from_slice(b"\nendobj\n");
    }
    let xref_at = out.len();
    let n = objects.len() + 1;
    out.extend_from_slice(format!("xref\n0 {n}\n0000000000 65535 f \n").as_bytes());
    for offset in offsets {
        out.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
    }
    out.extend_from_slice(
        format!("trailer\n<< /Size {n} /Root 1 0 R >>\nstartxref\n{xref_at}\n%%EOF\n").as_bytes(),
    );
    out
}

fn deflate(data: &[u8], flush: FlushCompress) -> Vec<u8> {
    let mut compress = Compress::new(Compression::best(), false);
    let mut out = Vec::with_capacity(data.len() / 100 + 64);
    compress
        .compress_vec(data, &mut out, flush)
        .expect("deflate");
    out
}

/// Zlib that inflates to `mib` MiB of spaces. A deflate block ended by a sync
/// flush can be repeated, so this takes no time to build, and it ends with no
/// checksum, which is no obstacle to a reader that has its output by then.
fn inflates_to_spaces(mib: usize) -> Vec<u8> {
    let block = deflate(&vec![b' '; MIB], FlushCompress::Sync);
    let mut out = vec![0x78, 0x9c];
    for _ in 0..mib {
        out.extend_from_slice(&block);
    }
    out
}

fn zlib(data: &[u8]) -> Vec<u8> {
    use std::io::Write;
    let mut encoder = flate2::write::ZlibEncoder::new(Vec::new(), Compression::best());
    encoder.write_all(data).unwrap();
    encoder.finish().unwrap()
}

/// `entries` objects, every one read from offset 0.
fn entries_at_one_offset(entries: usize) -> Vec<u8> {
    let index: String = (0..entries).map(|i| format!("{} 0 ", 100 + i)).collect();
    let mut content = index.clone().into_bytes();
    content.push(b'[');
    content.extend(b"null ".repeat(MIB / 5));
    content.push(b']');
    pdf(
        "",
        &[stream(
            &format!(
                "/Type /ObjStm /N {entries} /First {} /Filter /FlateDecode",
                index.len()
            ),
            &zlib(&content),
        )],
    )
}

/// `depth` objects, each starting one bracket further into the same array.
fn entries_nested_into_one_another(depth: usize) -> Vec<u8> {
    let index: String = (0..depth).map(|i| format!("{} {i} ", 100 + i)).collect();
    let mut content = index.clone().into_bytes();
    content.extend(b"[".repeat(depth));
    content.extend(b"0 ".repeat(MIB / 2));
    content.extend(b"]".repeat(depth));
    pdf(
        "",
        &[stream(
            &format!(
                "/Type /ObjStm /N {depth} /First {} /Filter /FlateDecode",
                index.len()
            ),
            &zlib(&content),
        )],
    )
}

/// Two layers, which only `lopdf` can decode, so the first is held to measure.
/// A form, because `pdf-inspector` inflates one and only the check bounds it.
fn form_of_spaces_twice(mib: usize) -> Vec<u8> {
    pdf_with(
        "/XObject << /X1 6 0 R >>",
        "/X1 Do",
        "",
        &[stream(
            "/Type /XObject /Subtype /Form /BBox [0 0 612 792] \
             /Filter [/FlateDecode /FlateDecode]",
            &zlib(&inflates_to_spaces(mib)),
        )],
    )
}

/// A numeral no parser can hold costs the whole object at the first load, and
/// `pdf-inspector` repairs the bytes and loads again, so it is read after all.
fn form_the_loader_drops(mib: usize) -> Vec<u8> {
    let far = "9".repeat(308);
    pdf_with(
        "/XObject << /X1 6 0 R >>",
        "/X1 Do",
        "",
        &[stream(
            &format!(
                "/Type /XObject /Subtype /Form /BBox [-{far} -{far} {far} {far}] \
                 /Filter /FlateDecode"
            ),
            &inflates_to_spaces(mib),
        )],
    )
}

fn metadata_of_spaces(mib: usize) -> Vec<u8> {
    pdf(
        "/Metadata 6 0 R",
        &[stream(
            "/Type /Metadata /Subtype /XML /Filter /FlateDecode",
            &inflates_to_spaces(mib),
        )],
    )
}

/// Each with the most reading it may hold, counting what a `Vec` reserves: it
/// doubles on its way to the 1 GiB a stream may reach, and two layers of Flate
/// are the case that has one to fill. A byte limit alone would let the first
/// two through, since each inflates to about 1 MiB.
fn crafted() -> [(&'static str, Vec<u8>, usize); 5] {
    [
        ("one_offset.pdf", entries_at_one_offset(100), 128 * MIB),
        ("nested.pdf", entries_nested_into_one_another(90), 128 * MIB),
        ("metadata.pdf", metadata_of_spaces(3072), 64 * MIB),
        ("twice.pdf", form_of_spaces_twice(3072), 2600 * MIB),
        ("dropped.pdf", form_the_loader_drops(3072), 64 * MIB),
    ]
}

async fn scan(root: &Path, raw: &Path) -> Vec<(String, String)> {
    let db = RawDb::open(&ingest::db_path_for(raw)).await.unwrap();
    let cache = FingerprintCache::open(&raw.join("fingerprints.sqlite"))
        .await
        .unwrap();
    ingest::fetch(ingest::FetchOptions {
        db: db.clone(),
        source_id: "tng_pdf".to_string(),
        root: root.to_path_buf(),
        ignore: vec![],
        cache: cache.clone(),
        max_bytes: None,
        now: "2364-04-13T08:45:00-07:00".to_string(),
        progress: datalib_etl::progress::Progress::noop(),
    })
    .await
    .unwrap();
    let problems = sqlx::query_as("SELECT scope_key, sample FROM problems")
        .fetch_all(db.pool())
        .await
        .unwrap();
    cache.pool().close().await;
    db.close().await;
    problems
}

/// A small PDF could make the scan or the render hold gigabytes: its streams
/// inflated without limit, or its object stream's index had a parser read one
/// array hundreds of times. Now each is refused for what it would cost, at a
/// cost that does not grow with what it asks for, and a person sees why.
#[tokio::test]
async fn a_crafted_pdf_is_refused_before_it_costs_what_it_asks_for() {
    let dir = tempfile::tempdir().unwrap();
    let (root, raw) = (dir.path().join("logs"), dir.path().join("raw"));
    std::fs::create_dir_all(&raw).unwrap();
    std::fs::create_dir_all(&root).unwrap();

    let ordinary = pdf("", &[]);
    std::fs::write(root.join("bridge.pdf"), &ordinary).unwrap();
    assert!(scan(&root, &raw).await.is_empty(), "nothing to refuse yet");
    let pages = convert::convert(&ordinary, Path::new("bridge.pdf")).unwrap();
    assert!(
        pages[0].text.contains("Warp core nominal"),
        "{}",
        pages[0].text
    );

    for (name, bytes, most) in crafted() {
        std::fs::write(root.join(name), &bytes).unwrap();

        let start = start_measuring();
        let problems = scan(&root, &raw).await;
        let peak = held_since(start);
        assert!(
            peak <= most,
            "{name}: the scan held {} MiB; it may hold {} MiB",
            peak / MIB,
            most / MIB
        );
        let keys: Vec<&str> = problems.iter().map(|(key, _)| key.as_str()).collect();
        assert_eq!(keys, [format!("record:pdf_paths:{name}")], "{name}");
        assert!(
            problems[0].1.contains("so the file was not read"),
            "{name}: {}",
            problems[0].1
        );

        let start = start_measuring();
        let converted = convert::convert(&bytes, Path::new(name));
        let peak = held_since(start);
        assert!(
            peak <= most,
            "{name}: converting held {} MiB; it may hold {} MiB",
            peak / MIB,
            most / MIB
        );
        let error = converted
            .err()
            .map(|e| format!("{e:#}"))
            .unwrap_or_default();
        assert!(
            error.contains("so the file was not read"),
            "{name}: {error}"
        );

        std::fs::remove_file(root.join(name)).unwrap();
    }
}
