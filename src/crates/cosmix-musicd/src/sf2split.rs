//! Split a SoundFont into one single-preset SF2 per preset.
//!
//! Why: SF2 synth instances (e.g. Ardour's ACE Fluid Synth, one per MIDI
//! track) each load the ENTIRE bank into private heap — 12 tracks ×
//! MuseScore_General (197 MB) ≈ 2.9 GB, and a 64-track session is impossible
//! on a 16 GB machine. Extracting each preset into its own .sf2 — only the
//! instruments and samples that preset actually references — shrinks a
//! per-track instance to the size of its one instrument.
//!
//! Extraction is structural, not resynthesis: preset/instrument zones,
//! generators, and modulators are copied verbatim (only cross-array indices
//! are remapped), sample PCM is sliced out with the spec's 46-point zero
//! guard re-appended, and the preset keeps its original bank/program number
//! so MIDI bank-select/program-change events still land. A render through an
//! extract is therefore identical to a render through the full bank.
//!
//! Pure std (+anyhow): part of the render core, no features required. SF3
//! inputs are decoded to SF2 by [`crate::sf3`] before this module sees them
//! (the CLI does that hop), so the splitter only handles uncompressed SF2.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use anyhow::{Context, Result, anyhow, bail};

use crate::riff::{Chunk, find_leaf, find_list, parse_chunks, write_chunk};

// Fixed record sizes of the nine pdta "hydra" sub-chunks (SF2 spec §7).
const PHDR_LEN: usize = 38;
const PBAG_LEN: usize = 4;
const PMOD_LEN: usize = 10;
const PGEN_LEN: usize = 4;
const INST_LEN: usize = 22;
const IBAG_LEN: usize = 4;
const IMOD_LEN: usize = 10;
const IGEN_LEN: usize = 4;
const SHDR_LEN: usize = 46;

// Field offsets within a phdr record: name[20], wPreset, wBank, wPresetBagNdx.
const PHDR_PRESET: usize = 20;
const PHDR_BANK: usize = 22;
const PHDR_BAG_NDX: usize = 24;
// inst record: name[20], wInstBagNdx.
const INST_BAG_NDX: usize = 20;
// shdr record: name[20], dwStart, dwEnd, dwStartloop, dwEndloop, dwSampleRate,
// byOriginalPitch, chCorrection, wSampleLink, sfSampleType.
const SHDR_START: usize = 20;
const SHDR_END: usize = 24;
const SHDR_STARTLOOP: usize = 28;
const SHDR_ENDLOOP: usize = 32;
const SHDR_LINK: usize = 42;
const SHDR_TYPE: usize = 44;

// Generator opers that carry cross-array indices (SF2 spec §8.1.2).
const GEN_INSTRUMENT: u16 = 41;
const GEN_SAMPLE_ID: u16 = 53;

// sfSampleType bits: 1 mono, 2 right, 4 left, 8 linked, 0x8000 ROM.
const TYPE_LINK_BITS: u16 = 2 | 4 | 8;
const TYPE_ROM: u16 = 0x8000;

/// SF2 spec: a run of ≥46 zero data points must follow every sample so an
/// interpolating oscillator can read past end/loop without bleeding into the
/// next sample.
const GUARD_POINTS: usize = 46;

/// Identity of one preset inside a bank.
#[derive(Clone, Debug)]
pub struct PresetEntry {
    pub bank: u16,
    pub program: u16,
    pub name: String,
}

/// One extracted preset: its identity, the single-preset SF2 image, and any
/// non-fatal oddities found while extracting (dangling stereo links etc.).
#[derive(Debug)]
pub struct SplitOutput {
    pub entry: PresetEntry,
    pub sf2: Vec<u8>,
    pub warnings: Vec<String>,
}

/// Borrowed view of a parsed SF2: the chunks the extractor reads.
struct Font<'a> {
    /// The whole `LIST INFO` chunk, cloned into each extract (INAM patched).
    info: Option<&'a Chunk>,
    smpl: &'a [u8],
    /// 24-bit sample extension: one low-order byte per `smpl` word.
    sm24: Option<&'a [u8]>,
    phdr: &'a [u8],
    pbag: &'a [u8],
    pmod: &'a [u8],
    pgen: &'a [u8],
    inst: &'a [u8],
    ibag: &'a [u8],
    imod: &'a [u8],
    igen: &'a [u8],
    shdr: &'a [u8],
}

fn u16_at(buf: &[u8], off: usize) -> u16 {
    u16::from_le_bytes(buf[off..off + 2].try_into().unwrap())
}

fn u32_at(buf: &[u8], off: usize) -> u32 {
    u32::from_le_bytes(buf[off..off + 4].try_into().unwrap())
}

/// NUL-terminated fixed-20 name field → trimmed String.
fn name20(rec: &[u8]) -> String {
    let raw = &rec[..20];
    let end = raw.iter().position(|&b| b == 0).unwrap_or(20);
    String::from_utf8_lossy(&raw[..end]).trim().to_owned()
}

