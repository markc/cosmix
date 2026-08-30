//! Vendored Let's Encrypt intermediate-cert allowlist. Each entry
//! pins an LE intermediate by SHA-256 of its SubjectPublicKeyInfo
//! (the form used by RFC 7469 / HPKP).
//!
//! The module ships two tables, partitioned by issuance
//! environment. The validator selects one of the two via
//! `pin_table_for(env)`; a chain whose intermediate's SPKI is in
//! the wrong table is rejected even if the SPKI bytes happen to
//! collide (see the global-uniqueness test below).
//!
//! ## Production — [`LE_INTERMEDIATE_PINS`]
//!
//! Source: <https://letsencrypt.org/certificates/>, fetched
//! 2026-05-20 (X1/X2 series) and 2026-06-06 (gen-y / YE-YR series).
//! Pins both LE issuing-intermediate generations:
//!   - the X1/X2-rooted "classic" intermediates (E7, E8, R12, R13
//!     active; E9, R14 backup), and
//!   - the Root YE / Root YR ("gen-y") intermediates
//!     (`/certs/gen-y/int-ye{1,2,3}` + `int-yr{1,2,3}`, all Active),
//!     which LE began serving by default for new ECDSA/RSA issuance
//!     in 2026.
//!
//! The gen-y intermediates were previously *omitted* on the
//! rationale that Root YE / Root YR are not in
//! `webpki_roots::TLS_SERVER_ROOTS`. That rationale was wrong for
//! the chains LE actually serves: the gen-y leaf chain is
//! `leaf → YE1 → Root YE → ISRG Root X2` (cross-signed down to a
//! root that IS a webpki anchor), so path validation succeeds — the
//! only blocker was the pin step, which (pre-2026-06-06) required
//! *every* non-leaf cert to be pinned, including the cross-sign
//! roots. `pin_intermediates` now requires only that the chain
//! contain at least one pinned LE issuing-intermediate (the
//! cross-sign roots are verified by `path_validate` against the
//! X1/X2 anchors), so pinning the YE/YR issuing-intermediates here
//! is sufficient and we do not pin Root YE / Root X2 / Root YR.
//!
//! ## Staging — [`LE_STAGING_INTERMEDIATE_PINS`]
//!
//! Source: <https://github.com/letsencrypt/website/tree/main/static/certs/staging>,
//! fetched 2026-05-21. Pins the X1/X2-rooted staging intermediates
//! (E5-E9 + R10-R14, all Active) used by the LE staging environment
//! a webd operator points at during `acme.provider = letsencrypt_staging`
//! cut-over. The YE/YR-rooted staging intermediates (YE1-3, YR1-3)
//! are documented on the LE staging page but not yet committed to
//! the letsencrypt/website repo, so we leave them out rather than
//! ship placeholder SPKIs; the deferred follow-up lands when LE
//! publishes the DERs.
//!
//! ## Refresh protocol
//!
//! When LE rotates either active set, fetch the new intermediate's
//! PEM from the corresponding source (`letsencrypt.org/certificates/`
//! for prod, `letsencrypt/website` for staging) and compute its
//! SPKI-SHA-256 via:
//!
//! ```text
//! openssl x509 -in <name>.pem -pubkey -noout \
//!   | openssl pkey -pubin -outform der \
//!   | openssl dgst -sha256 -binary \
//!   | xxd -p -c 999
//! ```
//!
//! Move the previous Active entry to Backup, add the new entry as
//! Active in the correct table, and record the refresh in
//! `_journal/<date>-le-intermediates-vN.md`. The per-table tests
//! below (uniqueness, environment partition, global SPKI disjointness)
//! catch a refresh that targets the wrong table.

/// Lifecycle state of a pinned LE intermediate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum IntermediateState {
    /// Currently issuing — new vhost cert provisioning expects to
    /// see this state in the leaf's path.
    Active,
    /// Issued in the past but not currently active. Existing
    /// unexpired chains using this intermediate continue to
    /// validate; new issuance does not produce chains rooted here.
    Backup,
    /// Intentional kill-switch. Any chain still using this
    /// intermediate at validation time is rejected, regardless of
    /// expiry. Reserved for historical-compromise responses.
    Retired,
}

