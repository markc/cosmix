//! Bounded MIME inspection and account-bound, hash-bound part identifiers.
//!
//! Download bytes come from transfer-decoding raw ranges, never from the
//! parser's charset-converted Text/Html values. Embedded messages have their
//! own raw buffer and therefore their own offset space.

use crate::mailstore::{MailStore, SqliteMailStore, parse_hash};
use cosmix_mds::{BlobHash, ItemId, Mds, blob};
use mail_parser::{Message, MessageParser, MessagePart, MimeHeaders, PartType};
use std::{fmt, io::Read};
use uuid::Uuid;

pub const MAX_MESSAGE: usize = 64 * 1024 * 1024;
pub const MAX_PART: usize = 64 * 1024 * 1024;
pub const MAX_DEPTH: usize = 32;
pub const MAX_PARTS: usize = 1000;
pub const MAX_PATH: usize = 64;

#[derive(Debug, PartialEq, Eq)]
pub enum Error {
    TooLarge(&'static str),
    Unreadable(String),
    NotFound,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooLarge(reason) => write!(f, "too_large: {reason}"),
            Self::Unreadable(reason) => write!(f, "unreadable: {reason}"),
            Self::NotFound => f.write_str("not_found: message or part"),
        }
    }
}
impl std::error::Error for Error {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PartBlobId {
    pub item: ItemId,
    pub message_hash: BlobHash,
    pub part: String,
}

pub(crate) fn valid_path(path: &str) -> bool {
    path.len() <= MAX_PATH
        && path.is_ascii()
        && path.split('.').count() <= MAX_DEPTH
        && path.split('.').next() == Some("1")
        && path.split('.').all(|n| {
            !n.starts_with('0')
                && !n.is_empty()
                && n.bytes().all(|b| b.is_ascii_digit())
                && n.parse::<usize>().is_ok_and(|n| n > 0 && n <= MAX_PARTS)
        })
}

impl PartBlobId {
    pub fn parse(value: &str) -> Option<Self> {
        if !value.is_ascii() || value.len() > 4 + 32 + 1 + 64 + 1 + MAX_PATH {
            return None;
        }
        let mut fields = value.strip_prefix("mp1_")?.splitn(3, '_');
        let uuid = fields.next()?;
        let hash = fields.next()?;
        let path = fields.next()?;
        let lower_hex = |s: &str| {
            s.bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        };
        if uuid.len() != 32
            || hash.len() != 64
            || !lower_hex(uuid)
            || !lower_hex(hash)
            || path.contains('.')
        {
            return None;
        }
        let part = path.replace('_', ".");
        if !valid_path(&part) {
            return None;
        }
        Some(Self {
            item: ItemId(Uuid::parse_str(uuid).ok()?),
            message_hash: parse_hash(hash)?,
            part,
        })
    }
}

impl fmt::Display for PartBlobId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "mp1_{}_{}_{}",
            self.item.0.simple(),
            blob::hex(&self.message_hash),
            self.part.replace('.', "_")
        )
    }
}

#[derive(Debug)]
pub struct Part {
    pub path: String,
    pub name: Option<String>,
    pub mime: String,
    pub size: usize,
    pub disposition: Option<String>,
    pub cid: Option<String>,
    pub attachment: bool,
    /// Embedded-message children are inspectable but are not outer body parts.
    pub embedded: bool,
    pub text: bool,
    pub html: bool,
    pub value: Option<String>,
}

#[derive(Debug)]
pub struct Inspection {
    pub parts: Vec<Part>,
    /// Only populated when a caller requests extraction of one path.
    pub extracted: Option<Vec<u8>>,
}

/// The caller must establish account ownership before using this low-level read.
pub fn read_message(ms: &SqliteMailStore, hash: &BlobHash) -> Result<Vec<u8>, Error> {
    let file = ms
        .mds()
        .blob_file(hash)
        .map_err(|e| Error::Unreadable(e.to_string()))?;
    if file
        .metadata()
        .map_err(|e| Error::Unreadable(e.to_string()))?
        .len()
        > MAX_MESSAGE as u64
    {
        return Err(Error::TooLarge("raw message exceeds 64 MiB"));
    }
    let mut data = Vec::new();
    file.take(MAX_MESSAGE as u64 + 1)
        .read_to_end(&mut data)
        .map_err(|e| Error::Unreadable(e.to_string()))?;
    if data.len() > MAX_MESSAGE {
        return Err(Error::TooLarge("raw message exceeds 64 MiB"));
    }
    if blake3::hash(&data).as_bytes() != &hash.0 {
        return Err(Error::Unreadable("message hash mismatch".into()));
    }
    Ok(data)
}

