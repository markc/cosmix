//! Minimal RIFF chunk tree — parse, serialize, locate leaves.
//!
//! Shared by the SF3 decoder ([`crate::sf3`]) and the per-preset SF2 splitter
//! ([`crate::sf2split`]), both of which rewrite parts of an `sfbk` RIFF image
//! while preserving the rest byte-for-byte. Pure std; no features.

use anyhow::{Result, anyhow, bail};

/// A parsed RIFF chunk: a container (`RIFF`/`LIST` with a form type and
/// children) or a leaf (id + raw bytes).
#[derive(Clone)]
pub(crate) enum Chunk {
    List {
        id: [u8; 4],
        form: [u8; 4],
        children: Vec<Chunk>,
    },
    Leaf {
        id: [u8; 4],
        data: Vec<u8>,
    },
}

pub(crate) fn fourcc(id: &[u8; 4]) -> String {
    String::from_utf8_lossy(id).into_owned()
}

/// Parse a flat sequence of RIFF chunks out of `buf`.
pub(crate) fn parse_chunks(buf: &[u8]) -> Result<Vec<Chunk>> {
    let mut out = Vec::new();
    let mut pos = 0usize;
    while pos + 8 <= buf.len() {
        let id: [u8; 4] = buf[pos..pos + 4].try_into().unwrap();
        let size = u32::from_le_bytes(buf[pos + 4..pos + 8].try_into().unwrap()) as usize;
        pos += 8;
        if pos + size > buf.len() {
            bail!("RIFF chunk '{}' size {} overruns buffer", fourcc(&id), size);
        }
        let body = &buf[pos..pos + size];
        if &id == b"RIFF" || &id == b"LIST" {
            if size < 4 {
                bail!("container '{}' too small for a form type", fourcc(&id));
            }
            let form: [u8; 4] = body[0..4].try_into().unwrap();
            let children = parse_chunks(&body[4..])?;
            out.push(Chunk::List { id, form, children });
        } else {
            out.push(Chunk::Leaf {
                id,
                data: body.to_vec(),
            });
        }
        pos += size;
        // RIFF chunks are word-aligned: a pad byte follows an odd size.
        if size % 2 == 1 {
            pos += 1;
        }
    }
    Ok(out)
}

/// A RIFF chunk size is a u32 field; every chunk (leaf or container) must fit.
fn chunk_size_u32(id: &[u8; 4], len: usize) -> Result<u32> {
    u32::try_from(len)
        .map_err(|_| anyhow!("chunk '{}' body {len} exceeds RIFF u32 size", fourcc(id)))
}

/// Serialize a chunk (recomputing all sizes + word-pad) into `out`. Fallible so
/// that a chunk that overruns the u32 size field is rejected rather than
/// silently truncated into a corrupt file.
pub(crate) fn write_chunk(c: &Chunk, out: &mut Vec<u8>) -> Result<()> {
    match c {
        Chunk::Leaf { id, data } => {
            let size = chunk_size_u32(id, data.len())?;
            out.extend_from_slice(id);
            out.extend_from_slice(&size.to_le_bytes());
            out.extend_from_slice(data);
            if data.len() % 2 == 1 {
                out.push(0);
            }
        }
        Chunk::List { id, form, children } => {
            let mut body = Vec::new();
            body.extend_from_slice(form);
            for ch in children {
                write_chunk(ch, &mut body)?;
            }
            let size = chunk_size_u32(id, body.len())?;
            out.extend_from_slice(id);
            out.extend_from_slice(&size.to_le_bytes());
            out.extend_from_slice(&body);
            if body.len() % 2 == 1 {
                out.push(0);
            }
        }
    }
    Ok(())
}

/// Find the leaf `leaf_id` inside the `LIST` whose form type is `list_form`.
pub(crate) fn find_leaf<'a>(
    children: &'a [Chunk],
    list_form: &[u8; 4],
    leaf_id: &[u8; 4],
) -> Option<&'a [u8]> {
    for c in children {
        if let Chunk::List { form, children, .. } = c
            && form == list_form
        {
            for sub in children {
                if let Chunk::Leaf { id, data } = sub
                    && id == leaf_id
                {
                    return Some(data);
                }
            }
        }
    }
    None
}

/// Mutable twin of [`find_leaf`].
pub(crate) fn find_leaf_mut<'a>(
    children: &'a mut [Chunk],
    list_form: &[u8; 4],
    leaf_id: &[u8; 4],
) -> Option<&'a mut Vec<u8>> {
    for c in children {
        if let Chunk::List { form, children, .. } = c
            && form == list_form
        {
            for sub in children {
                if let Chunk::Leaf { id, data } = sub
                    && id == leaf_id
                {
                    return Some(data);
                }
            }
        }
    }
    None
}

/// Find the `LIST` child with the given form type (e.g. `INFO`).
pub(crate) fn find_list<'a>(children: &'a [Chunk], list_form: &[u8; 4]) -> Option<&'a Chunk> {
    children
        .iter()
        .find(|c| matches!(c, Chunk::List { form, .. } if form == list_form))
}

#[cfg(feature = "sf3")]
pub(crate) fn read_i32(buf: &[u8], off: usize) -> i32 {
    i32::from_le_bytes(buf[off..off + 4].try_into().unwrap())
}

#[cfg(feature = "sf3")]
pub(crate) fn write_i32(buf: &mut [u8], off: usize, v: i32) {
    buf[off..off + 4].copy_from_slice(&v.to_le_bytes());
}