/// Production vs staging environment marker.
///
/// Phase 1 shipped this as `Production`-only; Phase 2 adds the
/// `Staging` variant alongside the vendored LE-staging
/// trust-anchor set in
/// [`crate::tls::le_staging_roots::LE_STAGING_TRUST_ROOTS`] and
/// the matching [`LE_STAGING_INTERMEDIATE_PINS`] table.
///
/// The enum is closed (not `#[non_exhaustive]`) on purpose: every
/// `match Environment` site in the workspace is a deliberate
/// security checkpoint, and the umbrella's "Let's Encrypt only"
/// directive treats new environments (e.g. a hypothetical
/// `LetsEncryptProdEab`) as normative-spec changes rather than
/// quiet additions. The same closed-enum argument applies to
/// [`crate::tls::le_validator::ChainEnvironment`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Environment {
    Production,
    Staging,
}

/// One pinned LE intermediate certificate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct IntermediatePin {
    /// LE's published name for this intermediate (e.g. `"E8"`).
    pub friendly_name: &'static str,
    /// SHA-256 of the intermediate's SubjectPublicKeyInfo
    /// (RFC 7469 / HPKP pin form).
    pub spki_sha256: [u8; 32],
    /// Subject Key Identifier in hex (informational; lifted from
    /// `openssl x509 -ext subjectKeyIdentifier`).
    pub ski_hex: &'static str,
    pub state: IntermediateState,
    pub environment: Environment,
}