/// String → fixed-20 NUL-padded name field.
fn fixed20(s: &str) -> [u8; 20] {
    let mut out = [0u8; 20];
    for (i, b) in s.bytes().take(19).enumerate() {
        out[i] = b;
    }
    out
}

/// Locate a mandatory hydra leaf and validate its record geometry: length a
/// multiple of `rec_len` with at least the terminal record present.
fn hydra_leaf<'a>(children: &'a [Chunk], id: &[u8; 4], rec_len: usize) -> Result<&'a [u8]> {
    let name = String::from_utf8_lossy(id);
    let data = find_leaf(children, b"pdta", id)
        .ok_or_else(|| anyhow!("missing pdta sub-chunk '{name}'"))?;
    if data.len() < rec_len || data.len() % rec_len != 0 {
        bail!(
            "pdta '{name}' length {} is not a multiple of {rec_len}",
            data.len()
        );
    }
    Ok(data)
}

fn parse_font(roots: &[Chunk]) -> Result<Font<'_>> {
    let Some(Chunk::List { id, form, children }) = roots.first() else {
        bail!("not a RIFF container");
    };
    if id != b"RIFF" || form != b"sfbk" {
        bail!(
            "not an sfbk SoundFont (form '{}')",
            String::from_utf8_lossy(form)
        );
    }
    let smpl = find_leaf(children, b"sdta", b"smpl")
        .ok_or_else(|| anyhow!("SoundFont has no sample data (sdta/smpl)"))?;
    Ok(Font {
        info: find_list(children, b"INFO"),
        smpl,
        sm24: find_leaf(children, b"sdta", b"sm24"),
        phdr: hydra_leaf(children, b"phdr", PHDR_LEN)?,
        pbag: hydra_leaf(children, b"pbag", PBAG_LEN)?,
        pmod: hydra_leaf(children, b"pmod", PMOD_LEN)?,
        pgen: hydra_leaf(children, b"pgen", PGEN_LEN)?,
        inst: hydra_leaf(children, b"inst", INST_LEN)?,
        ibag: hydra_leaf(children, b"ibag", IBAG_LEN)?,
        imod: hydra_leaf(children, b"imod", IMOD_LEN)?,
        igen: hydra_leaf(children, b"igen", IGEN_LEN)?,
        shdr: hydra_leaf(children, b"shdr", SHDR_LEN)?,
    })
}

/// The bag/gen/mod index ranges of one zone list (preset- or instrument-level
/// share the same 4-byte bag record shape).
struct ZoneRange {
    gen_lo: usize,
    gen_hi: usize,
    mod_lo: usize,
    mod_hi: usize,
}

/// One level of the hydra (preset or instrument): its bag array plus the REAL
/// (terminal-excluded) gen/mod record counts a zone range may end at but never
/// cross.
struct Level<'a> {
    bag: &'a [u8],
    n_gens: usize,
    n_mods: usize,
    what: &'static str,
}

/// Resolve the zones of record `idx` in a header array (`phdr`/`inst`): bag
/// range from this record's bag index to the next record's, then each bag's
/// gen/mod ranges the same way. Validates monotonicity and bounds so a
/// malformed font fails loudly instead of extracting garbage.
fn zones_of(
    hdr: &[u8],
    hdr_len: usize,
    bag_ndx_off: usize,
    idx: usize,
    lvl: &Level<'_>,
) -> Result<Vec<ZoneRange>> {
    let Level {
        bag,
        n_gens,
        n_mods,
        what,
    } = *lvl;
    let n_bags = bag.len() / PBAG_LEN; // PBAG_LEN == IBAG_LEN
    let lo = u16_at(hdr, idx * hdr_len + bag_ndx_off) as usize;
    let hi = u16_at(hdr, (idx + 1) * hdr_len + bag_ndx_off) as usize;
    if lo > hi || hi >= n_bags {
        bail!("{what} #{idx} has invalid bag range {lo}..{hi} (of {n_bags} bags)");
    }
    let mut out = Vec::with_capacity(hi - lo);
    for b in lo..hi {
        let gen_lo = u16_at(bag, b * PBAG_LEN) as usize;
        let gen_hi = u16_at(bag, (b + 1) * PBAG_LEN) as usize;
        let mod_lo = u16_at(bag, b * PBAG_LEN + 2) as usize;
        let mod_hi = u16_at(bag, (b + 1) * PBAG_LEN + 2) as usize;
        if gen_lo > gen_hi || gen_hi > n_gens || mod_lo > mod_hi || mod_hi > n_mods {
            bail!("{what} #{idx} bag #{b} has invalid gen/mod ranges");
        }
        out.push(ZoneRange {
            gen_lo,
            gen_hi,
            mod_lo,
            mod_hi,
        });
    }
    Ok(out)
}

/// List every preset in the font, in phdr order.
pub fn list_presets(bytes: &[u8]) -> Result<Vec<PresetEntry>> {
    let roots = parse_chunks(bytes)?;
    let font = parse_font(&roots)?;
    let n = font.phdr.len() / PHDR_LEN - 1; // last record is the terminator
    Ok((0..n)
        .map(|p| {
            let rec = &font.phdr[p * PHDR_LEN..(p + 1) * PHDR_LEN];
            PresetEntry {
                bank: u16_at(rec, PHDR_BANK),
                program: u16_at(rec, PHDR_PRESET),
                name: name20(rec),
            }
        })
        .collect())
}

