//! SF3 → SF2 in-memory decode.
//!
//! rustysynth loads only **uncompressed** SF2. An SF3 SoundFont is a normal
//! SF2 RIFF whose `smpl` sample pool, instead of raw 16-bit PCM, holds one
//! **Ogg-Vorbis stream per sample** concatenated end to end; each sample
//! header's `start`/`end` are *byte* offsets delimiting that stream, and its
//! `startloop`/`endloop` are frame offsets **relative to the sample's start**.
//!
//! This module decodes those streams (pure-Rust `lewton`, MIT/Apache-2.0) and
//! rebuilds a byte-identical-in-structure SF2 image: an uncompressed `smpl`
//! pool of PCM plus a `shdr` chunk whose offsets are rewritten into SF2's
//! word-index convention. Everything else (all `pdta` generators/modulators,
//! the `INFO` block) is preserved verbatim, so the rest of the pipeline —
//! `rustysynth` — is completely unchanged. An SF2 input is returned untouched.
//!
//! Gated behind the default `sf3` feature; `lewton` is the only added dep and
//! it is pure Rust, so the "links no C" property of the render core holds.

use std::io::Cursor;

use anyhow::{Context, Result, anyhow, bail};

use crate::riff::{
    Chunk, find_leaf, find_leaf_mut, fourcc, parse_chunks, read_i32, write_chunk, write_i32,
};

const SHDR_RECORD_LEN: usize = 46;
// Byte offsets of the four i32 sample-address fields within a 46-byte shdr
// record (after the 20-byte name): start, end, startloop, endloop.
const OFF_START: usize = 20;
const OFF_END: usize = 24;
const OFF_STARTLOOP: usize = 28;
const OFF_ENDLOOP: usize = 32;

/// Decode one sample's Ogg-Vorbis stream to mono 16-bit PCM.
fn decode_ogg(bytes: &[u8]) -> Result<Vec<i16>> {
    use lewton::inside_ogg::OggStreamReader;
    let mut reader =
        OggStreamReader::new(Cursor::new(bytes)).map_err(|e| anyhow!("ogg header: {e}"))?;
    let channels = reader.ident_hdr.audio_channels as usize;
    let mut out = Vec::new();
    while let Some(pck) = reader
        .read_dec_packet_itl()
        .map_err(|e| anyhow!("ogg packet: {e}"))?
    {
        if channels <= 1 {
            out.extend_from_slice(&pck);
        } else {
            // SF2 samples are mono; if a stream were multi-channel, take L.
            out.extend(pck.iter().step_by(channels).copied());
        }
    }
    Ok(out)
}

/// True iff this SoundFont has Ogg-compressed samples (i.e. it is SF3).
///
/// SF3 carries no version marker distinct from SF2, so detection is
/// necessarily content-based: scan every non-terminator sample and, for any
/// region beginning with `OggS`, confirm with a real lewton header parse (not
/// the magic bytes alone). A genuine uncompressed SF2 therefore always returns
/// false — real PCM audio is never a byte-valid Ogg-Vorbis stream — so ordinary
/// SF2 files pass through untouched. The only theoretical false positive is an
/// *adversarially crafted* SF2 whose raw sample bytes happen to form a complete
/// valid Ogg stream; that isn't reachable from real audio, and the worst case
/// is a re-encoded (not corrupted) SF2, so this heuristic is accepted.
fn is_sf3(smpl: &[u8], shdr: &[u8]) -> bool {
    let n = shdr.len() / SHDR_RECORD_LEN;
    for i in 0..n.saturating_sub(1) {
        let off = i * SHDR_RECORD_LEN;
        let start = read_i32(shdr, off + OFF_START);
        let end = read_i32(shdr, off + OFF_END);
        if start < 0 || end <= start {
            continue;
        }
        let (s, e) = (start as usize, end as usize);
        if e > smpl.len() || e - s < 4 || &smpl[s..s + 4] != b"OggS" {
            continue;
        }
        if lewton::inside_ogg::OggStreamReader::new(Cursor::new(&smpl[s..e])).is_ok() {
            return true;
        }
    }
    false
}

