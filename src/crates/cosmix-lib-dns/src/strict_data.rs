//! `zones.mix` loader (`strict_data.rs`) — the **pinned P1 schema**.
//!
//! Parsed via `cosmix_mix::parse_data` (the mandated substrate
//! strict-data parser — NOT hand-rolled, NOT TOML/JSON/serde). Plan
//! §3's `zone "bus" { … bundle owner=… { record { … } } }` is
//! *illustrative pseudo-syntax for the fields*, **not** the on-disk
//! format. `parse_data` accepts only scalars / `[ list ]` / `{ map }`,
//! so the concrete schema (a reviewable P1 artifact, pinned by tests)
//! is the map/list tree below.
//!
//! ```mix
//! zones: {
//!   "bus": {
//!     soa: { primary: "gw.bus", mbox: "hostmaster.bus", ttl: 300, minimum: 60 },
//!     ns: [ "gw.bus" ],
//!     serial_floor: 1,
//!     bundles: [
//!       { owner: "alpha", serial: 1, records: [
//!           { name: "alpha.bus", type: "A",  ttl: 300, data: "192.0.2.5" },
//!           { name: "alpha.bus", type: "MX", ttl: 300, data: "10 alpha.bus" },
//!           { name: "old.bus",     type: "A",  ttl: 300, withdrawn: true, data: "1.2.3.4" }
//!       ] }
//!     ]
//!   }
//! }
//! ```
//!
//! `data` text formats: `A`=dotted IPv4, `AAAA`=IPv6, `NS`/`PTR`=name,
//! `MX`="<pref> <exch>", `SRV`="<prio> <weight> <port> <target>",
//! `TXT`=string or list of strings. `type` is the closed vocab; `SOA`
//! is **rejected** in `records` (apex SOA comes from the `soa` block),
//! as are `CNAME`/`HINFO`/unknown. Inside `{ }`, Mix strict-data
//! requires commas between entries (newlines are suppressed in braces);
//! tombstone shape = `withdrawn: true` (granularity / data-required is
//! v1-OPEN, not frozen here).

use crate::canonical::{Name, NameError};
use crate::model::{
    Bundle, OwnerId, ParsedZones, RecordAssertion, Serial, SoaConfig, ZoneName, ZoneSource,
};
use crate::rr::{RData, RecordType};
use cosmix_mix::value::Value;
use std::collections::BTreeMap;
use std::fmt;
use std::net::{Ipv4Addr, Ipv6Addr};
use std::path::Path;

#[derive(Debug)]
pub struct ZonesParseError(pub String);

impl fmt::Display for ZonesParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "zones.mix: {}", self.0)
    }
}

impl std::error::Error for ZonesParseError {}

impl From<NameError> for ZonesParseError {
    fn from(e: NameError) -> Self {
        ZonesParseError(format!("bad name: {e}"))
    }
}

type R<T> = Result<T, ZonesParseError>;

fn err<T>(m: impl Into<String>) -> R<T> {
    Err(ZonesParseError(m.into()))
}

fn as_map<'a>(v: &'a Value, ctx: &str) -> R<&'a indexmap::IndexMap<String, Value>> {
    match v {
        Value::Map(m) => Ok(m),
        _ => err(format!("{ctx}: expected a map")),
    }
}

fn as_list<'a>(v: &'a Value, ctx: &str) -> R<&'a Vec<Value>> {
    match v {
        Value::List(l) => Ok(l),
        _ => err(format!("{ctx}: expected a list")),
    }
}

fn as_str<'a>(v: &'a Value, ctx: &str) -> R<&'a str> {
    match v {
        Value::String(s) => Ok(s),
        _ => err(format!("{ctx}: expected a string")),
    }
}

fn as_u32(v: &Value, ctx: &str) -> R<u32> {
    match v {
        Value::Number(n) => {
            if !n.is_finite() || n.fract() != 0.0 || *n < 0.0 || *n > f64::from(u32::MAX) {
                return err(format!("{ctx}: {n} is not a u32"));
            }
            Ok(*n as u32)
        }
        _ => err(format!("{ctx}: expected a number")),
    }
}

fn get<'a>(m: &'a indexmap::IndexMap<String, Value>, k: &str, ctx: &str) -> R<&'a Value> {
    m.get(k)
        .ok_or_else(|| ZonesParseError(format!("{ctx}: missing key {k:?}")))
}

fn parse_soa(v: &Value) -> R<SoaConfig> {
    let m = as_map(v, "soa")?;
    Ok(SoaConfig {
        primary: Name::parse(as_str(get(m, "primary", "soa")?, "soa.primary")?)?,
        mbox: Name::parse(as_str(get(m, "mbox", "soa")?, "soa.mbox")?)?,
        ttl: as_u32(get(m, "ttl", "soa")?, "soa.ttl")?,
        minimum: as_u32(get(m, "minimum", "soa")?, "soa.minimum")?,
    })
}