/// Split `bytes` (an uncompressed SF2 image) into one single-preset SF2 per
/// preset, optionally filtered by bank and/or program number. Returns the
/// extracts in phdr order; an empty Vec means nothing matched the filter.
///
/// `retag` rewrites the extracted preset's (bank, program) — the socket a
/// third-party single-instrument font answers to, so any font can be
/// renumbered to the slot a MIDI track's program change actually selects.
/// Requires the filter to match exactly ONE preset (otherwise every output
/// would claim the same slot).
pub fn split_presets(
    bytes: &[u8],
    bank: Option<u16>,
    program: Option<u16>,
    retag: Option<(u16, u16)>,
) -> Result<Vec<SplitOutput>> {
    let roots = parse_chunks(bytes)?;
    let font = parse_font(&roots)?;
    let n = font.phdr.len() / PHDR_LEN - 1;
    let selected: Vec<usize> = (0..n)
        .filter(|&p| {
            let rec = &font.phdr[p * PHDR_LEN..(p + 1) * PHDR_LEN];
            !(bank.is_some_and(|b| b != u16_at(rec, PHDR_BANK))
                || program.is_some_and(|q| q != u16_at(rec, PHDR_PRESET)))
        })
        .collect();
    if let Some((b, q)) = retag {
        if selected.len() != 1 {
            bail!(
                "--as needs the bank/program filter to select exactly one preset (matched {})",
                selected.len()
            );
        }
        if b > 128 || q > 127 {
            bail!("retag {b}:{q} out of range (bank 0-128, program 0-127)");
        }
    }
    let mut out = Vec::new();
    for p in selected {
        let rec = &font.phdr[p * PHDR_LEN..(p + 1) * PHDR_LEN];
        let name = name20(rec);
        let (src_bank, src_prog) = (u16_at(rec, PHDR_BANK), u16_at(rec, PHDR_PRESET));
        let (new_bank, new_program) = retag.unwrap_or((src_bank, src_prog));
        let entry = PresetEntry {
            bank: new_bank,
            program: new_program,
            name,
        };
        let (sf2, warnings) = extract_preset(&font, p, retag)
            .with_context(|| format!("extracting preset {src_bank}:{src_prog} '{}'", entry.name))?;
        out.push(SplitOutput {
            entry,
            sf2,
            warnings,
        });
    }
    Ok(out)
}