/// LE intermediate allowlist, fetched 2026-05-20 from
/// <https://letsencrypt.org/certificates/>. Order is informational
/// — `validate_le_chain` matches by `spki_sha256` and ignores
/// position.
pub static LE_INTERMEDIATE_PINS: &[IntermediatePin] = &[
    // ── Active (X2-rooted, ECDSA leaves) ────────────────────────
    IntermediatePin {
        friendly_name: "E7",
        spki_sha256: [
            0xcb, 0xbc, 0x55, 0x9b, 0x44, 0xd5, 0x24, 0xd6, 0xa1, 0x32, 0xbd, 0xac, 0x67, 0x27,
            0x44, 0xda, 0x34, 0x07, 0xf1, 0x2a, 0xae, 0x5d, 0x5f, 0x72, 0x2c, 0x5f, 0x6c, 0x79,
            0x13, 0x87, 0x1c, 0x75,
        ],
        ski_hex: "AE489EDC871D44A06FDAA2E5607404780C29C00080",
        state: IntermediateState::Active,
        environment: Environment::Production,
    },
    IntermediatePin {
        friendly_name: "E8",
        spki_sha256: [
            0x88, 0x5b, 0xf0, 0x57, 0x22, 0x52, 0xc6, 0x74, 0x1d, 0xc9, 0xa5, 0x2f, 0x50, 0x44,
            0x48, 0x7f, 0xef, 0x2a, 0x93, 0xb8, 0x11, 0xcd, 0xed, 0xfa, 0xd7, 0x62, 0x4c, 0xc2,
            0x83, 0xb7, 0xcd, 0xd5,
        ],
        ski_hex: "8F0D13A2F62E7ED1506C3318385D598E237291CA",
        state: IntermediateState::Active,
        environment: Environment::Production,
    },
    // ── Active (X1-rooted, RSA leaves) ──────────────────────────
    IntermediatePin {
        friendly_name: "R12",
        spki_sha256: [
            0x91, 0x9c, 0x0d, 0xf7, 0xa7, 0x87, 0xb5, 0x97, 0xed, 0x05, 0x6a, 0xce, 0x65, 0x4b,
            0x1d, 0xe9, 0xc0, 0x38, 0x7a, 0xcf, 0x34, 0x9f, 0x73, 0x73, 0x4a, 0x4f, 0xd7, 0xb5,
            0x8c, 0xf6, 0x12, 0xa4,
        ],
        ski_hex: "00B529F22D8E6F31E89B4CAD783EFADCE90CD1D2",
        state: IntermediateState::Active,
        environment: Environment::Production,
    },
    IntermediatePin {
        friendly_name: "R13",
        spki_sha256: [
            0x02, 0x54, 0x90, 0x86, 0x0b, 0x49, 0x8a, 0xb7, 0x3c, 0x6a, 0x12, 0xf2, 0x7a, 0x49,
            0xad, 0x5f, 0xe2, 0x30, 0xfa, 0xfe, 0x3a, 0xc8, 0xf6, 0x11, 0x2c, 0x9b, 0x7d, 0x0a,
            0xad, 0x46, 0x94, 0x1d,
        ],
        ski_hex: "E7AB9F0F2C33A053D35E4F78C8B2840E3BD69233",
        state: IntermediateState::Active,
        environment: Environment::Production,
    },
    // ── Backup ──────────────────────────────────────────────────
    IntermediatePin {
        friendly_name: "E9",
        spki_sha256: [
            0xf1, 0x44, 0x0a, 0x9b, 0x76, 0xe1, 0xe4, 0x1e, 0x53, 0xa4, 0xcb, 0x46, 0x13, 0x29,
            0xbf, 0x63, 0x37, 0xb4, 0x19, 0x72, 0x6b, 0xe5, 0x13, 0xe4, 0x2e, 0x19, 0xf1, 0xc6,
            0x91, 0xc5, 0xd4, 0xb2,
        ],
        ski_hex: "5D77D14DAC4D227859B286598E431CB764591341",
        state: IntermediateState::Backup,
        environment: Environment::Production,
    },
    IntermediatePin {
        friendly_name: "R14",
        spki_sha256: [
            0xf1, 0x64, 0x7a, 0x5e, 0xe3, 0xef, 0xac, 0x54, 0xc8, 0x92, 0xe9, 0x30, 0x58, 0x4f,
            0xe4, 0x79, 0x79, 0xb7, 0xac, 0xd1, 0xc7, 0x6c, 0x12, 0x71, 0xbc, 0xa1, 0xc5, 0x07,
            0x6d, 0x86, 0x98, 0x88,
        ],
        ski_hex: "E51095D8B60C51D7A77235630A54686E0653F8FB",
        state: IntermediateState::Backup,
        environment: Environment::Production,
    },
    // ── Active (gen-y, Root YE, ECDSA leaves) ───────────────────
    // LE's 2026 ECDSA issuing-intermediates. Chains reach a webpki
    // anchor via the cross-sign Root YE → ISRG Root X2 (verified by
    // path_validate); only these issuing-intermediates are pinned.
    IntermediatePin {
        friendly_name: "YE1",
        spki_sha256: [
            0x6e, 0xbc, 0xef, 0xb4, 0x21, 0x0b, 0x08, 0x86, 0x54, 0xa3, 0x8b, 0x03, 0xfe, 0xa3,
            0xd7, 0xd1, 0xc7, 0x11, 0xb4, 0xfb, 0x1d, 0xdc, 0x36, 0x3a, 0x45, 0xf9, 0xb1, 0xa4,
            0xe5, 0x3d, 0xa0, 0x1e,
        ],
        ski_hex: "BB20CA470BFED7E59CF98F092AA38C3745B1BCD8",
        state: IntermediateState::Active,
        environment: Environment::Production,
    },
    IntermediatePin {
        friendly_name: "YE2",
        spki_sha256: [
            0xb3, 0xfb, 0x5d, 0x00, 0xe9, 0x94, 0xcd, 0xdf, 0x2c, 0xc9, 0xa4, 0xee, 0xa9, 0xf8,
            0x06, 0xbc, 0x57, 0x27, 0xe8, 0x3c, 0xc0, 0xe4, 0x29, 0x9b, 0xf9, 0x56, 0xf2, 0xd5,
            0x24, 0xfe, 0x53, 0x76,
        ],
        ski_hex: "B959F28ECF22F086D33748FF761418BA82D85587",
        state: IntermediateState::Active,
        environment: Environment::Production,
    },
    IntermediatePin {
        friendly_name: "YE3",
        spki_sha256: [
            0xa6, 0x98, 0xa2, 0x08, 0x24, 0xbe, 0x04, 0xe4, 0x7a, 0x1a, 0x33, 0xc4, 0xfa, 0x48,
            0x87, 0x31, 0xbe, 0x92, 0x01, 0x1f, 0x23, 0xa3, 0x1e, 0x90, 0x0e, 0x2c, 0xa2, 0x6c,
            0x9c, 0x2a, 0xcf, 0xce,
        ],
        ski_hex: "F6A27ED65E236FDDDD4546611E821575CDF1E68E",
        state: IntermediateState::Active,
        environment: Environment::Production,
    },
    // ── Active (gen-y, Root YR, RSA leaves) ─────────────────────
    IntermediatePin {
        friendly_name: "YR1",
        spki_sha256: [
            0x2e, 0x83, 0x07, 0x06, 0x8b, 0x6d, 0xb6, 0x20, 0xe4, 0xa3, 0x9d, 0x06, 0x8b, 0x5d,
            0xee, 0x5d, 0x6e, 0xf5, 0x78, 0x8c, 0xbb, 0x2c, 0x0b, 0x6d, 0x23, 0xea, 0xd8, 0x4f,
            0xcc, 0x17, 0x17, 0x8c,
        ],
        ski_hex: "1F2F35BE461482CD40B1AE792C5578FAF7D468FB",
        state: IntermediateState::Active,
        environment: Environment::Production,
    },
    IntermediatePin {
        friendly_name: "YR2",
        spki_sha256: [
            0x9d, 0x63, 0x7b, 0x3d, 0x27, 0xa9, 0xe5, 0x70, 0xd0, 0x76, 0x07, 0xb9, 0xcc, 0xad,
            0xb8, 0x0a, 0x70, 0x91, 0x5c, 0x7a, 0xf7, 0x2a, 0xfc, 0xe1, 0x28, 0x41, 0xb1, 0xb1,
            0xda, 0x82, 0x5f, 0xd1,
        ],
        ski_hex: "40152D2679ED32209EDF9A721DD6321F810C810C",
        state: IntermediateState::Active,
        environment: Environment::Production,
    },
    IntermediatePin {
        friendly_name: "YR3",
        spki_sha256: [
            0x51, 0xaa, 0xa8, 0x7d, 0x98, 0x4b, 0x55, 0x9a, 0xc6, 0x9e, 0x92, 0x9f, 0x88, 0x8a,
            0x02, 0x2d, 0x83, 0x2e, 0x08, 0x9f, 0xf4, 0xdb, 0xa0, 0xa4, 0x12, 0xb5, 0x10, 0x1b,
            0xca, 0x4b, 0xc7, 0x99,
        ],
        ski_hex: "696529F933F809B642D5E1F3B5B59D1F5F7A227C",
        state: IntermediateState::Active,
        environment: Environment::Production,
    },
];

