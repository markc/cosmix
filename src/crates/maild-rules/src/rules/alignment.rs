// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright 2026 Mark Constable <mc@mixos.dev>

//! `alignment` — checks alignment between `From:` and SPF/DKIM
//! identities (DMARC-style alignment beyond what `maild-auth`
//! already records). Phase 1 reads this off `VerifyResult` rather
//! than re-parsing.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlignmentMode {
    Spf,
    Dkim,
    Either,
}

#[derive(Debug, Clone)]
pub struct Alignment {
    pub mode: AlignmentMode,
}
