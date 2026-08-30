//! Inbound verification surface — DNS-coupled implementation.
//!
//! The pipeline is intentionally serial (with timeouts), not parallel:
//! DMARC depends on SPF + DKIM outputs, ARC reads the same parsed
//! `AuthenticatedMessage`, and the cost is dominated by one or two
//! TXT/MX/PTR round-trips per check. Going parallel buys little wall-
//! time and complicates timeout accounting.
//!
//! Every external lookup is wrapped in `tokio::time::timeout` so that
//! a slow or malicious authoritative server cannot hold the SMTP
//! transaction past the per-check budget. On timeout we report the
//! check as `TempError` (per RFC convention) and continue.
//!
//! The composer uses `mail_auth::AuthenticationResults` for the wire-
//! format string, while we keep parallel structured fields on
//! `AuthResultsHeader` so callers (cosmix-maild, JMAP) can route on
//! verdicts without reparsing the rendered string.

use crate::ar_trim::trim_forged_authentication_results;
use crate::error::{Error, Result};
use crate::mapping::{map_arc, map_dkim, map_dmarc, map_iprev, map_spf_result, naive_org_domain};
use crate::resolver::DnsResolver;
use crate::types::*;
use mail_auth::{
    AuthenticatedMessage, AuthenticationResults, MessageAuthenticator, SpfOutput,
    SpfResult as MaSpfResult, dmarc::verify::DmarcParameters, spf::verify::SpfParameters,
};
use std::net::IpAddr;
use std::sync::Arc;
use tokio::time::timeout;

#[allow(async_fn_in_trait)]
pub trait Verifier: Send + Sync {
    /// Run all five checks against an inbound message at DATA time.
    /// `message` is the raw RFC 5322 byte stream including headers
    /// and CRLFs; the verifier strips any forged
    /// `Authentication-Results: <our-host-identity>;` lines before
    /// parsing or composing the new header (RFC 8601 §5).
    async fn verify(
        &self,
        peer_ip: IpAddr,
        envelope_from: &str,
        helo: &str,
        message: &[u8],
    ) -> Result<VerifyResult>;

    /// Pre-DATA SPF check; lets cosmix-smtp reject obvious abuse at
    /// MAIL FROM time without buffering the body. Empty
    /// `envelope_from` falls back to the HELO identity per RFC 7208
    /// §2.4 and is reported as `SpfCheck::Helo`.
    async fn verify_spf_only(
        &self,
        peer_ip: IpAddr,
        envelope_from: &str,
        helo: &str,
    ) -> Result<SpfCheck>;
}

pub struct MailAuthVerifier {
    resolver: Arc<DnsResolver>,
    config: VerifierConfig,
}

impl MailAuthVerifier {
    pub fn new(resolver: Arc<DnsResolver>, config: VerifierConfig) -> Self {
        Self { resolver, config }
    }

    fn auth(&self) -> &MessageAuthenticator {
        self.resolver.auth()
    }