/// LE **staging** intermediate allowlist, fetched 2026-05-21 from
/// the [letsencrypt/website](https://github.com/letsencrypt/website/tree/main/static/certs/staging/2024)
/// repo and verified against
/// <https://letsencrypt.org/docs/staging-environment/>. The
/// X1/X2-rooted intermediates are pinned here; the YE/YR-rooted
/// intermediates (Artificial Amaranth YE1..3, Dastardly Durum
/// YR1..3) are documented on the staging-environment page but
/// **not** committed to the website repo, so they are not
/// pinnable from a static fetch. A staging chain that signs
/// through one of those will fail pin check until they are
/// published; the fix is a website-repo PR by LE plus a
/// follow-up vendor commit here.
///
/// Order is informational — `validate_le_chain_for_environment`
/// matches by `spki_sha256` and ignores position. All Active
/// initially; once a staging fixture chain is captured (Phase 2
/// commit 5's preflight) the issuing intermediate becomes the
/// "load-bearing" one and the rest can be moved to Backup if
/// LE's staging set rotates.
///
/// Refresh protocol: same as production — pull the new
/// intermediate's PEM under `static/certs/staging/...`, compute
/// SPKI-SHA-256 via
///
/// ```text
/// openssl x509 -in <name>.pem -pubkey -noout \
///   | openssl pkey -pubin -outform der \
///   | openssl dgst -sha256 -binary \
///   | xxd -p -c 999
/// ```
///
/// then add/replace the row here.
pub static LE_STAGING_INTERMEDIATE_PINS: &[IntermediatePin] = &[
    // ── Active (X2-rooted, ECDSA leaves) ────────────────────────
    IntermediatePin {
        friendly_name: "(STAGING) Pseudo Plum E5",
        spki_sha256: [
            0xa0, 0x3d, 0x32, 0xb4, 0x4f, 0xe2, 0xff, 0xf1, 0x06, 0x85, 0xea, 0x06, 0xdc, 0x34,
            0x58, 0x80, 0xa6, 0x8a, 0xcd, 0xef, 0xad, 0x2e, 0xb8, 0xf7, 0x05, 0x7f, 0x5a, 0x91,
            0x88, 0x6f, 0xad, 0x25,
        ],
        ski_hex: "FC46D101435FBB7BA63D3068AE11BAE0BC6DC9D3",
        state: IntermediateState::Active,
        environment: Environment::Staging,
    },
    IntermediatePin {
        friendly_name: "(STAGING) False Fennel E6",
        spki_sha256: [
            0x04, 0x6c, 0x79, 0x78, 0x42, 0xc7, 0xc5, 0x8b, 0x1d, 0xbd, 0x6d, 0xb8, 0x91, 0x11,
            0x71, 0xa6, 0x0f, 0xc6, 0xc9, 0xba, 0xf6, 0x16, 0x5b, 0x49, 0x83, 0x5b, 0x48, 0x26,
            0x6d, 0x95, 0x36, 0x70,
        ],
        ski_hex: "A1741A066D50B7862D4A2CC17EB48D88496CCD16",
        state: IntermediateState::Active,
        environment: Environment::Staging,
    },
    IntermediatePin {
        friendly_name: "(STAGING) Puzzling Parsnip E7",
        spki_sha256: [
            0xad, 0x0b, 0xc3, 0x00, 0xd7, 0xc5, 0xaa, 0x3a, 0xbd, 0x45, 0x34, 0x6b, 0xf1, 0xe5,
            0xfb, 0x97, 0xcc, 0xc6, 0x2f, 0x3b, 0x5c, 0x1e, 0x64, 0x7a, 0x52, 0xd9, 0x89, 0xc6,
            0xb0, 0xa0, 0x1e, 0xca,
        ],
        ski_hex: "A40F940B44636A99A9A0D98C6643B14FDCB02C46",
        state: IntermediateState::Active,
        environment: Environment::Staging,
    },
    IntermediatePin {
        friendly_name: "(STAGING) Mysterious Mulberry E8",
        spki_sha256: [
            0x47, 0x2f, 0xaa, 0xf0, 0xd1, 0xe0, 0xad, 0x36, 0x9e, 0x1c, 0xe6, 0x00, 0x1c, 0xa6,
            0xff, 0x9f, 0x65, 0x78, 0x03, 0x43, 0xfa, 0x62, 0xed, 0xfc, 0x4f, 0x3f, 0xd2, 0x24,
            0xd4, 0x39, 0x15, 0x38,
        ],
        ski_hex: "C941934248D18C170691F2F239D2A01FA7BBDB39",
        state: IntermediateState::Active,
        environment: Environment::Staging,
    },
    IntermediatePin {
        friendly_name: "(STAGING) Fake Fig E9",
        spki_sha256: [
            0x5d, 0xb1, 0xfe, 0x86, 0x41, 0x18, 0x94, 0x6d, 0x19, 0xf3, 0xe7, 0x1f, 0x81, 0xc8,
            0xcb, 0xe7, 0x54, 0xa6, 0xa7, 0x95, 0xcd, 0xa7, 0xfb, 0xb1, 0xfa, 0xf3, 0xb0, 0x8c,
            0x3c, 0xf4, 0x72, 0x43,
        ],
        ski_hex: "8EDDE5A8F7B0D9D1A571D4FF8A38F9D63B026D57",
        state: IntermediateState::Active,
        environment: Environment::Staging,
    },
    // ── Active (X1-rooted, RSA leaves) ──────────────────────────
    IntermediatePin {
        friendly_name: "(STAGING) Counterfeit Cashew R10",
        spki_sha256: [
            0xb3, 0xe4, 0x9a, 0xc8, 0xcb, 0x4a, 0xc4, 0xf9, 0x25, 0x3a, 0xd6, 0xdc, 0x0e, 0x7f,
            0x20, 0xd8, 0xd3, 0xca, 0xab, 0xd8, 0x17, 0x62, 0xd8, 0x28, 0x0e, 0xec, 0x22, 0x3c,
            0x82, 0xf6, 0xc9, 0x29,
        ],
        ski_hex: "A45246EA58A88F68D8B7B190D14A424A8F6B2871",
        state: IntermediateState::Active,
        environment: Environment::Staging,
    },
    IntermediatePin {
        friendly_name: "(STAGING) Wannabe Watercress R11",
        spki_sha256: [
            0xaa, 0x78, 0x26, 0x3a, 0x6b, 0x60, 0xeb, 0xc4, 0x90, 0x2d, 0x98, 0x0b, 0x54, 0x82,
            0xad, 0xaa, 0x56, 0x46, 0x4b, 0xee, 0x35, 0xf4, 0xa8, 0x9f, 0xfa, 0xab, 0xeb, 0xdf,
            0x64, 0x76, 0x05, 0xd2,
        ],
        ski_hex: "13CBD7F6AE9DFC694264D65C7C23CC85F947C7B6",
        state: IntermediateState::Active,
        environment: Environment::Staging,
    },
    IntermediatePin {
        friendly_name: "(STAGING) Riddling Rhubarb R12",
        spki_sha256: [
            0xd4, 0x29, 0x45, 0x65, 0x8c, 0x45, 0x70, 0x56, 0xe8, 0x92, 0xab, 0xd8, 0x31, 0xe8,
            0xa6, 0xcd, 0xc6, 0xa2, 0x19, 0x24, 0x2a, 0x30, 0x96, 0x96, 0x36, 0x9f, 0x4a, 0xe5,
            0xd9, 0xdc, 0xd5, 0x32,
        ],
        ski_hex: "F64303912F6625AF8525DDE464D1697ECBA21CD9",
        state: IntermediateState::Active,
        environment: Environment::Staging,
    },
    IntermediatePin {
        friendly_name: "(STAGING) Tenuous Tomato R13",
        spki_sha256: [
            0x6e, 0x2f, 0x2a, 0xad, 0xbb, 0x58, 0xbf, 0xed, 0x7f, 0xe0, 0x67, 0xb3, 0xdd, 0xec,
            0xad, 0xb1, 0xab, 0xd0, 0xe7, 0x96, 0xd3, 0xa1, 0xe3, 0x04, 0x0e, 0x79, 0x55, 0xd3,
            0x98, 0xd3, 0x4e, 0x6c,
        ],
        ski_hex: "3E348FCA29732B105819A1EBF8BBA9C45E64E6D1",
        state: IntermediateState::Active,
        environment: Environment::Staging,
    },
    IntermediatePin {
        friendly_name: "(STAGING) Not Nectarine R14",
        spki_sha256: [
            0xa0, 0xa0, 0xce, 0xd2, 0x37, 0x2f, 0xe3, 0xf4, 0xd9, 0x8e, 0xb4, 0x3d, 0xe9, 0xc8,
            0x15, 0xb5, 0xaf, 0xa1, 0xb0, 0x74, 0x51, 0x2b, 0x98, 0xb9, 0xf9, 0x43, 0x14, 0x88,
            0x33, 0xe1, 0xe1, 0xcf,
        ],
        ski_hex: "4DD33662689505F964120EFCDDE02C1F812BACF6",
        state: IntermediateState::Active,
        environment: Environment::Staging,
    },
];

