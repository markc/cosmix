//! Merge single-preset SF2 images into one multi-preset bank.
//!
//! The inverse of [`crate::sf2split`]: `split` gives Ardour one tiny font per
//! track, but musicd's whole-song `render` loads ONE font — so a mix-and-match
//! orchestra (piano from bank A, strings from bank B, a retagged third-party
//! bass) needs those picks combined back into a single .sf2. The CLI extracts
//! each take with the splitter (retag included), then this module concatenates
//! the resulting single-preset images.
//!
//! Combining is pure index arithmetic because every input is a complete,
//! self-consistent font: copy each input's REAL hydra records (terminals
//! dropped), shift every cross-array index by the records already emitted,
//! shift sample offsets by the sample words already pooled, and append one
//! fresh set of terminal records at the end. Sample pools carry their 46-word
//! guards internally, so pool concatenation is byte append. `sm24` merges only
//! when EVERY input has it (a mixed pool would silently degrade 24-bit inputs;
//! each input contributes exactly `smpl/2` low bytes — its parity pad byte, if
//! any, is NOT sample data and is re-derived once for the merged chunk).

use anyhow::{Context, Result, bail};

use crate::riff::{Chunk, write_chunk};
use crate::sf2split::SplitOutput;

// Record geometry shared with the splitter (SF2 spec §7).
const PHDR_LEN: usize = 38;
const BAG_LEN: usize = 4;
const MOD_LEN: usize = 10;
const GEN_LEN: usize = 4;
const INST_LEN: usize = 22;
const SHDR_LEN: usize = 46;

const PHDR_BAG_NDX: usize = 24;
const INST_BAG_NDX: usize = 20;
const SHDR_LINK: usize = 42;
const SHDR_TYPE: usize = 44;
const GEN_INSTRUMENT: u16 = 41;
const GEN_SAMPLE_ID: u16 = 53;
const TYPE_LINK_BITS: u16 = 2 | 4 | 8;

fn u16_at(buf: &[u8], off: usize) -> u16 {
    u16::from_le_bytes(buf[off..off + 2].try_into().unwrap())
}

fn u32_at(buf: &[u8], off: usize) -> u32 {
    u32::from_le_bytes(buf[off..off + 4].try_into().unwrap())
}

fn put_u16(buf: &mut [u8], off: usize, v: u16) {
    buf[off..off + 2].copy_from_slice(&v.to_le_bytes());
}

fn bump_u16(buf: &mut [u8], off: usize, by: u16) -> Result<()> {
    let v = u16_at(buf, off)
        .checked_add(by)
        .ok_or_else(|| anyhow::anyhow!("merged hydra index exceeds u16"))?;
    put_u16(buf, off, v);
    Ok(())
}

fn bump_u32(buf: &mut [u8], off: usize, by: u32) -> Result<()> {
    let v = u32_at(buf, off)
        .checked_add(by)
        .ok_or_else(|| anyhow::anyhow!("merged sample offset exceeds u32"))?;
    buf[off..off + 4].copy_from_slice(&v.to_le_bytes());
    Ok(())
}

/// One parsed input image: the raw hydra + pool slices we append from.
struct Part {
    smpl: Vec<u8>,
    sm24: Option<Vec<u8>>,
    phdr: Vec<u8>,
    pbag: Vec<u8>,
    pmod: Vec<u8>,
    pgen: Vec<u8>,
    inst: Vec<u8>,
    ibag: Vec<u8>,
    imod: Vec<u8>,
    igen: Vec<u8>,
    shdr: Vec<u8>,
    info: Option<Chunk>,
}

fn leaf_of(children: &[Chunk], form: &[u8; 4], id: &[u8; 4]) -> Option<Vec<u8>> {
    crate::riff::find_leaf(children, form, id).map(|d| d.to_vec())
}

