//! `LOGIN <user> <password>` — RFC 9051 §6.2.3.
//!
//! Legal only in `NotAuthenticated`. Phase 1 accepts the simple
//! two-atom form; quoted-string forms (`LOGIN "alice" "p w"`) are
//! also accepted because real clients use them when the password
//! contains a space. Literals (`{N}`) are rejected by the codec in
//! Phase 1 — Thunderbird only emits LOGIN literals when the password
//! contains an 8-bit byte, which is rare.

use anyhow::{Result, anyhow};

/// Parse the `<user> <password>` tail. Both fields may be either
/// atoms or `"..."`-quoted strings. Backslash escapes are recognised
/// inside quoted strings per RFC 9051 §4.3.
///
/// Returns `Err` for malformed input — caller maps to `BAD invalid
/// LOGIN syntax`.
pub fn parse_args(args: &str) -> Result<(String, String)> {
    let mut cursor = args.trim_start();
    let user = take_astring(&mut cursor)?;
    cursor = cursor.trim_start();
    let pass = take_astring(&mut cursor)?;
    if !cursor.trim().is_empty() {
        return Err(anyhow!("trailing data after password"));
    }
    Ok((user, pass))
}

fn take_astring(cursor: &mut &str) -> Result<String> {
    let bytes = cursor.as_bytes();
    if bytes.is_empty() {
        return Err(anyhow!("missing argument"));
    }
    if bytes[0] == b'"' {
        // Quoted string.
        let mut out = String::new();
        let mut i = 1;
        while i < bytes.len() {
            match bytes[i] {
                b'"' => {
                    *cursor = std::str::from_utf8(&bytes[i + 1..])
                        .map_err(|_| anyhow!("non-UTF8 LOGIN argument"))?;
                    return Ok(out);
                }
                b'\\' if i + 1 < bytes.len() => {
                    out.push(bytes[i + 1] as char);
                    i += 2;
                }
                b if b < 0x20 => return Err(anyhow!("control char in quoted string")),
                _ => {
                    out.push(bytes[i] as char);
                    i += 1;
                }
            }
        }
        Err(anyhow!("unterminated quoted string"))
    } else {
        // Atom — read until whitespace.
        let end = bytes
            .iter()
            .position(|b| matches!(*b, b' ' | b'\t'))
            .unwrap_or(bytes.len());
        let word = std::str::from_utf8(&bytes[..end])
            .map_err(|_| anyhow!("non-UTF8 LOGIN argument"))?
            .to_string();
        *cursor =
            std::str::from_utf8(&bytes[end..]).map_err(|_| anyhow!("non-UTF8 LOGIN argument"))?;
        Ok(word)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_atoms() {
        let (u, p) = parse_args("alice@example.com hunter2").unwrap();
        assert_eq!(u, "alice@example.com");
        assert_eq!(p, "hunter2");
    }

    #[test]
    fn parses_quoted_password() {
        let (u, p) = parse_args(r#"alice "p\"w""#).unwrap();
        assert_eq!(u, "alice");
        assert_eq!(p, "p\"w");
    }

    #[test]
    fn parses_quoted_both() {
        let (u, p) = parse_args(r#""al ice" "hunter 2""#).unwrap();
        assert_eq!(u, "al ice");
        assert_eq!(p, "hunter 2");
    }

    #[test]
    fn rejects_missing_password() {
        assert!(parse_args("alice").is_err());
    }

    #[test]
    fn rejects_trailing_data() {
        assert!(parse_args("alice pw extra").is_err());
    }

    #[test]
    fn rejects_unterminated_quote() {
        assert!(parse_args(r#"alice "pw"#).is_err());
    }
}
