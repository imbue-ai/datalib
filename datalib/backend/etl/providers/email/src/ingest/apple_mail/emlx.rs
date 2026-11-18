//! One `.emlx` file, and putting a `.partial.emlx` back together with the
//! attachments Mail keeps beside it. Pure: bytes in, bytes out. The file
//! format: `docs/dev/email_download_modes.md` §6.

use anyhow::{anyhow, bail, Result};
use base64::Engine as _;
use mail_parser::{Encoding, MessagePart};

use super::super::envelope;

/// The message inside an `.emlx`.
pub fn message(emlx: &[u8]) -> Result<&[u8]> {
    let line_end = emlx
        .iter()
        .position(|&b| b == b'\n')
        .ok_or_else(|| anyhow!("no length line"))?;
    let length: usize = std::str::from_utf8(&emlx[..line_end])
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .ok_or_else(|| anyhow!("the first line is not a length"))?;
    let start = line_end + 1;
    emlx.get(start..start + length).ok_or_else(|| {
        anyhow!(
            "the length line says {length} bytes, and only {} follow it",
            emlx.len() - start
        )
    })
}

/// A message with every attachment Mail had put back in place.
#[derive(Debug, Default)]
pub struct Rebuilt {
    pub raw: Vec<u8>,
    /// Parts Mail left out and holds no file for, by IMAP part number.
    pub missing: Vec<String>,
}

/// Put each part Mail left out of `message` back, from `attachment`, which
/// returns the decoded bytes Mail keeps for a part number. A message with
/// no such part comes back as it was.
pub fn rebuild(message: &[u8], attachment: impl Fn(&str) -> Option<Vec<u8>>) -> Result<Rebuilt> {
    let parsed = envelope::parse(message)?;
    let mut left_out = Vec::new();
    collect_left_out(parsed.root_part(), String::new(), &mut left_out);
    if left_out.is_empty() {
        return Ok(Rebuilt {
            raw: message.to_vec(),
            missing: Vec::new(),
        });
    }
    let crlf = message.windows(2).any(|w| w == b"\r\n");
    let mut out = Vec::with_capacity(message.len());
    let mut missing = Vec::new();
    let mut at = 0usize;
    for part in left_out {
        let Some(bytes) = attachment(&part.number) else {
            missing.push(part.number);
            continue;
        };
        if part.body.0 < at || part.body.1 > message.len() {
            bail!("part {} lies outside the message", part.number);
        }
        out.extend_from_slice(&message[at..part.body.0]);
        out.extend(encode(&bytes, part.encoding, crlf));
        at = part.body.1;
    }
    out.extend_from_slice(&message[at..]);
    Ok(Rebuilt { raw: out, missing })
}

struct LeftOut {
    number: String,
    body: (usize, usize),
    encoding: Encoding,
}

/// The parts Mail left out, in the order they appear. A nested message is
/// one part here: its own parts are numbered within it, and Mail keeps
/// such a message whole.
fn collect_left_out(part: MessagePart<'_>, number: String, out: &mut Vec<LeftOut>) {
    if part.is_multipart() {
        for (i, child) in part.children().enumerate() {
            let child_number = if number.is_empty() {
                (i + 1).to_string()
            } else {
                format!("{number}.{}", i + 1)
            };
            collect_left_out(child, child_number, out);
        }
        return;
    }
    let was_left_out = part.headers().value("X-Apple-Content-Length").is_some()
        && part.raw_body().iter().all(u8::is_ascii_whitespace);
    if was_left_out {
        out.push(LeftOut {
            number: if number.is_empty() {
                "1".to_string()
            } else {
                number
            },
            body: (part.offset_body() as usize, part.offset_end() as usize),
            encoding: part.encoding(),
        });
    }
}

/// `bytes` in the part's transfer encoding, with lines broken where that
/// encoding wants them, ending without a line break: the one before the
/// next delimiter is already in the message.
fn encode(bytes: &[u8], encoding: Encoding, crlf: bool) -> Vec<u8> {
    let eol: &[u8] = if crlf { b"\r\n" } else { b"\n" };
    match encoding {
        Encoding::Base64 => {
            let text = base64::engine::general_purpose::STANDARD.encode(bytes);
            let mut out = Vec::with_capacity(text.len() + text.len() / 38);
            for (i, line) in text.as_bytes().chunks(76).enumerate() {
                if i > 0 {
                    out.extend_from_slice(eol);
                }
                out.extend_from_slice(line);
            }
            out
        }
        Encoding::QuotedPrintable => quoted_printable(bytes, eol),
        Encoding::None => bytes.to_vec(),
    }
}