fn parse_rdata(rr_type: RecordType, data: &Value) -> R<RData> {
    // TXT is the only multi-string / list-shaped rdata.
    if rr_type == RecordType::Txt {
        return match data {
            Value::String(s) => Ok(RData::Txt(vec![s.clone()])),
            Value::List(l) => {
                let mut parts = Vec::with_capacity(l.len());
                for item in l.iter() {
                    parts.push(as_str(item, "TXT data item")?.to_string());
                }
                Ok(RData::Txt(parts))
            }
            _ => err("TXT data: expected a string or list of strings"),
        };
    }
    let s = as_str(data, "record data")?;
    match rr_type {
        RecordType::A => {
            let ip: Ipv4Addr = s
                .parse()
                .map_err(|_| ZonesParseError(format!("A data {s:?} is not IPv4")))?;
            Ok(RData::A(ip))
        }
        RecordType::Aaaa => {
            let ip: Ipv6Addr = s
                .parse()
                .map_err(|_| ZonesParseError(format!("AAAA data {s:?} is not IPv6")))?;
            Ok(RData::Aaaa(ip))
        }
        RecordType::Ns => Ok(RData::Ns(Name::parse(s)?)),
        RecordType::Ptr => Ok(RData::Ptr(Name::parse(s)?)),
        RecordType::Mx => {
            let mut it = s.split_whitespace();
            let pref = it
                .next()
                .ok_or_else(|| ZonesParseError("MX data: missing preference".into()))?;
            let exch = it
                .next()
                .ok_or_else(|| ZonesParseError("MX data: missing exchange".into()))?;
            if it.next().is_some() {
                return err(format!("MX data {s:?}: expected '<pref> <exch>'"));
            }
            let pref: u16 = pref
                .parse()
                .map_err(|_| ZonesParseError(format!("MX preference {pref:?} not u16")))?;
            Ok(RData::Mx {
                pref,
                exch: Name::parse(exch)?,
            })
        }
        RecordType::Srv => {
            let p: Vec<&str> = s.split_whitespace().collect();
            if p.len() != 4 {
                return err(format!(
                    "SRV data {s:?}: expected '<prio> <weight> <port> <target>'"
                ));
            }
            let prio: u16 = p[0]
                .parse()
                .map_err(|_| ZonesParseError(format!("SRV prio {:?} not u16", p[0])))?;
            let weight: u16 = p[1]
                .parse()
                .map_err(|_| ZonesParseError(format!("SRV weight {:?} not u16", p[1])))?;
            let port: u16 = p[2]
                .parse()
                .map_err(|_| ZonesParseError(format!("SRV port {:?} not u16", p[2])))?;
            Ok(RData::Srv {
                prio,
                weight,
                port,
                target: Name::parse(p[3])?,
            })
        }
        RecordType::Txt => unreachable!("TXT handled above"),
        RecordType::Soa => err("SOA is not a valid record type in `records`"),
    }
}

fn parse_record(owner: &OwnerId, v: &Value) -> R<RecordAssertion> {
    let m = as_map(v, "record")?;
    // Owner name — `parse_owner` so a leftmost `*` (RFC 4592 wildcard)
    // is accepted here, and ONLY here (rdata/SOA/NS/apex use strict parse).
    let name = Name::parse_owner(as_str(get(m, "name", "record")?, "record.name")?)?;
    let type_str = as_str(get(m, "type", "record")?, "record.type")?;
    let rr_type = RecordType::parse(type_str).ok_or_else(|| {
        ZonesParseError(format!(
            "record type {type_str:?} is not in the closed vocab \
             (A/AAAA/NS/MX/SRV/TXT/PTR; SOA/CNAME/HINFO rejected)"
        ))
    })?;
    let ttl = as_u32(get(m, "ttl", "record")?, "record.ttl")?;
    let withdrawn = match m.get("withdrawn") {
        None => false,
        Some(Value::Bool(b)) => *b,
        Some(_) => return err("record.withdrawn: expected a boolean"),
    };
    let rdata = parse_rdata(rr_type, get(m, "data", "record")?)?;
    if rdata.record_type() != rr_type {
        return err(format!(
            "record {:?}: data does not match declared type {type_str:?}",
            name.as_str()
        ));
    }
    Ok(RecordAssertion {
        owner: owner.clone(),
        name,
        rr_type,
        ttl,
        rdata,
        withdrawn,
    })
}

fn parse_bundle(v: &Value) -> R<Bundle> {
    let m = as_map(v, "bundle")?;
    let owner = OwnerId(as_str(get(m, "owner", "bundle")?, "bundle.owner")?.to_string());
    let serial = Serial(as_u32(get(m, "serial", "bundle")?, "bundle.serial")?);
    let records_v = as_list(get(m, "records", "bundle")?, "bundle.records")?;
    let mut records = Vec::with_capacity(records_v.len());
    for rv in records_v {
        records.push(parse_record(&owner, rv)?);
    }
    Ok(Bundle {
        owner,
        serial,
        records,
    })
}