fn parse_part(bytes: &[u8]) -> Result<Part> {
    let roots = crate::riff::parse_chunks(bytes)?;
    let Some(Chunk::List { id, form, children }) = roots.first() else {
        bail!("not a RIFF container");
    };
    if id != b"RIFF" || form != b"sfbk" {
        bail!("not an sfbk SoundFont");
    }
    let need = |id: &[u8; 4], rec: usize| -> Result<Vec<u8>> {
        let d = leaf_of(children, b"pdta", id)
            .with_context(|| format!("missing pdta '{}'", String::from_utf8_lossy(id)))?;
        if d.len() < rec || d.len() % rec != 0 {
            bail!(
                "pdta '{}' bad length {}",
                String::from_utf8_lossy(id),
                d.len()
            );
        }
        Ok(d)
    };
    Ok(Part {
        smpl: leaf_of(children, b"sdta", b"smpl").context("input has no smpl")?,
        sm24: leaf_of(children, b"sdta", b"sm24"),
        phdr: need(b"phdr", PHDR_LEN)?,
        pbag: need(b"pbag", BAG_LEN)?,
        pmod: need(b"pmod", MOD_LEN)?,
        pgen: need(b"pgen", GEN_LEN)?,
        inst: need(b"inst", INST_LEN)?,
        ibag: need(b"ibag", BAG_LEN)?,
        imod: need(b"imod", MOD_LEN)?,
        igen: need(b"igen", GEN_LEN)?,
        shdr: need(b"shdr", SHDR_LEN)?,
        info: crate::riff::find_list(children, b"INFO").cloned(),
    })
}