/// Build the single-preset SF2 image for phdr record `p`. `retag` replaces
/// the preset's (bank, program) identity in the output.
fn extract_preset(
    font: &Font<'_>,
    p: usize,
    retag: Option<(u16, u16)>,
) -> Result<(Vec<u8>, Vec<String>)> {
    let mut warnings = Vec::new();
    // Real record counts, excluding each array's terminal record. A zone's
    // gen/mod range may END at the terminal index (the terminal bag points
    // there) but must never extend past it — otherwise the zero-filled
    // terminal generator would be copied into a zone as a real generator.
    let n_insts = font.inst.len() / INST_LEN - 1;
    let n_samples = font.shdr.len() / SHDR_LEN - 1;
    let n_pgens = font.pgen.len() / PGEN_LEN - 1;
    let n_pmods = font.pmod.len() / PMOD_LEN - 1;
    let n_igens = font.igen.len() / IGEN_LEN - 1;
    let n_imods = font.imod.len() / IMOD_LEN - 1;

    // ── 1. Walk the reference graph: preset zones → instruments → samples ──
    let p_level = Level {
        bag: font.pbag,
        n_gens: n_pgens,
        n_mods: n_pmods,
        what: "preset",
    };
    let i_level = Level {
        bag: font.ibag,
        n_gens: n_igens,
        n_mods: n_imods,
        what: "instrument",
    };
    let p_zones = zones_of(font.phdr, PHDR_LEN, PHDR_BAG_NDX, p, &p_level)?;

    let mut inst_set = BTreeSet::new();
    for z in &p_zones {
        for g in z.gen_lo..z.gen_hi {
            if u16_at(font.pgen, g * PGEN_LEN) == GEN_INSTRUMENT {
                let i = u16_at(font.pgen, g * PGEN_LEN + 2) as usize;
                if i >= n_insts {
                    bail!("preset zone references instrument #{i} (font has {n_insts})");
                }
                inst_set.insert(i);
            }
        }
    }

    // Ascending old index → new index; deterministic and order-preserving.
    let inst_map: BTreeMap<usize, usize> = inst_set
        .iter()
        .enumerate()
        .map(|(new, &old)| (old, new))
        .collect();

    let mut i_zones: BTreeMap<usize, Vec<ZoneRange>> = BTreeMap::new();
    let mut sample_set = BTreeSet::new();
    for &i in &inst_set {
        let zones = zones_of(font.inst, INST_LEN, INST_BAG_NDX, i, &i_level)?;
        for z in &zones {
            for g in z.gen_lo..z.gen_hi {
                if u16_at(font.igen, g * IGEN_LEN) == GEN_SAMPLE_ID {
                    let s = u16_at(font.igen, g * IGEN_LEN + 2) as usize;
                    if s >= n_samples {
                        bail!("instrument #{i} references sample #{s} (font has {n_samples})");
                    }
                    sample_set.insert(s);
                }
            }
        }
        i_zones.insert(i, zones);
    }

    // Pull in stereo/link partners so a hard-panned pair never loses a side.
    let mut queue: VecDeque<usize> = sample_set.iter().copied().collect();
    while let Some(s) = queue.pop_front() {
        let rec = &font.shdr[s * SHDR_LEN..(s + 1) * SHDR_LEN];
        let stype = u16_at(rec, SHDR_TYPE);
        if stype & TYPE_ROM != 0 {
            bail!(
                "sample #{s} '{}' is a ROM sample — not supported",
                name20(rec)
            );
        }
        if stype & TYPE_LINK_BITS != 0 {
            let link = u16_at(rec, SHDR_LINK) as usize;
            if link < n_samples {
                if sample_set.insert(link) {
                    queue.push_back(link);
                }
            } else {
                warnings.push(format!(
                    "sample '{}' has dangling stereo link #{link}; extract keeps it mono",
                    name20(rec)
                ));
            }
        }
    }
    let sample_map: BTreeMap<usize, usize> = sample_set
        .iter()
        .enumerate()
        .map(|(new, &old)| (old, new))
        .collect();

    // ── 2. Slice the sample pools (smpl + optional sm24) ──
    let src_words = font.smpl.len() / 2;
    let mut smpl_n: Vec<u8> = Vec::new();
    let mut sm24_n: Option<Vec<u8>> = font.sm24.map(|_| Vec::new());
    // New (start, end) word offsets per selected sample, in new-index order.
    let mut new_pos: Vec<(u32, u32)> = Vec::with_capacity(sample_set.len());
    for &s in &sample_set {
        let rec = &font.shdr[s * SHDR_LEN..(s + 1) * SHDR_LEN];
        let start = u32_at(rec, SHDR_START) as usize;
        let end = u32_at(rec, SHDR_END) as usize;
        if start > end || end > src_words {
            bail!(
                "sample #{s} '{}' has invalid range {start}..{end} (pool {src_words} words)",
                name20(rec)
            );
        }
        let new_start = smpl_n.len() / 2;
        smpl_n.extend_from_slice(&font.smpl[start * 2..end * 2]);
        smpl_n.extend(std::iter::repeat_n(0u8, GUARD_POINTS * 2));
        let new_end = new_start + (end - start);
        if let Some(sm24_out) = sm24_n.as_mut() {
            // sm24 carries one low byte per smpl word; a source that doesn't
            // cover this sample's range is malformed enough that the extract
            // is better off as plain 16-bit.
            let sm24 = font.sm24.unwrap();
            if end <= sm24.len() {
                sm24_out.extend_from_slice(&sm24[start..end]);
                sm24_out.extend(std::iter::repeat_n(0u8, GUARD_POINTS));
            } else {
                warnings.push(format!(
                    "sm24 chunk too short for sample '{}'; extract downgraded to 16-bit",
                    name20(rec)
                ));
                sm24_n = None;
            }
        }
        new_pos.push((
            u32::try_from(new_start).context("extract sample pool exceeds u32")?,
            u32::try_from(new_end).context("extract sample pool exceeds u32")?,
        ));
    }
    // FluidSynth validates sm24's DECLARED chunk size as smpl/2 rounded up to
    // even (fluid_sffile.c: `sdtahalfsize += sdtahalfsize % 2`), so the parity
    // byte must live inside the chunk payload — a RIFF pad byte outside the
    // declared size would make FluidSynth ignore the whole sm24 chunk.
    if let Some(sm24_out) = sm24_n.as_mut()
        && sm24_out.len() % 2 == 1
    {
        sm24_out.push(0);
    }

    // ── 3. Rebuild the hydra with remapped indices ──
    // shdr: rebase sample offsets into the new pool, remap stereo links.
    let mut shdr_n = Vec::with_capacity((sample_set.len() + 1) * SHDR_LEN);
    for (new, &s) in sample_set.iter().enumerate() {
        let rec = &font.shdr[s * SHDR_LEN..(s + 1) * SHDR_LEN];
        let mut out: Vec<u8> = rec.to_vec();
        let (new_start, new_end) = new_pos[new];
        let old_start = u32_at(rec, SHDR_START);
        // Loops are absolute pool offsets; rebase, clamping a malformed loop
        // into the sample instead of letting it wrap or escape.
        let rebase =
            |v: u32| -> u32 { (v.saturating_sub(old_start)).min(new_end - new_start) + new_start };
        let sl = rebase(u32_at(rec, SHDR_STARTLOOP));
        let el = rebase(u32_at(rec, SHDR_ENDLOOP)).max(sl);
        out[SHDR_START..SHDR_START + 4].copy_from_slice(&new_start.to_le_bytes());
        out[SHDR_END..SHDR_END + 4].copy_from_slice(&new_end.to_le_bytes());
        out[SHDR_STARTLOOP..SHDR_STARTLOOP + 4].copy_from_slice(&sl.to_le_bytes());
        out[SHDR_ENDLOOP..SHDR_ENDLOOP + 4].copy_from_slice(&el.to_le_bytes());
        let stype = u16_at(rec, SHDR_TYPE);
        let link = u16_at(rec, SHDR_LINK) as usize;
        let (new_link, new_type) = if stype & TYPE_LINK_BITS != 0 {
            match sample_map.get(&link) {
                Some(&nl) => (nl as u16, stype),
                None => (0u16, 1), // dangling link → mono (warned above)
            }
        } else {
            (0u16, stype)
        };
        out[SHDR_LINK..SHDR_LINK + 2].copy_from_slice(&new_link.to_le_bytes());
        out[SHDR_TYPE..SHDR_TYPE + 2].copy_from_slice(&new_type.to_le_bytes());
        shdr_n.extend_from_slice(&out);
    }
    shdr_n.extend_from_slice(&terminal_rec(SHDR_LEN, "EOS", &[]));

    // Instrument level: inst + ibag + igen (sampleID remapped) + imod.
    let mut inst_n = Vec::new();
    let mut ibag_n = Vec::new();
    let mut igen_n = Vec::new();
    let mut imod_n = Vec::new();
    for &i in &inst_set {
        inst_n.extend_from_slice(&font.inst[i * INST_LEN..i * INST_LEN + INST_BAG_NDX]);
        inst_n.extend_from_slice(&u16_len(&ibag_n, IBAG_LEN)?.to_le_bytes());
        for z in &i_zones[&i] {
            ibag_n.extend_from_slice(&u16_len(&igen_n, IGEN_LEN)?.to_le_bytes());
            ibag_n.extend_from_slice(&u16_len(&imod_n, IMOD_LEN)?.to_le_bytes());
            for g in z.gen_lo..z.gen_hi {
                let oper = u16_at(font.igen, g * IGEN_LEN);
                let mut amt = u16_at(font.igen, g * IGEN_LEN + 2);
                if oper == GEN_SAMPLE_ID {
                    amt = sample_map[&(amt as usize)] as u16;
                }
                igen_n.extend_from_slice(&oper.to_le_bytes());
                igen_n.extend_from_slice(&amt.to_le_bytes());
            }
            imod_n.extend_from_slice(&font.imod[z.mod_lo * IMOD_LEN..z.mod_hi * IMOD_LEN]);
        }
    }
    // Terminals: header points at the terminal bag; the terminal bag points at
    // the terminal (zero-filled) gen and mod records.
    inst_n.extend_from_slice(&terminal_rec(
        INST_LEN,
        "EOI",
        &u16_len(&ibag_n, IBAG_LEN)?.to_le_bytes(),
    ));
    ibag_n.extend_from_slice(&u16_len(&igen_n, IGEN_LEN)?.to_le_bytes());
    ibag_n.extend_from_slice(&u16_len(&imod_n, IMOD_LEN)?.to_le_bytes());
    igen_n.extend_from_slice(&[0u8; IGEN_LEN]);
    imod_n.extend_from_slice(&[0u8; IMOD_LEN]);

    // Preset level: the one phdr record + pbag + pgen (instrument remapped) + pmod.
    let mut phdr_n = Vec::new();
    let mut pbag_n = Vec::new();
    let mut pgen_n = Vec::new();
    let mut pmod_n = Vec::new();
    phdr_n.extend_from_slice(&font.phdr[p * PHDR_LEN..p * PHDR_LEN + PHDR_BAG_NDX]);
    phdr_n.extend_from_slice(&0u16.to_le_bytes()); // its zones start at bag 0
    phdr_n.extend_from_slice(&font.phdr[p * PHDR_LEN + PHDR_BAG_NDX + 2..(p + 1) * PHDR_LEN]);
    if let Some((bank, program)) = retag {
        phdr_n[PHDR_PRESET..PHDR_PRESET + 2].copy_from_slice(&program.to_le_bytes());
        phdr_n[PHDR_BANK..PHDR_BANK + 2].copy_from_slice(&bank.to_le_bytes());
    }
    for z in &p_zones {
        pbag_n.extend_from_slice(&u16_len(&pgen_n, PGEN_LEN)?.to_le_bytes());
        pbag_n.extend_from_slice(&u16_len(&pmod_n, PMOD_LEN)?.to_le_bytes());
        for g in z.gen_lo..z.gen_hi {
            let oper = u16_at(font.pgen, g * PGEN_LEN);
            let mut amt = u16_at(font.pgen, g * PGEN_LEN + 2);
            if oper == GEN_INSTRUMENT {
                amt = inst_map[&(amt as usize)] as u16;
            }
            pgen_n.extend_from_slice(&oper.to_le_bytes());
            pgen_n.extend_from_slice(&amt.to_le_bytes());
        }
        pmod_n.extend_from_slice(&font.pmod[z.mod_lo * PMOD_LEN..z.mod_hi * PMOD_LEN]);
    }
    let mut phdr_term = terminal_rec(PHDR_LEN, "EOP", &[]);
    phdr_term[PHDR_BAG_NDX..PHDR_BAG_NDX + 2]
        .copy_from_slice(&u16_len(&pbag_n, PBAG_LEN)?.to_le_bytes());
    phdr_n.extend_from_slice(&phdr_term);
    pbag_n.extend_from_slice(&u16_len(&pgen_n, PGEN_LEN)?.to_le_bytes());
    pbag_n.extend_from_slice(&u16_len(&pmod_n, PMOD_LEN)?.to_le_bytes());
    pgen_n.extend_from_slice(&[0u8; PGEN_LEN]);
    pmod_n.extend_from_slice(&[0u8; PMOD_LEN]);

    // ── 4. Assemble the RIFF image ──
    let preset_name = name20(&font.phdr[p * PHDR_LEN..(p + 1) * PHDR_LEN]);
    let mut children = Vec::new();
    if let Some(info) = font.info {
        children.push(patched_info(info, &preset_name));
    }
    let mut sdta = vec![Chunk::Leaf {
        id: *b"smpl",
        data: smpl_n,
    }];
    if let Some(sm24) = sm24_n {
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
            leaf(b"phdr", phdr_n),
            leaf(b"pbag", pbag_n),
            leaf(b"pmod", pmod_n),
            leaf(b"pgen", pgen_n),
            leaf(b"inst", inst_n),
            leaf(b"ibag", ibag_n),
            leaf(b"imod", imod_n),
            leaf(b"igen", igen_n),
            leaf(b"shdr", shdr_n),
        ],
    });
    let root = Chunk::List {
        id: *b"RIFF",
        form: *b"sfbk",
        children,
    };
    let mut bytes = Vec::new();
    write_chunk(&root, &mut bytes)?;
    Ok((bytes, warnings))
}