#[cfg(test)]
mod tests {
    use super::*;

    fn pin_tables() -> [(&'static str, &'static [IntermediatePin], Environment); 2] {
        [
            ("production", LE_INTERMEDIATE_PINS, Environment::Production),
            (
                "staging",
                LE_STAGING_INTERMEDIATE_PINS,
                Environment::Staging,
            ),
        ]
    }

    #[test]
    fn pin_tables_non_empty() {
        for (name, table, _) in pin_tables() {
            assert!(!table.is_empty(), "{name} pin table must be non-empty");
        }
    }

    #[test]
    fn pin_tables_have_active_entries() {
        for (name, table, _) in pin_tables() {
            assert!(
                table.iter().any(|p| p.state == IntermediateState::Active),
                "{name} pin table must contain at least one Active entry"
            );
        }
    }

    #[test]
    fn pin_tables_ski_lengths_match_format() {
        // SKIs are 20 hex bytes = 40 hex chars (SHA-1 KeyIdentifier),
        // optionally with a leading hex pair from a malformed openssl run.
        for (_, table, _) in pin_tables() {
            for p in table {
                assert!(
                    p.ski_hex.len() >= 40,
                    "SKI for {} too short: {:?}",
                    p.friendly_name,
                    p.ski_hex
                );
                assert!(
                    p.ski_hex.chars().all(|c| c.is_ascii_hexdigit()),
                    "SKI for {} contains non-hex chars: {:?}",
                    p.friendly_name,
                    p.ski_hex
                );
            }
        }
    }

