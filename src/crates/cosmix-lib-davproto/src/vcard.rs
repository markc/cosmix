//! JSContact (RFC 9553) → vCard 4.0 (RFC 6350) emit.
//!
//! v1 maps the indexed fields (UID, FN, EMAIL, ORG) plus a JSContact
//! `name.full` fallback for FN. Multi-valued emails/phones and structured
//! N are a later refinement; the JSContact payload is the authority and is
//! preserved verbatim in storage. Built by hand (the mapped field set is
//! small and the escaping is well-defined) rather than pulling a builder
//! dep; the `vcard4` *parser* arrives with the M3 write path.

/// Key under which a DAV PUT stashes the client's **verbatim** vCard in
/// the stored JSContact `data`, for a lossless GET round-trip. JMAP-
/// created contacts have no such key and emit from their fields.
pub const RAW_VCARD_KEY: &str = "cosmix:rawVCard";

/// Escape a vCard text value (RFC 6350 §3.4): backslash, comma, semicolon,
/// and newline are escaped; bare CR is dropped.
fn esc(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => o.push_str("\\\\"),
            ',' => o.push_str("\\,"),
            ';' => o.push_str("\\;"),
            '\n' => o.push_str("\\n"),
            '\r' => {}
            _ => o.push(c),
        }
    }
    o
}

/// Build a vCard 4.0 string from a contact's indexed fields + JSContact
/// `data` (for an FN fallback). FN is required by RFC 6350 §6.2.1, so it
/// falls back to `name.full` then the UID. Lines are CRLF-terminated.
pub fn contact_to_vcf(
    uid: &str,
    full_name: Option<&str>,
    email: Option<&str>,
    company: Option<&str>,
    data: &serde_json::Value,
) -> String {
    if let Some(raw) = data.get(RAW_VCARD_KEY).and_then(|v| v.as_str()) {
        return raw.to_string();
    }
    let mut lines = vec!["BEGIN:VCARD".to_string(), "VERSION:4.0".to_string()];
    lines.push(format!("UID:{}", esc(uid)));

    let fn_val = full_name
        .or_else(|| jscontact_full_name(data))
        .unwrap_or(uid);
    lines.push(format!("FN:{}", esc(fn_val)));

    if let Some(e) = email {
        lines.push(format!("EMAIL:{}", esc(e)));
    }
    if let Some(c) = company {
        lines.push(format!("ORG:{}", esc(c)));
    }
    if let Some(ph) = primary_phone(data) {
        lines.push(format!("TEL:{}", esc(&ph)));
    }
    lines.push("END:VCARD".to_string());
    let mut out = lines.join("\r\n");
    out.push_str("\r\n");
    out
}

/// The fields a DAV PUT extracts from an incoming vCard: the indexed
/// columns plus a JSContact `data` that stashes the verbatim vCard under
/// [`RAW_VCARD_KEY`] for a lossless round-trip.
pub struct ParsedContact {
    pub uid: Option<String>,
    pub full_name: Option<String>,
    pub email: Option<String>,
    pub company: Option<String>,
    pub data: serde_json::Value,
}

/// Unfold vCard logical lines (RFC 6350 §3.2): a CRLF (or LF) followed by
/// a single space or tab is a line continuation.
fn unfold(vcf: &str) -> String {
    vcf.replace("\r\n ", "")
        .replace("\r\n\t", "")
        .replace("\n ", "")
        .replace("\n\t", "")
}

/// Unescape a vCard text value (inverse of [`esc`]).
fn unesc(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('n') | Some('N') => o.push('\n'),
                Some(other) => o.push(other),
                None => o.push('\\'),
            }
        } else {
            o.push(c);
        }
    }
    o
}