/// A zero-filled terminal record with an "EOP"/"EOI"/"EOS" name and an
/// optional trailing field (the bag index for phdr/inst terminals).
fn terminal_rec(len: usize, name: &str, bag_ndx: &[u8]) -> Vec<u8> {
    let mut rec = vec![0u8; len];
    rec[..20].copy_from_slice(&fixed20(name));
    if !bag_ndx.is_empty() {
        rec[20..20 + bag_ndx.len()].copy_from_slice(bag_ndx);
    }
    rec
}

/// Current record count of a growing hydra array, as the u16 the format
/// stores. Cannot exceed u16 in a single-preset extract of a valid font, but
/// guard anyway.
fn u16_len(buf: &[u8], rec_len: usize) -> Result<u16> {
    u16::try_from(buf.len() / rec_len).map_err(|_| anyhow!("hydra array exceeds 65535 records"))
}

/// Clone the source INFO list with INAM (bank name) replaced by the preset
/// name, so tools show "Bright Yamaha Grand" rather than twelve identical
/// bank titles. Everything else (ifil version, copyright, …) is verbatim.
fn patched_info(info: &Chunk, preset_name: &str) -> Chunk {
    let mut cloned = info.clone();
    if let Chunk::List { children, .. } = &mut cloned {
        for c in children {
            if let Chunk::Leaf { id, data } = c
                && id == b"INAM"
            {
                let mut name: Vec<u8> = preset_name.bytes().take(254).collect();
                name.push(0);
                if name.len() % 2 == 1 {
                    name.push(0);
                }
                *data = name;
            }
        }
    }
    cloned
}

