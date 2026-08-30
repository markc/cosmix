//! Rule pack loader (`.conf.mix`).
//!
//! Reads a `default.conf.mix` (or operator-supplied) file, dispatches
//! each `rule` entry to the matching `RuleMatcher` constructor, and
//! returns a compiled `(RuleSet, Vec<(RuleId, error_string)>)`.
//!
//! Compile errors are collected per-rule rather than aborting the
//! load — partial-reload semantics: well-formed rules in the same pack
//! continue to fire even if one regex is invalid.

use std::path::Path;

use serde::Deserialize;

use crate::compiled::{CompiledRule, RuleMatcher, RuleSet};
use crate::error::{Error, Result};
use crate::rules::{
    attachment_count::AttachmentCount,
    body_regex::BodyRegex,
    body_substring::BodySubstring,
    header_absent::HeaderAbsent,
    header_present::HeaderPresent,
    header_regex::HeaderRegex,
    header_substring::HeaderSubstring,
    mail_auth::{DkimMatch, DmarcMatch, MailAuth, SpfMatch},
    peer_ip_in_cidr::{Cidr, PeerIpInCidr},
    recipient_count::RecipientCount,
    structure::{Structure, StructureCheck},
    url_count::UrlCount,
};
use crate::types::{MatchView, RuleId};

/// Top-level shape of a rule-pack file.
#[derive(Debug, Deserialize)]
struct PackFile {
    pack_version: String,
    #[serde(default)]
    rule: Vec<RuleSpec>,
}

/// One `rule` entry in the pack.
#[derive(Debug, Deserialize)]
struct RuleSpec {
    id: String,
    description: String,
    weight: i32,
    #[serde(rename = "match")]
    match_: MatchSpec,
}

/// Inner `match` table — superset across all rule kinds. Per-kind
/// dispatch happens in `compile_matcher`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct MatchSpec {
    kind: String,
    name: Option<String>,
    regex: Option<String>,
    substring: Option<String>,
    spf: Option<String>,
    dkim: Option<String>,
    dmarc: Option<String>,
    gt: Option<u32>,
    cidrs: Option<Vec<String>>,
    view: Option<MatchView>,
    html_only: Option<bool>,
    has_executable_attachment: Option<bool>,
    deep_multipart_nesting: Option<bool>,
    missing_message_id: Option<bool>,
    executable_only: Option<bool>,
    what: Option<String>,
    zones: Option<Vec<String>>,
}

/// Parse a `.conf.mix` rule-pack string and compile it. Returns the
/// compiled `RuleSet` and the (possibly empty) list of per-rule failures.
pub(crate) fn load_pack_str(conf_mix_text: &str) -> Result<(RuleSet, Vec<(RuleId, String)>)> {
    let pack: PackFile = cosmix_mix::from_conf_mix_str(conf_mix_text)
        .map_err(|e| Error::PackParse(e.to_string()))?;
    Ok(compile_pack(pack))
}

/// Read a `.conf.mix` rule pack from disk and compile it. C11 removed
/// the legacy `.toml` operator-override fallback.
pub(crate) fn load_pack_file(path: &Path) -> Result<(RuleSet, Vec<(RuleId, String)>)> {
    let text = std::fs::read_to_string(path).map_err(|e| Error::PackIo(e.to_string()))?;
    load_pack_str(&text)
}

/// Compile a parsed `PackFile` into a `RuleSet`, collecting per-rule
/// compile failures rather than aborting (partial-reload semantics).
fn compile_pack(pack: PackFile) -> (RuleSet, Vec<(RuleId, String)>) {
    let mut rules = Vec::with_capacity(pack.rule.len());
    let mut failed = Vec::new();

    for spec in pack.rule {
        match compile_matcher(&spec) {
            Ok(matcher) => rules.push(CompiledRule {
                id: spec.id,
                description: spec.description,
                weight: spec.weight,
                matcher,
            }),
            Err(e) => failed.push((spec.id, e.to_string())),
        }
    }

    (
        RuleSet {
            pack_version: pack.pack_version,
            rules,
        },
        failed,
    )
}