/// Resolve the account-owned item before inspecting any CAS bytes. A stale
/// identifier, foreign item and absent path have the same NotFound result.
pub fn download(
    ms: &SqliteMailStore,
    account: i32,
    id: &PartBlobId,
) -> Result<(Part, Vec<u8>), Error> {
    let record = ms.get_email(account, id.item).map_err(|e| {
        let message = e.to_string();
        if message.starts_with("email not found") || message.starts_with("email envelope missing") {
            Error::NotFound
        } else {
            Error::Unreadable(message)
        }
    })?;
    if record.blob_hash != id.message_hash {
        return Err(Error::NotFound);
    }
    let data = read_message(ms, &record.blob_hash)?;
    let result = inspect(&data, Some(&id.part), false)?;
    let part = result
        .parts
        .into_iter()
        .find(|p| p.path == id.part)
        .ok_or(Error::NotFound)?;
    Ok((part, result.extracted.ok_or(Error::NotFound)?))
}

fn decoded(message: &Message<'_>, part: &MessagePart<'_>) -> Result<Vec<u8>, Error> {
    // mail-parser also flags its three-level *encoded message* recursion
    // limit as an encoding problem. Decode and resume those messages below;
    // our own global depth/part limits remain authoritative.
    if part.is_encoding_problem && !part.is_content_type("message", "rfc822") {
        return Err(Error::Unreadable("invalid MIME transfer encoding".into()));
    }
    let raw = message
        .raw_message
        .get(part.offset_body as usize..part.offset_end as usize)
        .ok_or_else(|| Error::Unreadable("invalid MIME offsets".into()))?;
    let encoding = part.content_transfer_encoding().unwrap_or("7bit").trim();
    let data = if encoding.eq_ignore_ascii_case("base64") {
        mail_parser::decoders::base64::base64_decode(raw)
    } else if encoding.eq_ignore_ascii_case("quoted-printable") {
        mail_parser::decoders::quoted_printable::quoted_printable_decode(raw)
    } else if ["7bit", "8bit", "binary"]
        .iter()
        .any(|e| encoding.eq_ignore_ascii_case(e))
    {
        Some(raw.to_vec())
    } else {
        None
    }
    .ok_or_else(|| Error::Unreadable("invalid MIME transfer encoding".into()))?;
    if data.len() > MAX_PART {
        return Err(Error::TooLarge("decoded part exceeds 64 MiB"));
    }
    Ok(data)
}

/// One parse and one bounded walk for both metadata and optional extraction.
/// `body_values` permits charset-converted display text, never download bytes.
pub fn inspect(
    data: &[u8],
    selected: Option<&str>,
    body_values: bool,
) -> Result<Inspection, Error> {
    if data.len() > MAX_MESSAGE {
        return Err(Error::TooLarge("raw message exceeds 64 MiB"));
    }
    if selected.is_some_and(|p| !valid_path(p)) {
        return Err(Error::NotFound);
    }
    let message = MessageParser::default()
        .parse(data)
        .ok_or_else(|| Error::Unreadable("cannot parse message".into()))?;
    let mut walker = Walker {
        count: 0,
        selected,
        body_values,
        result: Inspection {
            parts: Vec::new(),
            extracted: None,
        },
    };
    walker.visit(&message, 0, "1".into(), false, false)?;
    if selected.is_some() && walker.result.extracted.is_none() {
        return Err(Error::NotFound);
    }
    Ok(walker.result)
}

struct Walker<'a> {
    count: usize,
    selected: Option<&'a str>,
    body_values: bool,
    result: Inspection,
}