/// Deterministic output filename: `{bank:03}-{program:03}-{slug}.sf2`.
/// Program-sortable, shell-safe, and collision-free (bank+program is unique
/// within a font).
pub fn preset_file_name(bank: u16, program: u16, name: &str) -> String {
    let mut slug = String::new();
    for c in name.chars() {
        if c.is_ascii_alphanumeric() {
            slug.push(c.to_ascii_lowercase());
        } else if !slug.is_empty() && !slug.ends_with('-') {
            slug.push('-');
        }
    }
    let slug = slug.trim_end_matches('-');
    let slug = if slug.is_empty() { "preset" } else { slug };
    format!("{bank:03}-{program:03}-{slug}.sf2")
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::riff::Chunk;

    // ── A tiny two-preset font builder ──────────────────────────────────────
    //
    // Preset 0 "Piano" (bank 0, prog 0) → inst 0 → sample 0 (mono).
    // Preset 1 "Strings" (bank 8, prog 48) → inst 1 → sample 1 (left), whose
    // stereo partner sample 2 (right) is referenced ONLY via the link field —
    // exercising partner pull-in and link remapping.

    fn gen_rec(oper: u16, amt: u16) -> Vec<u8> {
        [oper.to_le_bytes(), amt.to_le_bytes()].concat()
    }

    fn bag(gen_ndx: u16, mod_ndx: u16) -> Vec<u8> {
        [gen_ndx.to_le_bytes(), mod_ndx.to_le_bytes()].concat()
    }

    fn shdr_rec(name: &str, start: u32, end: u32, link: u16, stype: u16) -> Vec<u8> {
        let mut r = Vec::with_capacity(SHDR_LEN);
        r.extend_from_slice(&fixed20(name));
        r.extend_from_slice(&start.to_le_bytes());
        r.extend_from_slice(&end.to_le_bytes());
        r.extend_from_slice(&start.to_le_bytes()); // startloop
        r.extend_from_slice(&end.to_le_bytes()); // endloop
        r.extend_from_slice(&44100u32.to_le_bytes());
        r.push(60); // original pitch
        r.push(0); // correction
        r.extend_from_slice(&link.to_le_bytes());
        r.extend_from_slice(&stype.to_le_bytes());
        r
    }

    fn phdr_rec(name: &str, prog: u16, bank: u16, bag_ndx: u16) -> Vec<u8> {
        let mut r = Vec::with_capacity(PHDR_LEN);
        r.extend_from_slice(&fixed20(name));
        r.extend_from_slice(&prog.to_le_bytes());
        r.extend_from_slice(&bank.to_le_bytes());
        r.extend_from_slice(&bag_ndx.to_le_bytes());
        r.extend_from_slice(&[0u8; 12]); // library/genre/morphology
        r
    }

    fn inst_rec(name: &str, bag_ndx: u16) -> Vec<u8> {
        let mut r = Vec::with_capacity(INST_LEN);
        r.extend_from_slice(&fixed20(name));
        r.extend_from_slice(&bag_ndx.to_le_bytes());
        r
    }

    /// Each sample is 8 words of ramp PCM followed by the 46-word guard.
    const SLEN: u32 = 8;
    const STRIDE: u32 = SLEN + 46;

    pub(crate) fn build_test_font() -> Vec<u8> {
        // Sample pool: three samples with guards.
        let mut smpl = Vec::new();
        for s in 0..3i16 {
            for i in 0..SLEN as i16 {
                smpl.extend_from_slice(&(s * 100 + i).to_le_bytes());
            }
            smpl.extend_from_slice(&[0u8; 46 * 2]);
        }
        let s_at = |i: u32| i * STRIDE;

        let shdr = [
            shdr_rec("mono", s_at(0), s_at(0) + SLEN, 0, 1),
            shdr_rec("strL", s_at(1), s_at(1) + SLEN, 2, 4), // left, links right
            shdr_rec("strR", s_at(2), s_at(2) + SLEN, 1, 2), // right, links left
            shdr_rec("EOS", 0, 0, 0, 0),
        ]
        .concat();

        // igen: inst0 zone → [keyrange, sampleID 0]; inst1 zone → [keyrange,
        // sampleID 1] (partner 2 pulled in only via the link). +terminal.
        let igen = [
            gen_rec(43, 0x7F00), // keyRange 0..127
            gen_rec(GEN_SAMPLE_ID, 0),
            gen_rec(43, 0x7F00),
            gen_rec(GEN_SAMPLE_ID, 1),
            gen_rec(0, 0), // terminal
        ]
        .concat();
        let ibag = [bag(0, 0), bag(2, 0), bag(4, 0)].concat(); // 2 zones + terminal
        let imod = vec![0u8; IMOD_LEN]; // terminal only
        let inst = [
            inst_rec("i-piano", 0),
            inst_rec("i-strings", 1),
            inst_rec("EOI", 2),
        ]
        .concat();

        // pgen: preset0 zone → [instrument 0]; preset1 zone → [instrument 1].
        let pgen = [
            gen_rec(GEN_INSTRUMENT, 0),
            gen_rec(GEN_INSTRUMENT, 1),
            gen_rec(0, 0),
        ]
        .concat();
        let pbag = [bag(0, 0), bag(1, 0), bag(2, 0)].concat();
        let pmod = vec![0u8; PMOD_LEN];
        let phdr = [
            phdr_rec("Piano", 0, 0, 0),
            phdr_rec("Strings", 48, 8, 1),
            phdr_rec("EOP", 0, 0, 2),
        ]
        .concat();

        let leaf = |id: &[u8; 4], data: Vec<u8>| Chunk::Leaf { id: *id, data };
        let info = Chunk::List {
            id: *b"LIST",
            form: *b"INFO",
            children: vec![
                leaf(b"ifil", vec![2, 0, 1, 0]),        // v2.01
                leaf(b"INAM", b"test bank\0".to_vec()), // INFO strings: even length
                leaf(b"isng", b"EMU8000\0".to_vec()),
            ],
        };
        let root = Chunk::List {
            id: *b"RIFF",
            form: *b"sfbk",
            children: vec![
                info,
                Chunk::List {
                    id: *b"LIST",
                    form: *b"sdta",
                    children: vec![leaf(b"smpl", smpl)],
                },
                Chunk::List {
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
                },
            ],
        };
        let mut out = Vec::new();
        write_chunk(&root, &mut out).unwrap();
        out
    }

    fn shdr_of(extract: &[u8]) -> Vec<u8> {
        let roots = parse_chunks(extract).unwrap();
        let Chunk::List { children, .. } = &roots[0] else {
            panic!()
        };
        find_leaf(children, b"pdta", b"shdr").unwrap().to_vec()
    }

    #[test]
    fn splits_and_rustysynth_loads_every_extract() {
        let font = build_test_font();
        let outs = split_presets(&font, None, None, None).unwrap();
        assert_eq!(outs.len(), 2);
        for o in &outs {
            assert!(
                o.warnings.is_empty(),
                "unexpected warnings: {:?}",
                o.warnings
            );
            // The authoritative validity check: the same parser the renderer
            // uses must accept the extract.
            rustysynth::SoundFont::new(&mut std::io::Cursor::new(o.sf2.clone()))
                .unwrap_or_else(|e| panic!("rustysynth rejects extract '{}': {e}", o.entry.name));
        }
        assert_eq!((outs[0].entry.bank, outs[0].entry.program), (0, 0));
        assert_eq!((outs[1].entry.bank, outs[1].entry.program), (8, 48));
    }

    #[test]
    fn extract_carries_only_its_samples() {
        let font = build_test_font();
        let outs = split_presets(&font, None, None, None).unwrap();
        // Piano: 1 sample + terminal. Its PCM must be sample 0's ramp.
        let piano_shdr = shdr_of(&outs[0].sf2);
        assert_eq!(piano_shdr.len() / SHDR_LEN, 2);
        assert_eq!(name20(&piano_shdr[..SHDR_LEN]), "mono");
        // Strings: referenced left sample + link-pulled right + terminal,
        // with the pair's links remapped to each other's NEW indices.
        let str_shdr = shdr_of(&outs[1].sf2);
        assert_eq!(str_shdr.len() / SHDR_LEN, 3);
        let l = &str_shdr[..SHDR_LEN];
        let r = &str_shdr[SHDR_LEN..2 * SHDR_LEN];
        assert_eq!((name20(l), u16_at(l, SHDR_LINK)), ("strL".into(), 1));
        assert_eq!((name20(r), u16_at(r, SHDR_LINK)), ("strR".into(), 0));
        // Rebased sample offsets: strL occupies the start of the new pool.
        assert_eq!(u32_at(l, SHDR_START), 0);
        assert_eq!(u32_at(l, SHDR_END), SLEN);
        assert_eq!(u32_at(r, SHDR_START), SLEN + GUARD_POINTS as u32);
    }

    #[test]
    fn bank_program_filter_selects_one() {
        let font = build_test_font();
        let outs = split_presets(&font, Some(8), Some(48), None).unwrap();
        assert_eq!(outs.len(), 1);
        assert_eq!(outs[0].entry.name, "Strings");
        assert!(
            split_presets(&font, Some(9), None, None)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn retag_rewrites_bank_and_program() {
        let font = build_test_font();
        // Retag the 8:48 Strings preset to GM slot 0:36.
        let outs = split_presets(&font, Some(8), Some(48), Some((0, 36))).unwrap();
        assert_eq!(outs.len(), 1);
        assert_eq!((outs[0].entry.bank, outs[0].entry.program), (0, 36));
        assert_eq!(outs[0].entry.name, "Strings"); // name untouched
        // The written phdr record must carry the new numbers…
        let roots = parse_chunks(&outs[0].sf2).unwrap();
        let Chunk::List { children, .. } = &roots[0] else {
            panic!()
        };
        let phdr = find_leaf(children, b"pdta", b"phdr").unwrap();
        assert_eq!(u16_at(phdr, PHDR_PRESET), 36);
        assert_eq!(u16_at(phdr, PHDR_BANK), 0);
        // …and the extract still parses with the renderer's own loader.
        rustysynth::SoundFont::new(&mut std::io::Cursor::new(outs[0].sf2.clone())).unwrap();
        // Retag with an ambiguous filter (2 matches) must be refused.
        assert!(split_presets(&font, None, None, Some((0, 36))).is_err());
    }

    #[test]
    fn zone_range_past_terminal_gen_is_rejected() {
        // Corrupt the terminal ibag to claim one generator PAST the terminal
        // igen record: extraction must fail, not copy the terminal as a real
        // generator.
        let font = build_test_font();
        let mut roots = parse_chunks(&font).unwrap();
        let Chunk::List { children, .. } = &mut roots[0] else {
            panic!()
        };
        let ibag = crate::riff::find_leaf_mut(children, b"pdta", b"ibag").unwrap();
        ibag[2 * IBAG_LEN..2 * IBAG_LEN + 2].copy_from_slice(&5u16.to_le_bytes());
        let mut mutated = Vec::new();
        write_chunk(&roots[0], &mut mutated).unwrap();
        let err = split_presets(&mutated, None, None, None).unwrap_err();
        assert!(
            format!("{err:#}").contains("invalid gen/mod ranges"),
            "got: {err:#}"
        );
    }

    #[test]
    fn file_names_are_slugged_and_sortable() {
        assert_eq!(
            preset_file_name(0, 0, "Acoustic Grand Piano"),
            "000-000-acoustic-grand-piano.sf2"
        );
        assert_eq!(
            preset_file_name(128, 0, "Standard (Drums)!"),
            "128-000-standard-drums.sf2"
        );
        assert_eq!(preset_file_name(1, 2, "***"), "001-002-preset.sf2");
    }
}