fn compile_matcher(spec: &RuleSpec) -> Result<RuleMatcher> {
    let m = &spec.match_;
    match m.kind.as_str() {
        "header_present" => Ok(RuleMatcher::HeaderPresent(HeaderPresent {
            header_name: required(spec, &m.name, "name")?.clone(),
        })),
        "header_absent" => Ok(RuleMatcher::HeaderAbsent(HeaderAbsent {
            header_name: required(spec, &m.name, "name")?.clone(),
        })),
        "header_regex" => {
            let pattern = compile_regex(&spec.id, required(spec, &m.regex, "regex")?)?;
            Ok(RuleMatcher::HeaderRegex(HeaderRegex {
                header_name: required(spec, &m.name, "name")?.clone(),
                pattern,
            }))
        }
        "header_substring" => Ok(RuleMatcher::HeaderSubstring(HeaderSubstring {
            header_name: required(spec, &m.name, "name")?.clone(),
            needle: required(spec, &m.substring, "substring")?.clone(),
        })),
        "body_regex" => {
            let pattern = compile_regex(&spec.id, required(spec, &m.regex, "regex")?)?;
            Ok(RuleMatcher::BodyRegex(BodyRegex {
                pattern,
                view: m.view.unwrap_or_default(),
            }))
        }
        "body_substring" => Ok(RuleMatcher::BodySubstring(BodySubstring {
            needle: required(spec, &m.substring, "substring")?.clone(),
            view: m.view.unwrap_or_default(),
        })),
        "mail_auth" => {
            let spf = m
                .spf
                .as_deref()
                .map(parse_spf)
                .transpose()
                .map_err(|e| Error::InvalidRule(format!("{}: {e}", spec.id)))?;
            let dkim = m
                .dkim
                .as_deref()
                .map(parse_dkim)
                .transpose()
                .map_err(|e| Error::InvalidRule(format!("{}: {e}", spec.id)))?;
            let dmarc = m
                .dmarc
                .as_deref()
                .map(parse_dmarc)
                .transpose()
                .map_err(|e| Error::InvalidRule(format!("{}: {e}", spec.id)))?;
            if spf.is_none() && dkim.is_none() && dmarc.is_none() {
                return Err(Error::MissingField {
                    id: spec.id.clone(),
                    field: "spf|dkim|dmarc".into(),
                });
            }
            Ok(RuleMatcher::MailAuth(MailAuth { spf, dkim, dmarc }))
        }
        "peer_ip_in_cidr" => {
            let raw = required(spec, &m.cidrs, "cidrs")?;
            let mut cidrs = Vec::with_capacity(raw.len());
            for c in raw {
                let parsed = Cidr::parse(c)
                    .ok_or_else(|| Error::InvalidRule(format!("{}: bad CIDR {:?}", spec.id, c)))?;
                cidrs.push(parsed);
            }
            Ok(RuleMatcher::PeerIpInCidr(PeerIpInCidr { cidrs }))
        }
        "url_count" => Ok(RuleMatcher::UrlCount(UrlCount {
            gt: required_u32(spec, m.gt, "gt")?,
        })),
        "attachment_count" => Ok(RuleMatcher::AttachmentCount(AttachmentCount {
            gt: required_u32(spec, m.gt, "gt")?,
            executable_only: m.executable_only.unwrap_or(false),
        })),
        "recipient_count" => Ok(RuleMatcher::RecipientCount(RecipientCount {
            gt: required_u32(spec, m.gt, "gt")?,
        })),
        "structure" => {
            let check = if m.html_only == Some(true) {
                StructureCheck::HtmlOnlyNoAlternative
            } else if m.deep_multipart_nesting == Some(true) {
                StructureCheck::DeepMultipartNesting
            } else if m.missing_message_id == Some(true) {
                StructureCheck::MissingMessageId
            } else if m.has_executable_attachment == Some(true) {
                StructureCheck::HasExecutableAttachment
            } else {
                return Err(Error::MissingField {
                    id: spec.id.clone(),
                    field: "html_only|deep_multipart_nesting|missing_message_id|has_executable_attachment".into(),
                });
            };
            Ok(RuleMatcher::Structure(Structure { check }))
        }
        // DNSBL — live as of Phase 2 commit 2b. Verdict comes from the
        // engine's async preflight pass (see `crate::preflight`); the
        // matcher itself is sync and consults the pre-resolved result.
        "peer_ip_in_dnsbl" => Ok(RuleMatcher::PeerIpInDnsbl(
            crate::rules::peer_ip_in_dnsbl::PeerIpInDnsbl {
                zones: m.zones.clone().unwrap_or_default(),
            },
        )),
        // Reserved kind — accepted at load time but always no-match
        // in v1. The payload is parsed and held until v1.1 wires the
        // matcher.
        "alignment" => Ok(RuleMatcher::Alignment(crate::rules::alignment::Alignment {
            mode: parse_alignment(m.what.as_deref().unwrap_or("either")),
        })),
        other => Err(Error::UnknownKind {
            id: spec.id.clone(),
            kind: other.to_string(),
        }),
    }
}