    /// Run an SPF check, branching on whether MAIL FROM is empty (per
    /// RFC 7208 §2.4 the HELO identity becomes the SPF identity in
    /// that case).
    async fn run_spf(&self, peer_ip: IpAddr, envelope_from: &str, helo: &str) -> SpfBranch {
        let host_identity = self.config.host_identity.as_str();
        let auth = self.auth();

        if envelope_from.is_empty() {
            // HELO branch
            let params = SpfParameters::verify_ehlo(peer_ip, helo, host_identity);
            let fut = auth.verify_spf(params);
            let out = match timeout(self.config.spf_timeout, fut).await {
                Ok(out) => out,
                Err(_) => SpfOutput::new(helo.to_string()).with_result(MaSpfResult::TempError),
            };
            SpfBranch::Helo {
                domain: helo.to_string(),
                output: out,
            }
        } else {
            // MAIL FROM branch — check the MAIL FROM identity ONLY.
            //
            // Deliberately NOT `SpfParameters::verify()`: that is
            // mail-auth's "both identities" helper, and it checks HELO
            // *first*, returning the HELO result unless HELO passes —
            // MAIL FROM is only evaluated when HELO already passed.
            // Since most MTAs publish no SPF record at their HELO
            // hostname, that yields `none` (or the HELO domain's
            // `fail`) for virtually every sender, which we would then
            // mislabel as `smtp.mailfrom=`. Observed 2026-08-10:
            // outlook.com mail via HELO
            // mail-japanwestazolkn190120001.outbound.protection.outlook.com
            // reported spf=none, while the MAIL FROM identity is a
            // clean pass via include:spf2.outlook.com.
            //
            // This output also feeds DMARC's SPF-alignment leg, so the
            // wrong identity here silently costs DMARC passes for any
            // sender without a valid aligned DKIM signature.
            let params =
                SpfParameters::verify_mail_from(peer_ip, helo, host_identity, envelope_from);
            let fut = auth.verify_spf(params);
            let out = match timeout(self.config.spf_timeout, fut).await {
                Ok(out) => out,
                Err(_) => SpfOutput::new(extract_domain(envelope_from).to_string())
                    .with_result(MaSpfResult::TempError),
            };
            SpfBranch::MailFrom {
                domain: extract_domain(envelope_from).to_string(),
                output: out,
            }
        }
    }
}

enum SpfBranch {
    MailFrom { domain: String, output: SpfOutput },
    Helo { domain: String, output: SpfOutput },
}

impl SpfBranch {
    fn output(&self) -> &SpfOutput {
        match self {
            SpfBranch::MailFrom { output, .. } | SpfBranch::Helo { output, .. } => output,
        }
    }
    fn to_check(&self) -> SpfCheck {
        match self {
            SpfBranch::MailFrom { domain, output } => SpfCheck::MailFrom {
                result: map_spf_result(output.result()),
                domain: domain.clone(),
            },
            SpfBranch::Helo { domain, output } => SpfCheck::Helo {
                result: map_spf_result(output.result()),
                domain: domain.clone(),
            },
        }
    }
    fn to_fields(&self) -> SpfFields {
        match self {
            SpfBranch::MailFrom { domain, output } => SpfFields {
                result: map_spf_result(output.result()),
                identity: domain.clone(),
                identity_kind: SpfIdentityKind::MailFrom,
            },
            SpfBranch::Helo { domain, output } => SpfFields {
                result: map_spf_result(output.result()),
                identity: domain.clone(),
                identity_kind: SpfIdentityKind::Helo,
            },
        }
    }
}

fn extract_domain(addr: &str) -> &str {
    addr.rsplit_once('@').map(|(_, d)| d).unwrap_or(addr)
}

impl Verifier for MailAuthVerifier {
    async fn verify(
        &self,
        peer_ip: IpAddr,
        envelope_from: &str,
        helo: &str,
        message: &[u8],
    ) -> Result<VerifyResult> {
        let host_identity = self.config.host_identity.clone();

        // 1. Strip forged Authentication-Results headers claiming our
        //    identity (RFC 8601 §5). Done first so subsequent ARC
        //    parsing sees only authentic A-R lines.
        let (trimmed, _removed) = trim_forged_authentication_results(message, &host_identity);

        // 2. iprev — independent of message body.
        let iprev_out =
            match timeout(self.config.iprev_timeout, self.auth().verify_iprev(peer_ip)).await {
                Ok(out) => out,
                Err(_) => return Err(Error::DnsTimeout("iprev".into())),
            };
        let iprev_mapped = map_iprev(&iprev_out, peer_ip);

        // 3. Parse the message for DKIM / ARC headers and From: domain.
        let parsed = AuthenticatedMessage::parse(&trimmed)
            .ok_or_else(|| Error::Internal("AuthenticatedMessage::parse failed".into()))?;

        // 4. DKIM
        let dkim_outputs =
            match timeout(self.config.dkim_timeout, self.auth().verify_dkim(&parsed)).await {
                Ok(out) => out,
                Err(_) => return Err(Error::DnsTimeout("dkim".into())),
            };
        let dkim_mapped = map_dkim(&dkim_outputs, self.config.max_dkim_signatures);

        // 5. SPF
        let spf_branch = self.run_spf(peer_ip, envelope_from, helo).await;

        // 6. DMARC — depends on parsed message, dkim outputs, SPF
        //    output, and an org-domain function. Phase 1 ships
        //    `naive_org_domain`; PSL replacement is a Phase 2 task.
        let envelope_domain = extract_domain(envelope_from).to_string();
        let dmarc_params: DmarcParameters<'_, fn(&str) -> &str> = DmarcParameters {
            message: &parsed,
            dkim_output: &dkim_outputs,
            rfc5321_mail_from_domain: &envelope_domain,
            spf_output: spf_branch.output(),
            domain_suffix_fn: naive_org_domain as fn(&str) -> &str,
        };
        let dmarc_out = match timeout(
            self.config.dmarc_timeout,
            self.auth().verify_dmarc(dmarc_params),
        )
        .await
        {
            Ok(out) => out,
            Err(_) => return Err(Error::DnsTimeout("dmarc".into())),
        };
        let org_domain = naive_org_domain(dmarc_out.domain()).to_string();
        let dmarc_mapped = map_dmarc(&dmarc_out, peer_ip, org_domain);