/// Merge extractor outputs into one bank image named `inam`. Inputs keep
/// their (possibly retagged) preset identities; duplicates are refused.
pub fn merge_images(takes: &[SplitOutput], inam: &str) -> Result<Vec<u8>> {
    if takes.is_empty() {
        bail!("nothing to merge");
    }
    let mut seen = std::collections::BTreeSet::new();
    for t in takes {
        if !seen.insert((t.entry.bank, t.entry.program)) {
            bail!(
                "duplicate preset identity {}:{} ('{}') — retag one of the takes",
                t.entry.bank,
                t.entry.program,
                t.entry.name
            );
        }
    }
    let parts: Vec<Part> = takes
        .iter()
        .map(|t| parse_part(&t.sf2))
        .collect::<Result<_>>()
        .context("parsing take for merge")?;
    // Enforce the single-preset-input contract: exactly one real phdr record
    // whose identity matches the take's entry — otherwise the duplicate
    // check above (which trusts `entry`) could be bypassed.
    for (t, p) in takes.iter().zip(&parts) {
        if p.phdr.len() / PHDR_LEN != 2 {
            bail!(
                "take '{}' is not a single-preset image ({} presets)",
                t.entry.name,
                p.phdr.len() / PHDR_LEN - 1
            );
        }
        let (prog, bank) = (u16_at(&p.phdr, 20), u16_at(&p.phdr, 22));
        if (bank, prog) != (t.entry.bank, t.entry.program) {
            bail!(
                "take '{}' claims {}:{} but its phdr says {bank}:{prog}",
                t.entry.name,
                t.entry.bank,
                t.entry.program
            );
        }
    }

    // sm24 only survives when every input carries it.
    let all_24bit = parts.iter().all(|p| p.sm24.is_some());

    let mut smpl = Vec::new();
    let mut sm24 = Vec::new();
    let (mut phdr, mut pbag, mut pmod, mut pgen) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    let (mut inst, mut ibag, mut imod, mut igen) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    let mut shdr = Vec::new();

    for p in &parts {
        // Bases: records already emitted before this input. Hydra indices
        // are u16 in the format — merging past 65535 records in any array
        // must fail, not wrap.
        let base = |len: usize, rec: usize, what: &str| -> Result<u16> {
            u16::try_from(len / rec)
                .map_err(|_| anyhow::anyhow!("merged '{what}' exceeds 65535 records"))
        };
        let pbag_b = base(pbag.len(), BAG_LEN, "pbag")?;
        let pgen_b = base(pgen.len(), GEN_LEN, "pgen")?;
        let pmod_b = base(pmod.len(), MOD_LEN, "pmod")?;
        let inst_b = base(inst.len(), INST_LEN, "inst")?;
        let ibag_b = base(ibag.len(), BAG_LEN, "ibag")?;
        let igen_b = base(igen.len(), GEN_LEN, "igen")?;
        let imod_b = base(imod.len(), MOD_LEN, "imod")?;
        let shdr_b = base(shdr.len(), SHDR_LEN, "shdr")?;
        let pool_b = u32::try_from(smpl.len() / 2)
            .map_err(|_| anyhow::anyhow!("merged sample pool exceeds u32 words"))?;

        let reals = |buf: &[u8], rec: usize| buf.len() / rec - 1; // strip terminal

        for i in 0..reals(&p.phdr, PHDR_LEN) {
            let mut r = p.phdr[i * PHDR_LEN..(i + 1) * PHDR_LEN].to_vec();
            bump_u16(&mut r, PHDR_BAG_NDX, pbag_b)?;
            phdr.extend_from_slice(&r);
        }
        for i in 0..reals(&p.pbag, BAG_LEN) {
            let mut r = p.pbag[i * BAG_LEN..(i + 1) * BAG_LEN].to_vec();
            bump_u16(&mut r, 0, pgen_b)?;
            bump_u16(&mut r, 2, pmod_b)?;
            pbag.extend_from_slice(&r);
        }
        pmod.extend_from_slice(&p.pmod[..p.pmod.len() - MOD_LEN]);
        for i in 0..reals(&p.pgen, GEN_LEN) {
            let mut r = p.pgen[i * GEN_LEN..(i + 1) * GEN_LEN].to_vec();
            if u16_at(&r, 0) == GEN_INSTRUMENT {
                bump_u16(&mut r, 2, inst_b)?;
            }
            pgen.extend_from_slice(&r);
        }
        for i in 0..reals(&p.inst, INST_LEN) {
            let mut r = p.inst[i * INST_LEN..(i + 1) * INST_LEN].to_vec();
            bump_u16(&mut r, INST_BAG_NDX, ibag_b)?;
            inst.extend_from_slice(&r);
        }
        for i in 0..reals(&p.ibag, BAG_LEN) {
            let mut r = p.ibag[i * BAG_LEN..(i + 1) * BAG_LEN].to_vec();
            bump_u16(&mut r, 0, igen_b)?;
            bump_u16(&mut r, 2, imod_b)?;
            ibag.extend_from_slice(&r);
        }
        imod.extend_from_slice(&p.imod[..p.imod.len() - MOD_LEN]);
        for i in 0..reals(&p.igen, GEN_LEN) {
            let mut r = p.igen[i * GEN_LEN..(i + 1) * GEN_LEN].to_vec();
            if u16_at(&r, 0) == GEN_SAMPLE_ID {
                bump_u16(&mut r, 2, shdr_b)?;
            }
            igen.extend_from_slice(&r);
        }
        for i in 0..reals(&p.shdr, SHDR_LEN) {
            let mut r = p.shdr[i * SHDR_LEN..(i + 1) * SHDR_LEN].to_vec();
            for off in [20usize, 24, 28, 32] {
                bump_u32(&mut r, off, pool_b)?;
            }
            if u16_at(&r, SHDR_TYPE) & TYPE_LINK_BITS != 0 {
                bump_u16(&mut r, SHDR_LINK, shdr_b)?;
            }
            shdr.extend_from_slice(&r);
        }
        smpl.extend_from_slice(&p.smpl);
        if all_24bit {
            // Exactly one low byte per smpl word: an input's trailing parity
            // pad byte is not sample data and must not shift later inputs.
            let words = p.smpl.len() / 2;
            let s = p.sm24.as_ref().unwrap();
            if s.len() < words {
                bail!("input sm24 shorter than its sample pool");
            }
            sm24.extend_from_slice(&s[..words]);
        }
    }

    // One set of terminals for the merged arrays; final counts must still
    // fit the format's u16 indices.
    let count = |len: usize, rec: usize, what: &str| -> Result<u16> {
        u16::try_from(len / rec)
            .map_err(|_| anyhow::anyhow!("merged '{what}' exceeds 65535 records"))
    };
    let mut phdr_term = vec![0u8; PHDR_LEN];
    phdr_term[..3].copy_from_slice(b"EOP");
    put_u16(
        &mut phdr_term,
        PHDR_BAG_NDX,
        count(pbag.len(), BAG_LEN, "pbag")?,
    );
    phdr.extend_from_slice(&phdr_term);
    pbag.extend_from_slice(&count(pgen.len(), GEN_LEN, "pgen")?.to_le_bytes());
    pbag.extend_from_slice(&count(pmod.len(), MOD_LEN, "pmod")?.to_le_bytes());
    pgen.extend_from_slice(&[0u8; GEN_LEN]);
    pmod.extend_from_slice(&[0u8; MOD_LEN]);
    let mut inst_term = vec![0u8; INST_LEN];
    inst_term[..3].copy_from_slice(b"EOI");
    put_u16(
        &mut inst_term,
        INST_BAG_NDX,
        count(ibag.len(), BAG_LEN, "ibag")?,
    );
    inst.extend_from_slice(&inst_term);
    ibag.extend_from_slice(&count(igen.len(), GEN_LEN, "igen")?.to_le_bytes());
    ibag.extend_from_slice(&count(imod.len(), MOD_LEN, "imod")?.to_le_bytes());
    igen.extend_from_slice(&[0u8; GEN_LEN]);
    imod.extend_from_slice(&[0u8; MOD_LEN]);
    let mut shdr_term = vec![0u8; SHDR_LEN];
    shdr_term[..3].copy_from_slice(b"EOS");
    shdr.extend_from_slice(&shdr_term);

    // INFO: first take's block with INAM replaced by the merged bank's name.
    let mut children = Vec::new();
    if let Some(info) = &parts[0].info {
        let mut cloned = info.clone();
        if let Chunk::List { children: kids, .. } = &mut cloned {
            for c in kids {
                if let Chunk::Leaf { id, data } = c
                    && id == b"INAM"
                {
                    let mut name: Vec<u8> = inam.bytes().take(254).collect();
                    name.push(0);
                    if name.len() % 2 == 1 {
                        name.push(0);
                    }
                    *data = name;
                }
            }
        }
        children.push(cloned);
    }
    let mut sdta = vec![Chunk::Leaf {
        id: *b"smpl",
        data: smpl,
    }];
    if all_24bit {
        if sm24.len() % 2 == 1 {
            sm24.push(0); // merged parity pad, derived once
        }
        sdta.push(Chunk::Leaf {
            id: *b"sm24",
            data: sm24,
        });
    }
    children.push(Chunk::List {
        id: *b"LIST",
        form: *b"sdta",
        children: sdta,
    });
    let leaf = |id: &[u8; 4], data: Vec<u8>| Chunk::Leaf { id: *id, data };
    children.push(Chunk::List {
        id: *b"LIST",
        form: *b"pdta",
        children: vec![
            leaf(b"phdr", phdr),
            leaf(b"pbag", pbag),
            leaf(b"pmod", pmod),
            leaf(b"pgen", pgen),
            leaf(b"inst", inst),
            leaf(b"ibag", ibag),
            leaf(b"imod", imod),
            leaf(b"igen", igen),
            leaf(b"shdr", shdr),
        ],
    });
    let root = Chunk::List {
        id: *b"RIFF",
        form: *b"sfbk",
        children,
    };
    let mut out = Vec::new();
    write_chunk(&root, &mut out)?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sf2split::{split_presets, tests::build_test_font};

    #[test]
    fn merged_bank_loads_and_keeps_both_identities() {
        let font = build_test_font();
        // Take Piano as-is and Strings retagged to 0:48 (GM strings slot).
        let piano = split_presets(&font, Some(0), Some(0), None)
            .unwrap()
            .remove(0);
        let strings = split_presets(&font, Some(8), Some(48), Some((0, 48)))
            .unwrap()
            .remove(0);
        let merged = merge_images(&[piano, strings], "custom-orchestra").unwrap();
        let sf = rustysynth::SoundFont::new(&mut std::io::Cursor::new(merged)).unwrap();
        let mut ids: Vec<(i32, i32)> = sf
            .get_presets()
            .iter()
            .map(|p| (p.get_bank_number(), p.get_patch_number()))
            .collect();
        ids.sort();
        assert_eq!(ids, vec![(0, 0), (0, 48)]);
    }

    #[test]
    fn duplicate_identity_is_refused() {
        let font = build_test_font();
        let a = split_presets(&font, Some(0), Some(0), None)
            .unwrap()
            .remove(0);
        let b = split_presets(&font, Some(8), Some(48), Some((0, 0)))
            .unwrap()
            .remove(0);
        assert!(merge_images(&[a, b], "x").is_err());
    }
}