fn required<'a, T>(spec: &RuleSpec, opt: &'a Option<T>, field: &str) -> Result<&'a T> {
    opt.as_ref().ok_or_else(|| Error::MissingField {
        id: spec.id.clone(),
        field: field.into(),
    })
}

fn required_u32(spec: &RuleSpec, opt: Option<u32>, field: &str) -> Result<u32> {
    opt.ok_or_else(|| Error::MissingField {
        id: spec.id.clone(),
        field: field.into(),
    })
}

fn compile_regex(id: &str, pattern: &str) -> Result<regex::Regex> {
    regex::Regex::new(pattern).map_err(|e| Error::RegexCompile {
        id: id.to_string(),
        source: e,
    })
}

fn parse_spf(s: &str) -> std::result::Result<SpfMatch, String> {
    Ok(match s.to_ascii_lowercase().as_str() {
        "pass" => SpfMatch::Pass,
        "fail" => SpfMatch::Fail,
        "softfail" => SpfMatch::SoftFail,
        "neutral" => SpfMatch::Neutral,
        "none" => SpfMatch::None,
        "temperror" => SpfMatch::TempError,
        "permerror" => SpfMatch::PermError,
        other => return Err(format!("bad spf value {other:?}")),
    })
}

fn parse_dkim(s: &str) -> std::result::Result<DkimMatch, String> {
    Ok(match s.to_ascii_lowercase().as_str() {
        "pass" => DkimMatch::Pass,
        "fail" => DkimMatch::Fail,
        "none" => DkimMatch::None,
        "temperror" => DkimMatch::TempError,
        "permerror" => DkimMatch::PermError,
        other => return Err(format!("bad dkim value {other:?}")),
    })
}

fn parse_dmarc(s: &str) -> std::result::Result<DmarcMatch, String> {
    Ok(match s.to_ascii_lowercase().as_str() {
        "pass" => DmarcMatch::Pass,
        "fail" => DmarcMatch::Fail,
        "none" => DmarcMatch::None,
        other => return Err(format!("bad dmarc value {other:?}")),
    })
}

fn parse_alignment(s: &str) -> crate::rules::alignment::AlignmentMode {
    match s.to_ascii_lowercase().as_str() {
        "spf" => crate::rules::alignment::AlignmentMode::Spf,
        "dkim" => crate::rules::alignment::AlignmentMode::Dkim,
        _ => crate::rules::alignment::AlignmentMode::Either,
    }
}