    #[test]
    fn pin_tables_spki_unique_globally() {
        // Stronger than per-table uniqueness: every SPKI hash across
        // *every* pin table must be unique. A future refresh that
        // accidentally copies a prod pin into the staging table (or
        // vice versa) would otherwise leave the partition invariant
        // `pin_table_for(env)` exposes load-bearing, but the
        // pin_intermediates path only consults one table at a time
        // and would silently accept a "matching" intermediate in the
        // wrong env. Asserting global SPKI disjointness closes that
        // gap at table-definition time rather than relying on the
        // validator wiring to stay correct.
        use std::collections::HashMap;
        let mut seen: HashMap<[u8; 32], (&'static str, &'static str)> = HashMap::new();
        for (table_name, table, _) in pin_tables() {
            for p in table {
                if let Some((prev_table, prev_friendly)) =
                    seen.insert(p.spki_sha256, (table_name, p.friendly_name))
                {
                    panic!(
                        "SPKI pin collision: {table_name}/{} and {prev_table}/{prev_friendly} \
                         share the same SPKI-SHA-256 — the prod/staging pin partition \
                         requires globally-unique key material",
                        p.friendly_name,
                    );
                }
            }
        }
    }

    #[test]
    fn pin_tables_environment_matches_table_origin() {
        // Every row in the production table is `Environment::Production`,
        // every row in the staging table is `Environment::Staging`.
        // A row with the wrong environment marker would slip through
        // `pin_table_for(env)` and accept the wrong chain class — a
        // latent security regression. This test pins the invariant.
        for (name, table, expected_env) in pin_tables() {
            for p in table {
                assert_eq!(
                    p.environment, expected_env,
                    "{name} pin {:?} has environment {:?}, expected {:?}",
                    p.friendly_name, p.environment, expected_env,
                );
            }
        }
    }
}
