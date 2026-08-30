use std::ffi::OsString;
use std::fmt;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::str::FromStr;

use percent_encoding::{AsciiSet, CONTROLS, percent_decode_str, percent_encode};

use crate::types::DragPayload;

const URI_PATH_ENCODE_SET: &AsciiSet = &CONTROLS
    .add(b' ')
    .add(b'"')
    .add(b'#')
    .add(b'%')
    .add(b'<')
    .add(b'>')
    .add(b'?')
    .add(b'`')
    .add(b'{')
    .add(b'}');

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct MimeType {
    type_: String,
    subtype: String,
    parameters: Vec<(String, String)>,
}

impl MimeType {
    pub fn essence(&self) -> String {
        format!("{}/{}", self.type_, self.subtype)
    }

    pub fn parameters(&self) -> &[(String, String)] {
        &self.parameters
    }

    pub fn is_uri_list(&self) -> bool {
        self.type_ == "text" && self.subtype == "uri-list" && self.parameters.is_empty()
    }

    pub fn is_utf8_text(&self) -> bool {
        if self.type_ != "text" || self.subtype != "plain" {
            return false;
        }
        self.parameters
            .iter()
            .all(|(name, value)| name == "charset" && matches!(value.as_str(), "utf-8" | "utf8"))
    }

    pub fn preferred_text() -> Self {
        Self {
            type_: "text".into(),
            subtype: "plain".into(),
            parameters: vec![("charset".into(), "utf-8".into())],
        }
    }
}

impl fmt::Display for MimeType {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}/{}", self.type_, self.subtype)?;
        for (name, value) in &self.parameters {
            write!(formatter, ";{name}={value}")?;
        }
        Ok(())
    }
}

impl FromStr for MimeType {
    type Err = MimeError;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        let mut parts = raw.split(';');
        let essence = parts.next().ok_or(MimeError::MalformedMime)?.trim();
        let (type_, subtype) = essence.split_once('/').ok_or(MimeError::MalformedMime)?;
        if type_.trim().is_empty() || subtype.trim().is_empty() || subtype.contains('/') {
            return Err(MimeError::MalformedMime);
        }

        let mut parameters = Vec::new();
        for raw_parameter in parts {
            let (name, value) = raw_parameter
                .split_once('=')
                .ok_or(MimeError::MalformedParameter)?;
            let name = name.trim().to_ascii_lowercase();
            let value = value.trim().trim_matches('"').to_ascii_lowercase();
            if name.is_empty() || value.is_empty() {
                return Err(MimeError::MalformedParameter);
            }
            parameters.push((name, value));
        }
        parameters.sort();