impl Walker<'_> {
    fn visit(
        &mut self,
        message: &Message<'_>,
        index: u32,
        path: String,
        embedded: bool,
        attached_parent: bool,
    ) -> Result<(), Error> {
        self.count += 1;
        if self.count > MAX_PARTS || path.split('.').count() > MAX_DEPTH || path.len() > MAX_PATH {
            return Err(Error::TooLarge(
                "MIME structure limit (depth 32, parts 1000, path 64)",
            ));
        }
        let part = message
            .parts
            .get(index as usize)
            .ok_or_else(|| Error::Unreadable("invalid MIME child index".into()))?;
        let name = part.attachment_name().map(str::to_owned);
        let disposition = part
            .content_disposition()
            .map(|d| d.c_type.to_ascii_lowercase());
        let attachment = name.is_some() || disposition.as_deref() == Some("attachment");
        if !matches!(part.body, PartType::Multipart(_)) || attachment {
            let bytes = decoded(message, part)?;
            let text = !embedded
                && !attachment
                && !attached_parent
                && message.text_body.contains(&index)
                && matches!(part.body, PartType::Text(_));
            let html = !embedded
                && !attachment
                && !attached_parent
                && message.html_body.contains(&index)
                && matches!(part.body, PartType::Html(_));
            let value = if self.body_values && (text || html) {
                match &part.body {
                    PartType::Text(s) | PartType::Html(s) => Some(s.to_string()),
                    _ => None,
                }
            } else {
                None
            };
            let mime = part
                .content_type()
                .map(|ct| {
                    format!(
                        "{}/{}",
                        ct.c_type.to_ascii_lowercase(),
                        ct.c_subtype
                            .as_deref()
                            .unwrap_or("octet-stream")
                            .to_ascii_lowercase()
                    )
                })
                .unwrap_or_else(|| {
                    match &part.body {
                        PartType::Text(_) => "text/plain",
                        PartType::Html(_) => "text/html",
                        PartType::Message(_) => "message/rfc822",
                        _ => "application/octet-stream",
                    }
                    .into()
                });
            self.result.parts.push(Part {
                path: path.clone(),
                name,
                mime,
                size: bytes.len(),
                disposition,
                cid: part.content_id().map(str::to_owned),
                attachment,
                embedded: embedded || attached_parent,
                text,
                html,
                value,
            });
            if self.selected == Some(path.as_str()) {
                self.result.extracted = Some(bytes);
            }
        }
        match &part.body {
            PartType::Multipart(children) => {
                for (n, child) in children.iter().enumerate() {
                    self.visit(
                        message,
                        *child,
                        format!("{path}.{}", n + 1),
                        embedded,
                        attached_parent || attachment,
                    )?;
                }
            }
            PartType::Message(inner) => self.visit(
                inner,
                0,
                format!("{path}.1"),
                true,
                attached_parent || attachment,
            )?,
            _ if part.is_content_type("message", "rfc822") => {
                let bytes = decoded(message, part)?;
                let inner = MessageParser::default()
                    .parse(&bytes)
                    .ok_or_else(|| Error::Unreadable("cannot parse embedded message".into()))?;
                self.visit(
                    &inner,
                    0,
                    format!("{path}.1"),
                    true,
                    attached_parent || attachment,
                )?;
            }
            _ => (),
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identifiers_are_canonical_ascii_and_bounded() {
        let id = PartBlobId {
            item: ItemId(Uuid::nil()),
            message_hash: BlobHash([0xab; 32]),
            part: "1.2.3".into(),
        };
        assert_eq!(PartBlobId::parse(&id.to_string()), Some(id.clone()));
        for bad in [
            id.to_string().replace("ab", "AB"),
            id.to_string().replace("1_2_3", "1_02_3"),
            id.to_string().replace("1_2_3", "2_2_3"),
            id.to_string().replace("1_2_3", "1.2.3"),
            id.to_string().replace("ab", "é"),
            format!("{}__", id),
        ] {
            assert!(PartBlobId::parse(&bad).is_none(), "{bad}");
        }
        assert!(!valid_path(&format!("1{}", ".1".repeat(32))));
        assert!(!valid_path("1.1001"));
    }

    #[test]
    fn truncated_and_garbage_mime_never_panics_or_escapes_bounds() {
        for raw in [
            b"Content-Type: multipart/mixed; boundary=x\r\n\r\n--x\r\nContent-Transfer-Encoding: base64\r\n\r\nAP8=\r\n--x--".as_slice(),
            b"Content-Type: message/rfc822\r\nContent-Transfer-Encoding: base64\r\n\r\n!!!!\xff\x00",
            b"Content-Type: text/plain\r\nContent-Transfer-Encoding: quoted-printable\r\n\r\n=Z=\xff\x00",
        ] {
            for end in 0..=raw.len() {
                if let Ok(result) = inspect(&raw[..end], None, false) {
                    assert!(result.parts.len() <= MAX_PARTS);
                    for part in result.parts { assert!(valid_path(&part.path)); assert!(part.size <= MAX_PART); }
                }
            }
        }
    }

    #[test]
    fn transfer_octets_are_not_charset_converted() {
        let raw = b"Content-Type: text/plain; charset=iso-8859-1\r\nContent-Disposition: attachment; filename=cafe.txt\r\nContent-Transfer-Encoding: quoted-printable\r\n\r\ncaf=E9";
        let result = inspect(raw, Some("1"), true).unwrap();
        assert_eq!(result.extracted.unwrap(), b"caf\xe9");
        assert_eq!(result.parts[0].size, 4);
        assert!(result.parts[0].attachment);
        assert!(!result.parts[0].text);
    }

    #[test]
    fn embedded_message_offsets_use_their_own_buffer() {
        use base64::Engine;
        let inner = b"Content-Type: application/octet-stream\r\nContent-Transfer-Encoding: base64\r\n\r\nAP8B";
        let raw = format!(
            "Content-Type: message/rfc822\r\nContent-Disposition: attachment; filename=inner.eml\r\nContent-Transfer-Encoding: base64\r\n\r\n{}",
            base64::engine::general_purpose::STANDARD.encode(inner)
        );
        assert_eq!(
            inspect(raw.as_bytes(), Some("1"), false)
                .unwrap()
                .extracted
                .unwrap(),
            inner
        );
        let result = inspect(raw.as_bytes(), Some("1.1"), false).unwrap();
        assert_eq!(result.extracted.unwrap(), [0, 255, 1]);
        assert!(result.parts[1].embedded);
    }

    #[test]
    fn multipart_paths_body_and_inline_filename() {
        let raw = b"Content-Type: multipart/mixed; boundary=x\r\n\r\n--x\r\nContent-Type: text/plain\r\n\r\nhello\r\n--x\r\nContent-Type: application/octet-stream\r\nContent-Disposition: inline; filename*=utf-8''caf%C3%A9.bin\r\nContent-ID: <a>\r\nContent-Transfer-Encoding: base64\r\n\r\nAP8=\r\n--x--\r\n";
        let result = inspect(raw, Some("1.2"), true).unwrap();
        assert_eq!(result.parts[0].path, "1.1");
        assert!(result.parts[0].text);
        assert_eq!(result.parts[1].name.as_deref(), Some("café.bin"));
        assert!(result.parts[1].attachment);
        assert_eq!(result.extracted.unwrap(), [0, 255]);
        assert_eq!(
            inspect(raw, Some("1.3"), false).unwrap_err(),
            Error::NotFound
        );
    }

    #[test]
    fn structure_and_raw_limits_fail_closed() {
        let mut raw = String::from("Content-Type: multipart/mixed; boundary=x\r\n\r\n");
        for _ in 0..1000 {
            raw.push_str("--x\r\nContent-Type: text/plain\r\n\r\nx\r\n");
        }
        raw.push_str("--x--\r\n");
        assert!(matches!(
            inspect(raw.as_bytes(), None, false),
            Err(Error::TooLarge(_))
        ));
        assert!(matches!(
            inspect(&vec![b'x'; MAX_MESSAGE + 1], None, false),
            Err(Error::TooLarge(_))
        ));
        assert!(matches!(
            inspect(b"", None, false),
            Err(Error::Unreadable(_))
        ));
    }

    #[test]
    fn encoded_messages_resume_beyond_the_parsers_internal_limit() {
        use base64::Engine;
        let mut raw = String::from("Content-Type: text/plain\r\n\r\ndeep");
        for _ in 0..5 {
            raw = format!(
                "Content-Type: message/rfc822\r\nContent-Transfer-Encoding: base64\r\n\r\n{}",
                base64::engine::general_purpose::STANDARD.encode(raw)
            );
        }
        let result = inspect(raw.as_bytes(), Some("1.1.1.1.1.1"), false).unwrap();
        assert_eq!(result.extracted.unwrap(), b"deep");
    }

    #[test]
    fn unencoded_message_depth_is_bounded() {
        let mut raw = String::from("Content-Type: text/plain\r\n\r\ndeep");
        for _ in 0..32 {
            raw = format!("Content-Type: message/rfc822\r\n\r\n{raw}");
        }
        assert!(matches!(
            inspect(raw.as_bytes(), None, false),
            Err(Error::TooLarge(_))
        ));
    }
}
