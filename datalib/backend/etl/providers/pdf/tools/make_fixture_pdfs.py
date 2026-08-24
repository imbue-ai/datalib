#!/usr/bin/env python3
"""Generate the `pdf` provider's fixture corpus.

Hand-built rather than produced by a PDF library, for three reasons:
the files stay small enough to review, the bytes are byte-for-byte
deterministic (so the fixture's blake3s — which are the provider's
primary keys — never drift), and we can place trailer `/ID` and XMP
packets exactly where the identity tests need them.

Regenerate with:

    uv run python datalib/backend/etl/providers/pdf/tools/make_fixture_pdfs.py

TNG-themed, per the repo's fixture convention.
"""

from __future__ import annotations

import pathlib
import sys

OUT = pathlib.Path(__file__).resolve().parent.parent / "tests" / "fixtures" / "pdf_tng"


def build(objects: list[bytes], trailer_extra: bytes = b"") -> bytes:
    """Assemble numbered objects into a PDF with a correct xref table.

    `objects` is 1-indexed by position; each entry is the body between
    `N 0 obj` and `endobj`.
    """
    out = bytearray(b"%PDF-1.7\n%\xe2\xe3\xcf\xd3\n")
    offsets = [0]
    for i, body in enumerate(objects, start=1):
        offsets.append(len(out))
        out += f"{i} 0 obj\n".encode()
        out += body
        out += b"\nendobj\n"

    xref_at = len(out)
    n = len(objects) + 1
    out += f"xref\n0 {n}\n".encode()
    out += b"0000000000 65535 f \n"
    for off in offsets[1:]:
        out += f"{off:010d} 00000 n \n".encode()
    out += b"trailer\n<< /Size " + str(n).encode() + b" /Root 1 0 R" + trailer_extra + b" >>\n"
    out += b"startxref\n" + str(xref_at).encode() + b"\n%%EOF\n"
    return bytes(out)


def text_stream(lines: list[str], y_start: int = 720) -> bytes:
    """A content stream that draws `lines` with real Tj operators."""
    parts = ["BT", "/F1 12 Tf"]
    y = y_start
    for ln in lines:
        esc = ln.replace("\\", r"\\").replace("(", r"\(").replace(")", r"\)")
        parts.append(f"1 0 0 1 72 {y} Tm ({esc}) Tj")
        y -= 18
    parts.append("ET")
    return "\n".join(parts).encode("latin-1", "replace")


def stream_obj(data: bytes, extra: str = "") -> bytes:
    return f"<< /Length {len(data)}{extra} >>\nstream\n".encode() + data + b"\nendstream"


def simple_doc(
    pages: list[list[str]],
    *,
    title: str | None = None,
    created: str | None = None,
    doc_id: str | None = None,
    xmp_document_id: str | None = None,
    xmp_instance_id: str | None = None,
) -> bytes:
    """A text PDF with `len(pages)` pages and optional metadata."""
    objs: list[bytes] = []
    n_pages = len(pages)
    # 1 catalog, 2 pages tree, then per page: page obj + content obj.
    page_obj_ids = [3 + 2 * i for i in range(n_pages)]
    content_ids = [4 + 2 * i for i in range(n_pages)]
    font_id = 3 + 2 * n_pages
    info_id = font_id + 1
    xmp_id = info_id + 1

    catalog = b"<< /Type /Catalog /Pages 2 0 R"
    if xmp_document_id or xmp_instance_id:
        catalog += f" /Metadata {xmp_id} 0 R".encode()
    catalog += b" >>"
    objs.append(catalog)

    kids = " ".join(f"{p} 0 R" for p in page_obj_ids)
    objs.append(f"<< /Type /Pages /Count {n_pages} /Kids [{kids}] >>".encode())

    for i, lines in enumerate(pages):
        objs.append(
            (
                f"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] "
                f"/Resources << /Font << /F1 {font_id} 0 R >> >> "
                f"/Contents {content_ids[i]} 0 R >>"
            ).encode()
        )
        objs.append(stream_obj(text_stream(lines)))

    objs.append(b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>")

    info = b"<< "
    if title:
        info += b"/Title (" + title.encode("latin-1", "replace") + b") "
    if created:
        info += b"/CreationDate (" + created.encode() + b") "
    info += b">>"
    objs.append(info)

    if xmp_document_id or xmp_instance_id:
        fields = ""
        if xmp_document_id:
            fields += f"<xmpMM:DocumentID>{xmp_document_id}</xmpMM:DocumentID>"
        if xmp_instance_id:
            fields += f"<xmpMM:InstanceID>{xmp_instance_id}</xmpMM:InstanceID>"
        xmp = (
            '<?xpacket begin="" id="W5M0MpCehiHzreSzNTczkc9d"?>'
            '<x:xmpmeta xmlns:x="adobe:ns:meta/">'
            '<rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">'
            '<rdf:Description rdf:about="" '
            'xmlns:xmpMM="http://ns.adobe.com/xap/1.0/mm/">'
            f"{fields}"
            "</rdf:Description></rdf:RDF></x:xmpmeta><?xpacket end=\"w\"?>"
        ).encode()
        objs.append(stream_obj(xmp, " /Type /Metadata /Subtype /XML"))
    else:
        objs.append(b"<< >>")  # placeholder so numbering stays fixed

    trailer_extra = f" /Info {info_id} 0 R".encode()
    if doc_id:
        trailer_extra += f" /ID [<{doc_id}> <{doc_id}>]".encode()
    return build(objs, trailer_extra)


def scanned_doc() -> bytes:
    """A page whose only content is an image XObject — no text operators,
    which is exactly what the classifier keys on to say `Scanned`."""
    # 2x2 grayscale, uncompressed. Enough to be a real image XObject.
    img = bytes([0x00, 0xFF, 0xFF, 0x00])
    content = b"q 612 0 0 792 0 0 cm /Im0 Do Q"
    objs = [
        b"<< /Type /Catalog /Pages 2 0 R >>",
        b"<< /Type /Pages /Count 1 /Kids [3 0 R] >>",
        (
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] "
            b"/Resources << /XObject << /Im0 5 0 R >> >> /Contents 4 0 R >>"
        ),
        stream_obj(content),
        stream_obj(
            img,
            " /Type /XObject /Subtype /Image /Width 2 /Height 2 "
            "/ColorSpace /DeviceGray /BitsPerComponent 8",
        ),
    ]
    return build(objs)