        Ok(Self {
            type_: type_.trim().to_ascii_lowercase(),
            subtype: subtype.trim().to_ascii_lowercase(),
            parameters,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MimeError {
    MalformedMime,
    MalformedParameter,
    UnsupportedMime(String),
    InvalidUtf8,
    MissingFinalCrlf,
    InvalidLineEnding,
    UnsupportedUri(String),
    NonLocalFileUri(String),
    InvalidPercentEncoding(String),
    RelativePath(PathBuf),
}

impl fmt::Display for MimeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{self:?}")
    }
}

impl std::error::Error for MimeError {}

pub fn decode_payload(mime: &MimeType, bytes: &[u8]) -> Result<DragPayload, MimeError> {
    if mime.is_uri_list() {
        let text = std::str::from_utf8(bytes).map_err(|_| MimeError::InvalidUtf8)?;
        parse_uri_list(text).map(DragPayload::Paths)
    } else if mime.is_utf8_text() {
        String::from_utf8(bytes.to_vec())
            .map(DragPayload::Text)
            .map_err(|_| MimeError::InvalidUtf8)
    } else {
        Err(MimeError::UnsupportedMime(mime.to_string()))
    }
}

pub fn parse_uri_list(body: &str) -> Result<Vec<PathBuf>, MimeError> {
    let bytes = body.as_bytes();
    if bytes
        .iter()
        .enumerate()
        .any(|(index, byte)| *byte == b'\n' && (index == 0 || bytes[index - 1] != b'\r'))
    {
        return Err(MimeError::InvalidLineEnding);
    }
    if !body.ends_with("\r\n") {
        return Err(MimeError::MissingFinalCrlf);
    }

    let mut paths = Vec::new();
    for line in body
        .strip_suffix("\r\n")
        .expect("checked above")
        .split("\r\n")
    {
        if line.contains(['\r', '\n']) {
            return Err(MimeError::InvalidLineEnding);
        }
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        paths.push(parse_file_uri(line)?);
    }
    Ok(paths)
}

pub fn encode_uri_list(paths: &[PathBuf]) -> Result<String, MimeError> {
    let mut body = String::new();
    for path in paths {
        if !path.is_absolute() {
            return Err(MimeError::RelativePath(path.clone()));
        }
        body.push_str("file://");
        body.push_str(
            &percent_encode(path.as_os_str().as_bytes(), URI_PATH_ENCODE_SET).to_string(),
        );
        body.push_str("\r\n");
    }
    Ok(body)
}

fn parse_file_uri(uri: &str) -> Result<PathBuf, MimeError> {
    let remainder = uri
        .strip_prefix("file://")
        .ok_or_else(|| MimeError::UnsupportedUri(uri.into()))?;
    let (authority, encoded_path) = if remainder.starts_with('/') {
        ("", remainder)
    } else {
        remainder
            .split_once('/')
            .map(|(authority, _)| (authority, &remainder[authority.len()..]))
            .ok_or_else(|| MimeError::UnsupportedUri(uri.into()))?
    };
    if !authority.is_empty() && !authority.eq_ignore_ascii_case("localhost") {
        return Err(MimeError::NonLocalFileUri(authority.into()));
    }
    if encoded_path.contains(['?', '#']) {
        return Err(MimeError::UnsupportedUri(uri.into()));
    }
    validate_percent_encoding(encoded_path)?;
    let bytes = percent_decode_str(encoded_path).collect::<Vec<_>>();
    if bytes.contains(&0) {
        return Err(MimeError::UnsupportedUri(uri.into()));
    }
    let path = PathBuf::from(OsString::from_vec(bytes));
    if !Path::new(&path).is_absolute() {
        return Err(MimeError::RelativePath(path));
    }
    Ok(path)
}

fn validate_percent_encoding(value: &str) -> Result<(), MimeError> {
    let bytes = value.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            if index + 2 >= bytes.len()
                || !bytes[index + 1].is_ascii_hexdigit()
                || !bytes[index + 2].is_ascii_hexdigit()
            {
                return Err(MimeError::InvalidPercentEncoding(value.into()));
            }
            index += 3;
        } else {
            index += 1;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_type_and_parameters_case_insensitively() {
        let plain = "text/plain".parse::<MimeType>().unwrap();
        let utf8 = "Text/Plain; Charset=\"UTF-8\"".parse::<MimeType>().unwrap();

        assert!(plain.is_utf8_text());
        assert!(utf8.is_utf8_text());
        assert_eq!(utf8.to_string(), "text/plain;charset=utf-8");
        assert_eq!(
            utf8.parameters(),
            &[("charset".to_owned(), "utf-8".to_owned())]
        );
    }

    #[test]
    fn rejects_unsupported_text_charset() {
        let latin1 = "text/plain;charset=iso-8859-1".parse::<MimeType>().unwrap();
        assert!(!latin1.is_utf8_text());
        assert!(matches!(
            decode_payload(&latin1, b"text"),
            Err(MimeError::UnsupportedMime(_))
        ));
    }

    #[test]
    fn parses_kde_uri_list_with_final_crlf_and_percent_decoding() {
        let body = "# Dolphin\r\nfile:///tmp/one%20two.txt\r\nfile://localhost/home/me/%23x\r\n";
        assert_eq!(
            parse_uri_list(body).unwrap(),
            vec![
                PathBuf::from("/tmp/one two.txt"),
                PathBuf::from("/home/me/#x")
            ]
        );
    }

    #[test]
    fn uri_list_requires_crlf_including_last_line() {
        assert_eq!(
            parse_uri_list("file:///tmp/a\n"),
            Err(MimeError::InvalidLineEnding)
        );
        assert_eq!(
            parse_uri_list("file:///tmp/a"),
            Err(MimeError::MissingFinalCrlf)
        );
    }

    #[test]
    fn rejects_non_local_file_host() {
        assert_eq!(
            parse_uri_list("file://fileserver/share/a\r\n"),
            Err(MimeError::NonLocalFileUri("fileserver".into()))
        );
    }

    #[test]
    fn encodes_every_uri_with_final_crlf() {
        let body = encode_uri_list(&[
            PathBuf::from("/tmp/one two.txt"),
            PathBuf::from("/tmp/#two"),
        ])
        .unwrap();
        assert_eq!(body, "file:///tmp/one%20two.txt\r\nfile:///tmp/%23two\r\n");
    }
}
