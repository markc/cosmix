//! DAV URL ⇆ resource parsing + account binding.
//!
//! All DAV resources live under `/dav/…`. The `{account}` segment is the
//! **stringified numeric JMAP account id** (`auth::authenticate`
//! returns `Option<i32>` and JMAP uses `account_id.to_string()` as its
//! `acct`), so the binding check downstream is integer equality — see
//! `_plan/2026-06-09-maild-dav-server.md` §4.

/// A parsed DAV resource path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resource {
    /// `/dav/` — discovery root (current-user-principal probe).
    Root,
    /// `/dav/principals/{account}/`
    Principal { account: i32 },
    /// `/dav/calendars/{account}/`
    CalendarHome { account: i32 },
    /// `/dav/calendars/{account}/{calendarId}/`
    Calendar { account: i32, calendar_id: String },
    /// `/dav/calendars/{account}/{calendarId}/{uid}.ics`
    CalendarObject {
        account: i32,
        calendar_id: String,
        uid: String,
    },
    /// `/dav/addressbooks/{account}/`
    AddressbookHome { account: i32 },
    /// `/dav/addressbooks/{account}/{bookId}/`
    Addressbook { account: i32, book_id: String },
    /// `/dav/addressbooks/{account}/{bookId}/{uid}.vcf`
    AddressbookObject {
        account: i32,
        book_id: String,
        uid: String,
    },
}

impl Resource {
    /// The numeric account this path is scoped to, if any (`Root` has none).
    pub fn account_id(&self) -> Option<i32> {
        match self {
            Resource::Root => None,
            Resource::Principal { account }
            | Resource::CalendarHome { account }
            | Resource::Calendar { account, .. }
            | Resource::CalendarObject { account, .. }
            | Resource::AddressbookHome { account }
            | Resource::Addressbook { account, .. }
            | Resource::AddressbookObject { account, .. } => Some(*account),
        }
    }
}

/// Reject `.`/`..`/empty segments — defence in depth. The resources only
/// ever become account-scoped DB lookup keys (never filesystem paths), but
/// a `..` segment must not silently parse as a calendar/uid identifier.
fn safe(seg: &str) -> Option<&str> {
    if seg.is_empty() || seg == "." || seg == ".." {
        None
    } else {
        Some(seg)
    }
}

/// Strip a required `.ics`/`.vcf` extension, returning the bare UID.
fn strip_ext<'a>(seg: &'a str, ext: &str) -> Option<&'a str> {
    safe(seg)?.strip_suffix(ext).filter(|s| !s.is_empty())
}

/// Parse a request path (e.g. `/dav/calendars/1/<uuid>/<uid>.ics`) into a
/// typed [`Resource`], or `None` if it isn't a recognised DAV path.
pub fn parse(path: &str) -> Option<Resource> {
    let segs: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    match segs.as_slice() {
        ["dav"] => Some(Resource::Root),
        ["dav", "principals", acct] => Some(Resource::Principal {
            account: safe(acct)?.parse().ok()?,
        }),
        ["dav", "calendars", acct] => Some(Resource::CalendarHome {
            account: safe(acct)?.parse().ok()?,
        }),
        ["dav", "calendars", acct, cal] => Some(Resource::Calendar {
            account: safe(acct)?.parse().ok()?,
            calendar_id: safe(cal)?.to_string(),
        }),
        ["dav", "calendars", acct, cal, obj] => Some(Resource::CalendarObject {
            account: safe(acct)?.parse().ok()?,
            calendar_id: safe(cal)?.to_string(),
            uid: strip_ext(obj, ".ics")?.to_string(),
        }),
        ["dav", "addressbooks", acct] => Some(Resource::AddressbookHome {
            account: safe(acct)?.parse().ok()?,
        }),
        ["dav", "addressbooks", acct, book] => Some(Resource::Addressbook {
            account: safe(acct)?.parse().ok()?,
            book_id: safe(book)?.to_string(),
        }),
        ["dav", "addressbooks", acct, book, obj] => Some(Resource::AddressbookObject {
            account: safe(acct)?.parse().ok()?,
            book_id: safe(book)?.to_string(),
            uid: strip_ext(obj, ".vcf")?.to_string(),
        }),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_discovery_paths() {
        assert_eq!(parse("/dav"), Some(Resource::Root));
        assert_eq!(parse("/dav/"), Some(Resource::Root));
        assert_eq!(
            parse("/dav/principals/7/"),
            Some(Resource::Principal { account: 7 })
        );
        assert_eq!(
            parse("/dav/calendars/7/"),
            Some(Resource::CalendarHome { account: 7 })
        );
        assert_eq!(
            parse("/dav/addressbooks/7/"),
            Some(Resource::AddressbookHome { account: 7 })
        );
    }

    #[test]
    fn parses_collection_and_object_paths() {
        assert_eq!(
            parse("/dav/calendars/7/abc/"),
            Some(Resource::Calendar {
                account: 7,
                calendar_id: "abc".into()
            })
        );
        assert_eq!(
            parse("/dav/calendars/7/abc/evt-1.ics"),
            Some(Resource::CalendarObject {
                account: 7,
                calendar_id: "abc".into(),
                uid: "evt-1".into()
            })
        );
        assert_eq!(
            parse("/dav/addressbooks/7/def/c-1.vcf"),
            Some(Resource::AddressbookObject {
                account: 7,
                book_id: "def".into(),
                uid: "c-1".into()
            })
        );
    }

    #[test]
    fn rejects_bad_paths() {
        assert_eq!(parse("/dav/calendars/notanint/"), None);
        assert_eq!(parse("/dav/calendars/7/../etc"), None); // `..` segment rejected
        assert_eq!(parse("/dav/calendars/7/abc/noext"), None); // object needs .ics
        assert_eq!(parse("/other"), None);
        assert_eq!(parse("/"), None);
    }

    #[test]
    fn account_binding_extracts_id() {
        assert_eq!(
            parse("/dav/calendars/7/abc/").unwrap().account_id(),
            Some(7)
        );
        assert_eq!(parse("/dav").unwrap().account_id(), None);
    }
}