/// Parse an incoming vCard, extracting UID / FN / EMAIL / ORG (first of
/// each) and stashing the verbatim body under [`RAW_VCARD_KEY`]. Only the
/// mapped fields are interpreted; everything else survives via the raw
/// stash. Hand-parsed (the field set is tiny and the grammar is simple).
pub fn parse_vcf(vcf: &str) -> anyhow::Result<ParsedContact> {
    let unfolded = unfold(vcf);
    let mut uid = None;
    let mut full_name = None;
    let mut email = None;
    let mut company = None;
    let mut phone = None;

    for raw_line in unfolded.lines() {
        let line = raw_line.trim_end_matches('\r');
        let Some(colon) = line.find(':') else {
            continue;
        };
        let (name_and_params, rest) = line.split_at(colon);
        let value = &rest[1..];
        // Property name is up to the first ';' (parameters), upper-cased.
        let prop = name_and_params
            .split(';')
            .next()
            .unwrap_or("")
            .to_ascii_uppercase();
        match prop.as_str() {
            "UID" if uid.is_none() => uid = Some(unesc(value)),
            "FN" if full_name.is_none() => full_name = Some(unesc(value)),
            "EMAIL" if email.is_none() => email = Some(unesc(value)),
            // ORG is `;`-structured (Company;Unit;…); take the first field.
            "ORG" if company.is_none() => {
                let first = value.split(';').next().unwrap_or(value);
                company = Some(unesc(first));
            }
            "TEL" if phone.is_none() => phone = Some(unesc(value)),
            _ => {}
        }
    }

    if full_name.is_none() {
        return Err(anyhow::anyhow!("vCard missing required FN"));
    }

    let mut data = serde_json::json!({ "@type": "Card" });
    if let Some(u) = &uid {
        data["uid"] = serde_json::Value::String(u.clone());
    }
    if let Some(n) = &full_name {
        data["name"] = serde_json::json!({ "full": n });
    }
    if let Some(p) = &phone {
        // JSContact phones map, so primary_phone() finds it structurally.
        data["phones"] = serde_json::json!({ "default": { "number": p } });
    }
    data[RAW_VCARD_KEY] = serde_json::Value::String(vcf.to_string());

    Ok(ParsedContact {
        uid,
        full_name,
        email,
        company,
        data,
    })
}

/// Patch the `FN` / `EMAIL` / `ORG` values of an existing vCard in place,
/// **preserving every other line** (phones, N, X-props, params). Used when
/// a structured (JMAP) edit touches a contact that carries a verbatim
/// vCard ([`RAW_VCARD_KEY`]) — so the DAV representation stays consistent
/// with the edit without losing the rich fields the structured model
/// doesn't carry. Only non-empty values are applied; the property+params
/// prefix before the first `:` is kept (so `EMAIL;TYPE=work` survives).
pub fn patch_vcf(
    raw: &str,
    full_name: Option<&str>,
    email: Option<&str>,
    company: Option<&str>,
    phone: Option<&str>,
) -> String {
    let fnv = full_name.filter(|s| !s.is_empty());
    let emv = email.filter(|s| !s.is_empty());
    let orv = company.filter(|s| !s.is_empty());
    let phv = phone.filter(|s| !s.is_empty());
    let mut out: Vec<String> = Vec::new();
    let (mut set_fn, mut set_em, mut set_or, mut set_ph) = (false, false, false, false);

    for line in unfold(raw).lines() {
        let l = line.trim_end_matches('\r');
        let prop = l
            .split([';', ':'])
            .next()
            .unwrap_or("")
            .to_ascii_uppercase();
        let colon = l.find(':');
        match (prop.as_str(), colon) {
            ("FN", Some(i)) if fnv.is_some() && !set_fn => {
                out.push(format!("{}:{}", &l[..i], esc(fnv.unwrap())));
                set_fn = true;
            }
            ("EMAIL", Some(i)) if emv.is_some() && !set_em => {
                out.push(format!("{}:{}", &l[..i], esc(emv.unwrap())));
                set_em = true;
            }
            ("ORG", Some(i)) if orv.is_some() && !set_or => {
                out.push(format!("{}:{}", &l[..i], esc(orv.unwrap())));
                set_or = true;
            }
            ("TEL", Some(i)) if phv.is_some() && !set_ph => {
                out.push(format!("{}:{}", &l[..i], esc(phv.unwrap())));
                set_ph = true;
            }
            ("END", _) => {
                if let Some(v) = fnv
                    && !set_fn
                {
                    out.push(format!("FN:{}", esc(v)));
                }
                if let Some(v) = emv
                    && !set_em
                {
                    out.push(format!("EMAIL:{}", esc(v)));
                }
                if let Some(v) = orv
                    && !set_or
                {
                    out.push(format!("ORG:{}", esc(v)));
                }
                if let Some(v) = phv
                    && !set_ph
                {
                    out.push(format!("TEL:{}", esc(v)));
                }
                out.push(l.to_string());
            }
            _ => out.push(l.to_string()),
        }
    }
    let mut s = out.join("\r\n");
    s.push_str("\r\n");
    s
}

