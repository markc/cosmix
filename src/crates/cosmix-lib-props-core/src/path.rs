//! Dotted property paths per SPEC 07 §2.2.

use serde::{Deserialize, Serialize};
use std::fmt;

/// A validated dotted property path. Segments are lowercase
/// alphanumeric + `_`; the wildcard `*` is reserved.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PropPath(String);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PropPathError {
    Empty,
    EmptySegment,
    InvalidChar { segment: String, ch: char },
    Wildcard,
}

impl fmt::Display for PropPathError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => write!(f, "property path is empty"),
            Self::EmptySegment => write!(f, "property path has empty segment"),
            Self::InvalidChar { segment, ch } => {
                write!(f, "segment {segment:?} contains invalid char {ch:?}")
            }
            Self::Wildcard => write!(f, "wildcard '*' is reserved (SPEC 07 §2.2)"),
        }
    }
}

impl std::error::Error for PropPathError {}

impl PropPath {
    pub fn new(s: impl Into<String>) -> Result<Self, PropPathError> {
        let s = s.into();
        if s.is_empty() {
            return Err(PropPathError::Empty);
        }
        for seg in s.split('.') {
            if seg.is_empty() {
                return Err(PropPathError::EmptySegment);
            }
            if seg == "*" || seg.contains('*') {
                return Err(PropPathError::Wildcard);
            }
            for ch in seg.chars() {
                if !(ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_') {
                    return Err(PropPathError::InvalidChar {
                        segment: seg.to_string(),
                        ch,
                    });
                }
            }
        }
        Ok(Self(s))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn segments(&self) -> std::str::Split<'_, char> {
        self.0.split('.')
    }

    /// Returns true iff `self` is `other` or a descendant.
    pub fn starts_with(&self, other: &PropPath) -> bool {
        if self.0 == other.0 {
            return true;
        }
        self.0.starts_with(&other.0) && self.0.as_bytes().get(other.0.len()) == Some(&b'.')
    }
}

impl fmt::Display for PropPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::str::FromStr for PropPath {
    type Err = PropPathError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::new(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_valid() {
        for s in ["config.bind", "lifecycle.uptime_s", "a", "x_y_z.q1"] {
            assert!(PropPath::new(s).is_ok(), "should accept {s}");
        }
    }

    #[test]
    fn rejects_invalid() {
        let cases = [
            ("", PropPathError::Empty),
            ("a..b", PropPathError::EmptySegment),
            (".a", PropPathError::EmptySegment),
            ("a.", PropPathError::EmptySegment),
            (
                "Foo",
                PropPathError::InvalidChar {
                    segment: "Foo".into(),
                    ch: 'F',
                },
            ),
            (
                "a-b",
                PropPathError::InvalidChar {
                    segment: "a-b".into(),
                    ch: '-',
                },
            ),
            ("foo.*", PropPathError::Wildcard),
            ("a.b*c", PropPathError::Wildcard),
        ];
        for (s, want) in cases {
            assert_eq!(PropPath::new(s).unwrap_err(), want, "input {s}");
        }
    }

    #[test]
    fn starts_with_subtree() {
        let p = PropPath::new("config.bind").unwrap();
        let root = PropPath::new("config").unwrap();
        assert!(p.starts_with(&root));
        assert!(p.starts_with(&p));
        // not a prefix-by-string match: "config" vs "configd"
        let other = PropPath::new("configd").unwrap();
        assert!(!PropPath::new("configd.x").unwrap().starts_with(&root));
        assert!(other.starts_with(&PropPath::new("configd").unwrap()));
    }
}