fn parse_zone(zone_name: &str, v: &Value) -> R<ZoneSource> {
    let m = as_map(v, "zone")?;
    let name = ZoneName(Name::parse(zone_name)?);
    let soa = parse_soa(get(m, "soa", "zone")?)?;
    let ns_list = as_list(get(m, "ns", "zone")?, "zone.ns")?;
    let mut ns = Vec::with_capacity(ns_list.len());
    for nv in ns_list {
        ns.push(Name::parse(as_str(nv, "zone.ns item")?)?);
    }
    let configured_serial_floor = Serial(as_u32(
        get(m, "serial_floor", "zone")?,
        "zone.serial_floor",
    )?);
    let bundles_list = as_list(get(m, "bundles", "zone")?, "zone.bundles")?;
    let mut bundles = BTreeMap::new();
    for bv in bundles_list {
        let b = parse_bundle(bv)?;
        if bundles.insert(b.owner.clone(), b.clone()).is_some() {
            return err(format!(
                "zone {zone_name:?}: duplicate bundle for owner {:?}",
                b.owner.0
            ));
        }
    }
    Ok(ZoneSource {
        name,
        soa,
        ns,
        configured_serial_floor,
        bundles,
    })
}

fn parsed_zones_from_value(root: &Value) -> R<ParsedZones> {
    let top = as_map(root, "top level")?;
    let zones_v = get(top, "zones", "top level")?;
    let zones_m = as_map(zones_v, "zones")?;
    let mut zones = BTreeMap::new();
    for (zname, zval) in zones_m {
        let zs = parse_zone(zname, zval)?;
        zones.insert(zs.name.clone(), zs);
    }
    if zones.is_empty() {
        return err("no zones defined");
    }
    Ok(ParsedZones { zones })
}

/// Parse a `zones.mix` *source string* into owner-aware `ParsedZones`
/// (Layer 1 — never flattened here; anti-pattern (a)).
pub fn parse_zones(source: &str) -> R<ParsedZones> {
    let v = cosmix_mix::parse_data(source)
        .map_err(|e| ZonesParseError(format!("strict-data parse failed: {e}")))?;
    parsed_zones_from_value(&v)
}

/// Parse a `zones.mix` *file*.
pub fn parse_zones_file(path: &Path) -> R<ParsedZones> {
    let src = std::fs::read_to_string(path)
        .map_err(|e| ZonesParseError(format!("read {}: {e}", path.display())))?;
    parse_zones(&src)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
zones: {
  "bus": {
    soa: { primary: "gw.bus", mbox: "hostmaster.bus", ttl: 300, minimum: 60 },
    ns: [ "gw.bus" ],
    serial_floor: 1,
    bundles: [
      { owner: "alpha", serial: 1, records: [
          { name: "alpha.bus", type: "A",  ttl: 300, data: "192.0.2.5" },
          { name: "alpha.bus", type: "MX", ttl: 300, data: "10 alpha.bus" },
          { name: "alpha.bus", type: "TXT", ttl: 300, data: [ "v=mesh1" ] }
      ] }
    ]
  }
}
"#;

    #[test]
    fn parses_pinned_map_list_schema() {
        let pz = parse_zones(SAMPLE).unwrap();
        let zn = ZoneName(Name::parse("bus").unwrap());
        let z = pz.zones.get(&zn).unwrap();
        assert_eq!(z.soa.ttl, 300);
        assert_eq!(z.configured_serial_floor, Serial(1));
        let b = z.bundles.get(&OwnerId("alpha".into())).unwrap();
        assert_eq!(b.serial, Serial(1));
        assert_eq!(b.records.len(), 3);
    }

    #[test]
    fn rejects_plan_pseudo_syntax_string() {
        // The plan's illustrative DSL is NOT the on-disk format.
        let pseudo = r#"zone "bus" { bundle owner="alpha" serial=1 { record { } } }"#;
        assert!(
            parse_zones(pseudo).is_err(),
            "plan pseudo-syntax must not parse as the schema"
        );
    }

    #[test]
    fn rejects_soa_and_cname_and_hinfo_record_types() {
        for bad in ["SOA", "CNAME", "HINFO"] {
            let src = format!(
                r#"zones: {{ "bus": {{ soa: {{ primary: "gw.bus", mbox: "h.bus", ttl: 1, minimum: 1 }}, ns: [ "gw.bus" ], serial_floor: 1, bundles: [ {{ owner: "c", serial: 1, records: [ {{ name: "x.bus", type: "{bad}", ttl: 1, data: "gw.bus" }} ] }} ] }} }} }}"#
            );
            assert!(parse_zones(&src).is_err(), "{bad} must be rejected");
        }
    }
}