/// If `bytes` is an SF3 (Ogg-compressed) SoundFont, decode it to an equivalent
/// uncompressed SF2 byte image. If it is already an uncompressed SF2 (or has no
/// samples), return it unchanged.
pub fn maybe_decode_sf3(bytes: Vec<u8>) -> Result<Vec<u8>> {
    let mut roots = parse_chunks(&bytes)?;
    let Some(Chunk::List { id, form, children }) = roots.first_mut() else {
        bail!("not a RIFF container");
    };
    if id != b"RIFF" || form != b"sfbk" {
        bail!("not an sfbk SoundFont (form '{}')", fourcc(form));
    }

    // Read the compressed sample pool and the sample-header table.
    let smpl = match find_leaf(children, b"sdta", b"smpl") {
        Some(d) => d.to_vec(),
        None => return Ok(bytes), // no sample data — nothing to do
    };
    let shdr = match find_leaf(children, b"pdta", b"shdr") {
        Some(d) => d.to_vec(),
        None => return Ok(bytes),
    };
    if shdr.len() < SHDR_RECORD_LEN || shdr.len() % SHDR_RECORD_LEN != 0 {
        bail!("shdr chunk length {} is not a multiple of 46", shdr.len());
    }

    if !is_sf3(&smpl, &shdr) {
        return Ok(bytes); // already uncompressed SF2, or no compressed samples
    }

    // records[.. n-1] are samples; the last is the SF2 terminator record.
    let n = shdr.len() / SHDR_RECORD_LEN;
    let mut new_pcm: Vec<i16> = Vec::new();
    let mut new_shdr = shdr.clone();

    for i in 0..n {
        let off = i * SHDR_RECORD_LEN;
        let start = read_i32(&shdr, off + OFF_START);
        let end = read_i32(&shdr, off + OFF_END);
        let start_loop = read_i32(&shdr, off + OFF_STARTLOOP);
        let end_loop = read_i32(&shdr, off + OFF_ENDLOOP);

        if i == n - 1 {
            // Terminator: no sample data; point every field at the pool end.
            let base = new_pcm.len().min(i32::MAX as usize) as i32;
            for f in [OFF_START, OFF_END, OFF_STARTLOOP, OFF_ENDLOOP] {
                write_i32(&mut new_shdr, off + f, base);
            }
            continue;
        }

        // Reject (not clamp) malformed byte offsets — a valid SF3 never has them.
        if start < 0 || end < 0 || start > end {
            bail!("sample #{i} has invalid byte offsets {start}..{end}");
        }
        let (s, e) = (start as usize, end as usize);
        if e > smpl.len() {
            bail!(
                "sample #{i} byte range {s}..{e} exceeds smpl ({})",
                smpl.len()
            );
        }
        let region = &smpl[s..e];
        let decoded: Vec<i16> = if region.len() >= 4 && &region[0..4] == b"OggS" {
            decode_ogg(region).with_context(|| format!("decoding sample #{i}"))?
        } else {
            // Defensive: a raw-PCM sample inside an SF3 (uncommon).
            if region.len() % 2 != 0 {
                bail!(
                    "sample #{i} raw region has odd byte length {}",
                    region.len()
                );
            }
            region
                .chunks_exact(2)
                .map(|b| i16::from_le_bytes([b[0], b[1]]))
                .collect()
        };

        let new_start = new_pcm.len();
        new_pcm.extend_from_slice(&decoded);
        let new_end = new_pcm.len();
        // SF2 sample offsets are i32; a pool past that range can't be addressed.
        if new_end > i32::MAX as usize {
            bail!("decoded sample pool exceeds SF2's i32 address range");
        }
        // startloop/endloop are frame offsets relative to the sample's start;
        // clamp into the decoded sample (and keep start <= end) so a malformed
        // header can't emit an out-of-range or wrapping loop point.
        let dlen = decoded.len() as i64;
        let sl = (start_loop as i64).clamp(0, dlen);
        let el = (end_loop as i64).clamp(sl, dlen);
        write_i32(&mut new_shdr, off + OFF_START, new_start as i32);
        write_i32(&mut new_shdr, off + OFF_END, new_end as i32);
        write_i32(
            &mut new_shdr,
            off + OFF_STARTLOOP,
            new_start as i32 + sl as i32,
        );
        write_i32(
            &mut new_shdr,
            off + OFF_ENDLOOP,
            new_start as i32 + el as i32,
        );

        // SF2 mandates a run of zero samples after each sample so an
        // interpolating oscillator can read a few frames past the end/loop
        // without bleeding into the next sample. 46 is the spec minimum.
        new_pcm.extend(std::iter::repeat_n(0i16, 46));
    }

    // Serialize the decoded PCM as the new little-endian 16-bit `smpl` chunk.
    // This early check gives a clear error and bounds the capacity reservation
    // for the one chunk decompression can grow past a RIFF u32; `write_chunk`
    // independently guards every chunk (incl. the grown `sdta`/`RIFF` parents).
    let smpl_bytes = new_pcm
        .len()
        .checked_mul(2)
        .filter(|&b| b <= u32::MAX as usize)
        .ok_or_else(|| anyhow!("decoded sample pool too large for a RIFF chunk"))?;
    let mut new_smpl = Vec::with_capacity(smpl_bytes);
    for sample in &new_pcm {
        new_smpl.extend_from_slice(&sample.to_le_bytes());
    }

    // Swap the two leaves in place; everything else is preserved verbatim.
    *find_leaf_mut(children, b"sdta", b"smpl").expect("smpl located above") = new_smpl;
    *find_leaf_mut(children, b"pdta", b"shdr").expect("shdr located above") = new_shdr;

    let mut out = Vec::with_capacity(bytes.len().saturating_mul(6));
    write_chunk(roots.first().unwrap(), &mut out)?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn non_sf3_passes_through_unchanged() {
        // A tiny well-formed sfbk with a raw-PCM smpl and one shdr record +
        // terminator must be returned byte-for-byte.
        let mut shdr = vec![0u8; SHDR_RECORD_LEN * 2];
        // sample #0: start=0, end=2 (2 words of PCM), loops 0..2
        write_i32(&mut shdr, OFF_END, 2);
        write_i32(&mut shdr, OFF_ENDLOOP, 2);
        let smpl = vec![0x11, 0x22, 0x33, 0x44]; // 2 i16 samples, NOT OggS
        let sfbk = build_sfbk(&smpl, &shdr);
        let out = maybe_decode_sf3(sfbk.clone()).unwrap();
        assert_eq!(out, sfbk, "SF2 input must pass through untouched");
    }

    // Minimal sfbk: RIFF(sfbk){ LIST(sdta){smpl}, LIST(pdta){shdr} }
    fn build_sfbk(smpl: &[u8], shdr: &[u8]) -> Vec<u8> {
        let sdta = Chunk::List {
            id: *b"LIST",
            form: *b"sdta",
            children: vec![Chunk::Leaf {
                id: *b"smpl",
                data: smpl.to_vec(),
            }],
        };
        let pdta = Chunk::List {
            id: *b"LIST",
            form: *b"pdta",
            children: vec![Chunk::Leaf {
                id: *b"shdr",
                data: shdr.to_vec(),
            }],
        };
        let root = Chunk::List {
            id: *b"RIFF",
            form: *b"sfbk",
            children: vec![sdta, pdta],
        };
        let mut out = Vec::new();
        write_chunk(&root, &mut out).unwrap();
        out
    }

    #[test]
    fn riff_roundtrips() {
        // Odd-length smpl (5 bytes) forces a write pad byte, then a parse skip —
        // exercising both sides of the word-alignment handling.
        let smpl = vec![1, 2, 3, 4, 5];
        let shdr = vec![0u8; SHDR_RECORD_LEN];
        let sfbk = build_sfbk(&smpl, &shdr);
        assert_eq!(sfbk.len() % 2, 0, "container is word-aligned overall");
        let parsed = parse_chunks(&sfbk).unwrap();
        let mut re = Vec::new();
        write_chunk(&parsed[0], &mut re).unwrap();
        assert_eq!(re, sfbk);
    }

    #[test]
    fn pcm_starting_with_oggs_is_not_mistaken_for_sf3() {
        // A raw-PCM smpl whose first bytes are exactly "OggS" must still be
        // detected as SF2 (lewton header parse fails) and pass through.
        let smpl = b"OggS\x00\x00\x00\x00".to_vec(); // 4 i16 samples, NOT a real Ogg
        let mut shdr = vec![0u8; SHDR_RECORD_LEN * 2];
        write_i32(&mut shdr, OFF_END, 4);
        let sfbk = build_sfbk(&smpl, &shdr);
        assert!(
            !is_sf3(&smpl, &shdr),
            "invalid Ogg header must not read as SF3"
        );
        assert_eq!(maybe_decode_sf3(sfbk.clone()).unwrap(), sfbk);
    }
}