FIXTURES: dict[str, bytes] = {}


def main() -> int:
    OUT.mkdir(parents=True, exist_ok=True)

    # A normal text document, fully identified: Info title + date,
    # trailer /ID, and an XMP DocumentID/InstanceID pair.
    captains_log = simple_doc(
        [
            [
                "Captain's Log, Stardate 41153.7",
                "Our destination is planet Deneb IV, beyond which",
                "lies the great unexplored mass of the galaxy.",
            ],
            [
                "Captain's Log, supplemental.",
                "The alien entity known as Q has confronted us.",
            ],
        ],
        title="Captain's Log",
        created="D:23640413084500-07'00'",
        doc_id="0123456789abcdef0123456789abcdef",
        xmp_document_id="uuid:enterprise-ncc-1701-d",
        xmp_instance_id="uuid:instance-0001",
    )
    FIXTURES["captains_log.pdf"] = captains_log

    # Same document, second location. Byte-identical, so it must collapse
    # to ONE `pdf_documents` row with two `pdf_paths` rows.
    FIXTURES["archive/captains_log_copy.pdf"] = captains_log

    # A later revision: different bytes (new page), but the SAME
    # xmpMM:DocumentID. This is the Ship-of-Theseus case — two content
    # identities linked by one lineage id.
    FIXTURES["captains_log_v2.pdf"] = simple_doc(
        [
            [
                "Captain's Log, Stardate 41153.7",
                "Our destination is planet Deneb IV, beyond which",
                "lies the great unexplored mass of the galaxy.",
            ],
            [
                "Captain's Log, supplemental.",
                "The alien entity known as Q has confronted us.",
            ],
            [
                "Captain's Log, Stardate 41986.0",
                "Addendum filed after review by Starfleet Command.",
            ],
        ],
        title="Captain's Log",
        created="D:23640413084500-07'00'",
        doc_id="0123456789abcdef0123456789abcdef",
        xmp_document_id="uuid:enterprise-ncc-1701-d",
        # A new save gets a new InstanceID; DocumentID is unchanged.
        xmp_instance_id="uuid:instance-0002",
    )

    # No Info dict, no /ID, no XMP. Every identity column must come back
    # NULL without the scan failing.
    FIXTURES["engineering/unlabeled_schematic.pdf"] = simple_doc(
        [["Warp core intermix ratio nominal.", "Dilithium chamber aligned."]]
    )

    # Image-only: classified `scanned`, recorded, and NOT converted.
    FIXTURES["holodeck/scanned_blueprint.pdf"] = scanned_doc()

    # Not a PDF despite the extension. Must be counted as an error and
    # skipped, not abort the scan.
    FIXTURES["holodeck/corrupt.pdf"] = b"%PDF-1.7\nthis file is truncated garbage\n"

    # A non-PDF file that the walk must ignore entirely.
    (OUT / "readme.txt").write_bytes(b"Not a PDF. The walker must skip this.\n")

    for rel, data in FIXTURES.items():
        p = OUT / rel
        p.parent.mkdir(parents=True, exist_ok=True)
        p.write_bytes(data)
        print(f"{rel:44s} {len(data):6d} bytes")
    return 0


if __name__ == "__main__":
    sys.exit(main())