/// The primary phone number for a stored contact: the first JSContact
/// `phones` entry's `number`, falling back to the first `TEL` line of a
/// stashed vCard ([`RAW_VCARD_KEY`]) — so TB-imported contacts (whose
/// phone lives only in the raw vCard) surface a number too.
pub fn primary_phone(data: &serde_json::Value) -> Option<String> {
    if let Some(num) = data
        .get("phones")
        .and_then(|v| v.as_object())
        .and_then(|m| m.values().next())
        .and_then(|p| p.get("number"))
        .and_then(|n| n.as_str())
        && !num.is_empty()
    {
        return Some(num.to_string());
    }
    if let Some(raw) = data.get(RAW_VCARD_KEY).and_then(|v| v.as_str()) {
        for line in unfold(raw).lines() {
            let l = line.trim_end_matches('\r');
            let prop = l
                .split([';', ':'])
                .next()
                .unwrap_or("")
                .to_ascii_uppercase();
            if prop == "TEL"
                && let Some(i) = l.find(':')
            {
                let v = unesc(l[i + 1..].trim());
                if !v.is_empty() {
                    return Some(v);
                }
            }
        }
    }
    None
}

/// Set the primary phone `number` in a JSContact `data`, **preserving**
/// any existing `phones` map (siblings + per-phone metadata): updates the
/// first entry's `number`, or creates a `default` entry if none exist.
pub fn set_primary_phone(data: &mut serde_json::Value, number: &str) {
    if let Some(map) = data.get_mut("phones").and_then(|v| v.as_object_mut()) {
        if let Some(k) = map.keys().next().cloned() {
            if let Some(obj) = map.get_mut(&k) {
                obj["number"] = serde_json::Value::String(number.to_string());
            }
        } else {
            map.insert("default".into(), serde_json::json!({ "number": number }));
        }
    } else {
        data["phones"] = serde_json::json!({ "default": { "number": number } });
    }
}