        // 7. ARC
        let arc_out = match timeout(self.config.arc_timeout, self.auth().verify_arc(&parsed)).await
        {
            Ok(out) => out,
            Err(_) => return Err(Error::DnsTimeout("arc".into())),
        };
        let arc_mapped = map_arc(&arc_out, self.config.max_arc_instances);

        // 8. Compose Authentication-Results header. `mail_auth`
        //    renders the wire-format string; we capture structured
        //    field views so consumers don't reparse.
        let header_from_for_dkim = parsed.from.first().cloned().unwrap_or_default();
        let mut ar = AuthenticationResults::new(&host_identity)
            .with_iprev_result(&iprev_out, peer_ip)
            .with_dkim_results(&dkim_outputs, &header_from_for_dkim);
        ar = match &spf_branch {
            SpfBranch::MailFrom { output, .. } => {
                ar.with_spf_mailfrom_result(output, peer_ip, envelope_from, helo)
            }
            SpfBranch::Helo { output, .. } => ar.with_spf_ehlo_result(output, peer_ip, helo),
        };
        ar = ar
            .with_arc_result(&arc_out, peer_ip)
            .with_dmarc_result(&dmarc_out);
        let rendered = format!("Authentication-Results: {ar}\r\n");

        let dkim_fields: Vec<DkimFields> = dkim_mapped
            .signatures
            .iter()
            .map(|s| DkimFields {
                result: s.result,
                d: s.d.clone(),
                s: s.s.clone(),
            })
            .collect();

        let dmarc_fields = Some(DmarcFields {
            result: dmarc_mapped.outcome.clone(),
            header_from: dmarc_out.domain().to_string(),
        });

        let arc_fields = Some(ArcFields {
            result: arc_mapped.chain_validation,
            smtp_client_ip: Some(peer_ip),
        });

        let iprev_fields = Some(IprevFields {
            result: iprev_mapped.result,
            policy_iprev: Some(peer_ip),
        });

        let header = AuthResultsHeader {
            host_identity: host_identity.clone(),
            rendered,
            spf: Some(spf_branch.to_fields()),
            iprev: iprev_fields,
            dkim: dkim_fields,
            dmarc: dmarc_fields,
            arc: arc_fields,
        };

        Ok(VerifyResult {
            spf: spf_branch.to_check(),
            iprev: iprev_mapped,
            dkim: dkim_mapped,
            dmarc: dmarc_mapped,
            arc: arc_mapped,
            authentication_results_header: header,
        })
    }

    async fn verify_spf_only(
        &self,
        peer_ip: IpAddr,
        envelope_from: &str,
        helo: &str,
    ) -> Result<SpfCheck> {
        let branch = self.run_spf(peer_ip, envelope_from, helo).await;
        Ok(branch.to_check())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_domain_from_address() {
        assert_eq!(extract_domain("alice@example.org"), "example.org");
        assert_eq!(extract_domain("example.org"), "example.org");
        assert_eq!(extract_domain(""), "");
    }
}