/// RFC 2045 §6.7: every byte outside printable ASCII, and `=`, as `=XX`;
/// line breaks kept; lines kept under 76 with soft breaks.
fn quoted_printable(bytes: &[u8], eol: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len() + bytes.len() / 8);
    let mut line_len = 0usize;
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'\n' || (b == b'\r' && bytes.get(i + 1) == Some(&b'\n')) {
            out.extend_from_slice(eol);
            line_len = 0;
            i += if b == b'\r' { 2 } else { 1 };
            continue;
        }
        let next_is_break = matches!(bytes.get(i + 1), None | Some(b'\n') | Some(b'\r'));
        let literal = matches!(b, b'!'..=b'<' | b'>'..=b'~') || (b == b' ' && !next_is_break);
        let piece_len = if literal { 1 } else { 3 };
        if line_len + piece_len > 75 {
            out.push(b'=');
            out.extend_from_slice(eol);
            line_len = 0;
        }
        if literal {
            out.push(b);
        } else {
            out.extend_from_slice(format!("={b:02X}").as_bytes());
        }
        line_len += piece_len;
        i += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const PARTIAL: &str = "From: Jean-Luc Picard <picard@enterprise.example>\n\
To: Data <data@enterprise.example>\n\
Subject: Duty roster\n\
Message-ID: <roster-1@enterprise.example>\n\
MIME-Version: 1.0\n\
Content-Type: multipart/mixed; boundary=outer\n\
\n\
--outer\n\
Content-Type: multipart/alternative; boundary=inner\n\
\n\
--inner\n\
Content-Type: text/plain; charset=utf-8\n\
\n\
The roster is attached.\n\
--inner\n\
Content-Type: text/calendar; charset=utf-8\n\
Content-Transfer-Encoding: quoted-printable\n\
X-Apple-Content-Length: 30\n\
\n\
\n\
--inner--\n\
\n\
--outer\n\
Content-Type: application/pdf; name=roster.pdf\n\
Content-Disposition: attachment; filename=roster.pdf\n\
Content-Transfer-Encoding: base64\n\
X-Apple-Content-Length: 120\n\
\n\
\n\
--outer--\n";

    fn emlx(message: &str) -> Vec<u8> {
        let mut out = format!("{:<10}\n", message.len()).into_bytes();
        out.extend_from_slice(message.as_bytes());
        out.extend_from_slice(b"<?xml version=\"1.0\"?>\n<plist version=\"1.0\"><dict/></plist>\n");
        out
    }

    #[test]
    fn the_message_is_the_length_the_first_line_says() {
        let file = emlx("Subject: hi\n\nbody\n");
        assert_eq!(message(&file).unwrap(), b"Subject: hi\n\nbody\n");
    }

    /// A file cut short must not pass as a shorter message.
    #[test]
    fn a_file_shorter_than_its_length_line_is_refused() {
        let mut file = emlx("Subject: hi\n\nbody\n");
        file.truncate(15);
        assert!(message(&file).is_err());
    }

    #[test]
    fn a_whole_message_comes_back_as_it_was() {
        let raw = b"Subject: hi\nMessage-ID: <a@b>\n\nbody\n";
        let rebuilt = rebuild(raw, |_| panic!("asked for a part")).unwrap();
        assert_eq!(rebuilt.raw, raw);
        assert!(rebuilt.missing.is_empty());
    }

    /// Each left-out part goes back in under its IMAP part number, and the
    /// result decodes to the bytes Mail kept.
    #[test]
    fn left_out_parts_are_put_back_by_part_number() {
        let pdf: Vec<u8> = (0..=255u8).cycle().take(120).collect();
        let calendar = b"BEGIN:VCALENDAR\nX-A=1\nEND:VCALENDAR\n".to_vec();
        let rebuilt = rebuild(PARTIAL.as_bytes(), |part| match part {
            "1.2" => Some(calendar.clone()),
            "2" => Some(pdf.clone()),
            other => panic!("asked for part {other}"),
        })
        .unwrap();
        assert!(rebuilt.missing.is_empty());

        let msg = envelope::parse(&rebuilt.raw).unwrap();
        let attachment = msg
            .parts()
            .find(|p| p.attachment_name() == Some("roster.pdf"))
            .expect("the pdf is a part");
        assert_eq!(attachment.decoded().as_ref(), pdf.as_slice());
        let ics = msg
            .parts()
            .find(|p| p.is_content_type("text", "calendar"))
            .expect("the calendar is a part");
        assert_eq!(ics.decoded().as_ref(), calendar.as_slice());
        let text = msg
            .parts()
            .find(|p| p.is_content_type("text", "plain"))
            .unwrap();
        assert_eq!(text.text().unwrap().trim(), "The roster is attached.");
    }

    #[test]
    fn a_part_with_no_file_is_named_and_left_empty() {
        let rebuilt = rebuild(PARTIAL.as_bytes(), |part| {
            (part == "2").then(|| b"%PDF".to_vec())
        })
        .unwrap();
        assert_eq!(rebuilt.missing, ["1.2"]);
        envelope::parse(&rebuilt.raw).unwrap();
    }

    #[test]
    fn quoted_printable_round_trips() {
        let text = "caf\u{e9} = bar, a line that runs on and on past the seventy-six columns a line may have \n".repeat(3);
        let encoded = quoted_printable(text.as_bytes(), b"\n");
        assert!(encoded.split(|&b| b == b'\n').all(|l| l.len() <= 76));
        let message = [
            b"Content-Type: text/plain; charset=utf-8\nContent-Transfer-Encoding: quoted-printable\n\n"
                .as_slice(),
            &encoded,
        ]
        .concat();
        let msg = envelope::parse(&message).unwrap();
        assert_eq!(msg.root_part().decoded().as_ref(), text.as_bytes());
    }
}