/// JSContact `name.full` — the formatted full name, if present.
fn jscontact_full_name(data: &serde_json::Value) -> Option<&str> {
    data.get("name")
        .and_then(|n| n.get("full"))
        .and_then(|f| f.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn emits_valid_vcard() {
        let data = serde_json::json!({});
        let vcf = contact_to_vcf(
            "c-1",
            Some("Ada Lovelace"),
            Some("ada@example.com"),
            Some("Analytical Engines"),
            &data,
        );
        assert!(vcf.starts_with("BEGIN:VCARD\r\nVERSION:4.0\r\n"));
        assert!(vcf.contains("UID:c-1\r\n"));
        assert!(vcf.contains("FN:Ada Lovelace\r\n"));
        assert!(vcf.contains("EMAIL:ada@example.com\r\n"));
        assert!(vcf.contains("ORG:Analytical Engines\r\n"));
        assert!(vcf.ends_with("END:VCARD\r\n"));
    }

    #[test]
    fn set_primary_phone_preserves_siblings_and_metadata() {
        let mut d =
            serde_json::json!({"phones":{"home":{"number":"111","features":{"voice":true}}}});
        set_primary_phone(&mut d, "999");
        assert_eq!(d["phones"]["home"]["number"], "999"); // first entry updated
        assert_eq!(d["phones"]["home"]["features"]["voice"], true); // metadata kept
        let mut e = serde_json::json!({"@type":"Card"});
        set_primary_phone(&mut e, "555");
        assert_eq!(e["phones"]["default"]["number"], "555"); // created when absent
    }

    #[test]
    fn primary_phone_raw_fallback_exact_tel() {
        // A custom `TELX:` must not be mistaken for `TEL:`.
        let d = serde_json::json!({"cosmix:rawVCard":
            "BEGIN:VCARD\r\nFN:N\r\nTELX:nope\r\nTEL;TYPE=cell:real\r\nEND:VCARD\r\n"});
        assert_eq!(primary_phone(&d).as_deref(), Some("real"));
    }

    #[test]
    fn fn_falls_back_to_jscontact_then_uid() {
        let data = serde_json::json!({ "name": { "full": "Grace Hopper" } });
        let vcf = contact_to_vcf("c-2", None, None, None, &data);
        assert!(vcf.contains("FN:Grace Hopper\r\n"));

        let empty = serde_json::json!({});
        let vcf2 = contact_to_vcf("c-3", None, None, None, &empty);
        assert!(vcf2.contains("FN:c-3\r\n"));
    }

    #[test]
    fn escapes_special_chars() {
        let data = serde_json::json!({});
        let vcf = contact_to_vcf("c-4", Some("Doe; John, Jr."), None, None, &data);
        assert!(vcf.contains("FN:Doe\\; John\\, Jr.\r\n"));
    }

    #[test]
    fn parse_extracts_fields_and_stashes_raw() {
        let vcf = "BEGIN:VCARD\r\nVERSION:4.0\r\nUID:u-9\r\nFN:Alan Turing\r\n\
                   EMAIL;TYPE=work:alan@example.com\r\nORG:Bletchley;Hut 8\r\nEND:VCARD\r\n";
        let p = parse_vcf(vcf).unwrap();
        assert_eq!(p.uid.as_deref(), Some("u-9"));
        assert_eq!(p.full_name.as_deref(), Some("Alan Turing"));
        assert_eq!(p.email.as_deref(), Some("alan@example.com"));
        assert_eq!(p.company.as_deref(), Some("Bletchley")); // first ORG field
        assert_eq!(p.data[RAW_VCARD_KEY], vcf);
    }

    #[test]
    fn raw_stash_round_trips_through_emit() {
        let vcf = "BEGIN:VCARD\r\nVERSION:4.0\r\nUID:u\r\nFN:N\r\n\
                   X-CUSTOM:keep-me\r\nEND:VCARD\r\n";
        let p = parse_vcf(vcf).unwrap();
        let out = contact_to_vcf("u", Some("N"), None, None, &p.data);
        assert_eq!(out, vcf);
        assert!(out.contains("X-CUSTOM:keep-me"));
    }

    #[test]
    fn parse_rejects_missing_fn() {
        let vcf = "BEGIN:VCARD\r\nVERSION:4.0\r\nUID:u\r\nEND:VCARD\r\n";
        assert!(parse_vcf(vcf).is_err());
    }

    #[test]
    fn patch_preserves_other_lines() {
        let vcf = "BEGIN:VCARD\r\nVERSION:4.0\r\nUID:u\r\nFN:Old Name\r\n\
                   TEL;VALUE=UNKNOWN:53 684 245\r\nEMAIL;TYPE=work:old@x.com\r\nEND:VCARD\r\n";
        let out = patch_vcf(vcf, Some("New Name"), Some("new@x.com"), Some("Acme"), None);
        assert!(out.contains("FN:New Name\r\n")); // updated
        assert!(out.contains("EMAIL;TYPE=work:new@x.com\r\n")); // value changed, params kept
        assert!(out.contains("TEL;VALUE=UNKNOWN:53 684 245\r\n")); // phone PRESERVED (None = no change)
        assert!(out.contains("UID:u\r\n")); // uid preserved
        assert!(out.contains("ORG:Acme\r\n")); // added (wasn't present)
        assert!(!out.contains("Old Name"));
    }

    #[test]
    fn phone_round_trips_and_patches() {
        // primary_phone derives from a stashed vCard's TEL line.
        let p = parse_vcf(
            "BEGIN:VCARD\r\nVERSION:4.0\r\nFN:T\r\nTEL;TYPE=cell:+61400000000\r\nEND:VCARD\r\n",
        )
        .unwrap();
        assert_eq!(primary_phone(&p.data).as_deref(), Some("+61400000000"));
        assert_eq!(p.data["phones"]["default"]["number"], "+61400000000"); // also structured
        // emit builds a TEL from data.phones for a non-raw contact.
        let data = serde_json::json!({"phones":{"x":{"number":"123"}}});
        assert!(contact_to_vcf("u", Some("N"), None, None, &data).contains("TEL:123\r\n"));
        // patch updates the TEL value, keeping its params + other lines.
        let raw =
            "BEGIN:VCARD\r\nVERSION:4.0\r\nFN:N\r\nTEL;TYPE=cell:111\r\nNOTE:keep\r\nEND:VCARD\r\n";
        let out = patch_vcf(raw, None, None, None, Some("999"));
        assert!(out.contains("TEL;TYPE=cell:999\r\n"));
        assert!(out.contains("NOTE:keep\r\n"));
        assert!(!out.contains(":111"));
    }

    #[test]
    fn unfolds_continuation_lines() {
        let vcf = "BEGIN:VCARD\r\nVERSION:4.0\r\nFN:Very Long\r\n  Name\r\nEND:VCARD\r\n";
        let p = parse_vcf(vcf).unwrap();
        assert_eq!(p.full_name.as_deref(), Some("Very Long Name"));
    }
}
