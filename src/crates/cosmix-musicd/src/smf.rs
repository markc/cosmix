//! Minimal Standard-MIDI-File track surgery for per-track stem rendering.
//!
//! Just enough SMF understanding to (a) slice a format-1 file into its MTrk
//! chunks, (b) walk one track's events to learn its name, first program /
//! bank-select, drum-channel usage and whether it has notes at all, and (c)
//! rebuild a tempo/meta-only "conductor" track so a stem can carry the tempo
//! map without leaking track 0's notes (codex review decision). Events are
//! otherwise copied verbatim — no interpretation, no requantizing.

use anyhow::{Context, Result, bail};

/// A parsed SMF: header fields plus each track chunk's raw body bytes.
pub struct Smf {
    pub format: u16,
    /// Raw division field, copied verbatim into stem headers.
    pub division: [u8; 2],
    pub tracks: Vec<Vec<u8>>,
}

/// What a track contains, as far as stem rendering cares. Only "sounding
/// notes" (note-ons with velocity > 0) count — setup-only CC/program tracks
/// and velocity-0 (note-off-semantics) events don't make a track a stem.
#[derive(Debug, Default)]
pub struct TrackInfo {
    /// First `TrkName` meta event.
    pub name: Option<String>,
    /// Distinct EFFECTIVE (bank, program) identities at sounding-note time,
    /// in order of first use: per-channel bank-select MSB and program state
    /// (both default 0) resolved when each note fires; channel 10 notes
    /// report bank 128. This is what a font must provide to play the track.
    pub identities: Vec<(u16, u16)>,
    /// Sounding notes on MIDI channel 10 (0-based 9) — the GM drum channel.
    pub drum_notes: usize,
    /// Sounding notes on all other channels.
    pub melodic_notes: usize,
}

impl TrackInfo {
    /// Track produces sound — worth a stem.
    pub fn sounding(&self) -> bool {
        self.drum_notes + self.melodic_notes > 0
    }
    /// Pure drum track (GM channel 10 only).
    pub fn drums(&self) -> bool {
        self.drum_notes > 0 && self.melodic_notes == 0
    }
    /// A single per-preset font can't cover this track: its sounding notes
    /// use more than one (bank, program) identity — mid-track program or
    /// bank changes, or a drum/melodic channel mix.
    pub fn needs_full_bank(&self) -> bool {
        self.identities.len() > 1
    }
}

pub fn parse(bytes: &[u8]) -> Result<Smf> {
    if bytes.len() < 14 || &bytes[0..4] != b"MThd" {
        bail!("not a Standard MIDI File (no MThd)");
    }
    let hlen = usize::try_from(u32::from_be_bytes(bytes[4..8].try_into().unwrap()))
        .context("MThd length exceeds address space")?;
    if hlen < 6 {
        bail!("MThd too short");
    }
    let format = u16::from_be_bytes(bytes[8..10].try_into().unwrap());
    let division = [bytes[12], bytes[13]];
    let mut tracks = Vec::new();
    let mut pos = 8usize.checked_add(hlen).context("MThd length overflow")?;
    if pos > bytes.len() {
        bail!("MThd overruns file");
    }
    while let Some(chunk_header_end) = pos.checked_add(8).filter(|end| *end <= bytes.len()) {
        let id = &bytes[pos..pos + 4];
        let len = usize::try_from(u32::from_be_bytes(
            bytes[pos + 4..chunk_header_end].try_into().unwrap(),
        ))
        .context("SMF chunk length exceeds address space")?;
        pos = chunk_header_end;
        let end = pos.checked_add(len).context("SMF chunk length overflow")?;
        if end > bytes.len() {
            bail!("SMF chunk overruns file");
        }
        if id == b"MTrk" {
            tracks.push(bytes[pos..end].to_vec());
        } // unknown chunk ids are skipped, per spec
        pos = end;
    }
    if tracks.is_empty() {
        bail!("SMF has no tracks");
    }
    Ok(Smf {
        format,
        division,
        tracks,
    })
}

/// Assemble a format-1 SMF from raw track bodies.
pub fn assemble(division: [u8; 2], tracks: &[&[u8]]) -> Vec<u8> {
    assemble_with_format(1, division, tracks)
}

/// Assemble a canonical format-0 SMF from one raw track body.
pub fn assemble_format0(division: [u8; 2], track: &[u8]) -> Vec<u8> {
    assemble_with_format(0, division, &[track])
}

fn assemble_with_format(format: u16, division: [u8; 2], tracks: &[&[u8]]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(b"MThd");
    out.extend_from_slice(&6u32.to_be_bytes());
    out.extend_from_slice(&format.to_be_bytes());
    out.extend_from_slice(&(tracks.len() as u16).to_be_bytes());
    out.extend_from_slice(&division);
    for t in tracks {
        out.extend_from_slice(b"MTrk");
        out.extend_from_slice(&(t.len() as u32).to_be_bytes());
        out.extend_from_slice(t);
    }
    out
}

fn read_varlen(buf: &[u8], pos: &mut usize) -> Result<u32> {
    let mut v: u32 = 0;
    for _ in 0..4 {
        let Some(&b) = buf.get(*pos) else {
            bail!("truncated varlen")
        };
        *pos += 1;
        v = (v << 7) | (b & 0x7F) as u32;
        if b & 0x80 == 0 {
            return Ok(v);
        }
    }
    bail!("varlen longer than 4 bytes")
}

/// SMF varlen values are capped at four encoded bytes.
const VARLEN_MAX: u32 = 0x0FFF_FFFF;

fn write_varlen(out: &mut Vec<u8>, mut v: u32) {
    let mut stack = [0u8; 5]; // any u32 fits in 5 groups; callers cap at VARLEN_MAX
    let mut n = 0;
    loop {
        stack[n] = (v & 0x7F) as u8;
        v >>= 7;
        n += 1;
        if v == 0 {
            break;
        }
    }
    for i in (0..n).rev() {
        out.push(stack[i] | if i > 0 { 0x80 } else { 0 });
    }
}

/// One decoded event, borrowed from the track buffer.
enum Event<'a> {
    /// Channel voice message: status byte (with channel) + data bytes.
    Channel { status: u8, data: &'a [u8] },
    /// Meta event: type byte + payload.
    Meta { kind: u8, data: &'a [u8] },
    /// SysEx (F0/F7): skipped content.
    SysEx,
}

/// Walk `track`, calling `f(delta, event)` per event. Handles running status;
/// stops at end-of-track (also delivered to `f`).
fn walk<'a>(track: &'a [u8], mut f: impl FnMut(u32, Event<'a>)) -> Result<()> {
    let mut pos = 0usize;
    let mut running: Option<u8> = None;
    while pos < track.len() {
        let delta = read_varlen(track, &mut pos)?;
        let Some(&first) = track.get(pos) else {
            bail!("truncated event")
        };
        let status = if first >= 0x80 {
            pos += 1;
            first
        } else {
            match running {
                Some(s) => s,
                None => bail!("running status with no prior status byte"),
            }
        };
        match status {
            0x80..=0xEF => {
                running = Some(status);
                let n = match status & 0xF0 {
                    0xC0 | 0xD0 => 1,
                    _ => 2,
                };
                if pos + n > track.len() {
                    bail!("truncated channel event");
                }
                f(
                    delta,
                    Event::Channel {
                        status,
                        data: &track[pos..pos + n],
                    },
                );
                pos += n;
            }
            0xFF => {
                running = None; // meta events cancel running status
                let Some(&kind) = track.get(pos) else {
                    bail!("truncated meta")
                };
                pos += 1;
                let len = read_varlen(track, &mut pos)? as usize;
                if pos + len > track.len() {
                    bail!("truncated meta payload");
                }
                f(
                    delta,
                    Event::Meta {
                        kind,
                        data: &track[pos..pos + len],
                    },
                );
                pos += len;
                if kind == 0x2F {
                    return Ok(()); // end of track
                }
            }
            0xF0 | 0xF7 => {
                running = None; // sysex cancels running status
                let len = read_varlen(track, &mut pos)? as usize;
                if pos + len > track.len() {
                    bail!("truncated sysex");
                }
                f(delta, Event::SysEx);
                pos += len;
            }
            _ => bail!("unexpected status byte 0x{status:02X}"),
        }
    }
    Ok(())
}

/// Inspect one track: name, sounding-note counts, and the effective
/// (bank, program) identity of every sounding note, tracked per channel.
pub fn scan(track: &[u8]) -> Result<TrackInfo> {
    let mut info = TrackInfo::default();
    let mut bank = [0u8; 16]; // per-channel bank-select MSB, default 0
    let mut prog = [0u8; 16]; // per-channel program, default 0
    walk(track, |_, ev| match ev {
        Event::Channel { status, data } => {
            let ch = (status & 0x0F) as usize;
            match status & 0xF0 {
                0x90 if data[1] > 0 => {
                    let ident = if ch == 9 {
                        info.drum_notes += 1;
                        // Percussion channels select bank 128 + CC0 (that is
                        // rustysynth's model); plain GM drums are 128.
                        (128u16 + bank[ch] as u16, prog[ch] as u16)
                    } else {
                        info.melodic_notes += 1;
                        (bank[ch] as u16, prog[ch] as u16)
                    };
                    if !info.identities.contains(&ident) {
                        info.identities.push(ident);
                    }
                }
                0xC0 => prog[ch] = data[0],
                0xB0 if data[0] == 0 => bank[ch] = data[1],
                _ => {}
            }
        }
        Event::Meta { kind: 0x03, data } if info.name.is_none() => {
            info.name = Some(String::from_utf8_lossy(data).trim().to_owned());
        }
        _ => {}
    })?;
    Ok(info)
}

/// Effective (bank, program) identities of every sounding note across the
/// WHOLE song, with channel state tracked in absolute-time order across all
/// tracks — in a format-1 file, a channel's bank-select/program-change may
/// live in a different track than its notes, so per-track scanning would
/// misattribute them. Ties at the same tick resolve in track order, matching
/// how sequencers merge simultaneous events. Percussion-channel notes report
/// bank 128 + CC0.
pub fn scan_song(smf: &Smf) -> Result<Vec<(u16, u16)>> {
    enum Ev {
        Bank(u8, u8),
        Prog(u8, u8),
        Note(u8),
    }
    // (tick, track, seq) sort key; push order gives stable intra-track seq.
    let mut events: Vec<(u64, usize, usize, Ev)> = Vec::new();
    for (ti, track) in smf.tracks.iter().enumerate() {
        let mut tick: u64 = 0;
        let mut seq = 0usize;
        walk(track, |delta, ev| {
            tick += delta as u64;
            if let Event::Channel { status, data } = ev {
                let ch = status & 0x0F;
                let e = match status & 0xF0 {
                    0x90 if data[1] > 0 => Some(Ev::Note(ch)),
                    0xC0 => Some(Ev::Prog(ch, data[0])),
                    0xB0 if data[0] == 0 => Some(Ev::Bank(ch, data[1])),
                    _ => None,
                };
                if let Some(e) = e {
                    events.push((tick, ti, seq, e));
                    seq += 1;
                }
            }
        })
        .with_context(|| format!("scanning track {ti}"))?;
    }
    events.sort_by_key(|&(t, ti, seq, _)| (t, ti, seq));

    let mut bank = [0u8; 16];
    let mut prog = [0u8; 16];
    let mut identities: Vec<(u16, u16)> = Vec::new();
    for (_, _, _, e) in events {
        match e {
            Ev::Bank(ch, v) => bank[ch as usize] = v,
            Ev::Prog(ch, v) => prog[ch as usize] = v,
            Ev::Note(ch) => {
                let c = ch as usize;
                let ident = if ch == 9 {
                    (128u16 + bank[c] as u16, prog[c] as u16)
                } else {
                    (bank[c] as u16, prog[c] as u16)
                };
                if !identities.contains(&ident) {
                    identities.push(ident);
                }
            }
        }
    }
    Ok(identities)
}

/// Rebuild a track keeping ONLY what a stem's conductor needs: tempo (0x51),
/// time signature (0x58) and end-of-track. Dropped events fold their deltas
/// into the next kept event, so the tempo map's absolute timing is exact.
pub fn conductor_only(track: &[u8]) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut pending: u32 = 0;
    let mut overflow = false;
    walk(track, |delta, ev| {
        pending = pending.saturating_add(delta);
        if pending > VARLEN_MAX {
            overflow = true; // folded delta no longer varlen-encodable
            return;
        }
        if let Event::Meta {
            kind: kind @ (0x51 | 0x58 | 0x2F),
            data,
        } = ev
        {
            write_varlen(&mut out, pending);
            pending = 0;
            out.push(0xFF);
            out.push(kind);
            write_varlen(&mut out, data.len() as u32);
            out.extend_from_slice(data);
        }
    })?;
    if overflow {
        bail!("conductor delta exceeds SMF varlen range");
    }
    // A track without an explicit end-of-track meta still needs one.
    if !out.ends_with(&[0xFF, 0x2F, 0x00]) {
        write_varlen(&mut out, pending);
        out.push(0xFF);
        out.push(0x2F);
        out.push(0x00);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// delta-encoded test track: TrkName, tempo, bank CC, program change,
    /// a note pair on ch 3, a note on ch 10 (0-based 9), end.
    fn test_track() -> Vec<u8> {
        let mut t = Vec::new();
        t.extend_from_slice(&[0x00, 0xFF, 0x03, 4]);
        t.extend_from_slice(b"Bass");
        t.extend_from_slice(&[0x00, 0xFF, 0x51, 3, 0x07, 0xA1, 0x20]); // 500000 µs
        t.extend_from_slice(&[0x00, 0xB2, 0x00, 0x08]); // ch3 CC0 bank 8
        t.extend_from_slice(&[0x00, 0xC2, 0x24]); // ch3 program 36
        t.extend_from_slice(&[0x10, 0x92, 60, 100]); // note on (explicit)
        t.extend_from_slice(&[0x60, 60, 0]); // running status note off
        t.extend_from_slice(&[0x00, 0x99, 36, 100]); // ch10 kick
        t.extend_from_slice(&[0x20, 0xFF, 0x2F, 0x00]);
        t
    }

    #[test]
    fn scan_reads_name_identities_notes() {
        let info = scan(&test_track()).unwrap();
        assert_eq!(info.name.as_deref(), Some("Bass"));
        // melodic note on ch3 fires under bank 8 / program 36; the ch-10
        // note reports the drum bank with its channel's default program.
        assert_eq!(info.identities, vec![(8, 36), (128, 0)]);
        // one sounding melodic note (the vel-0 running-status pair is a
        // note-off) + one sounding ch-10 note → mixed track.
        assert_eq!((info.melodic_notes, info.drum_notes), (1, 1));
        assert!(info.sounding());
        assert!(!info.drums(), "mixed track is not a pure drum track");
        assert!(
            info.needs_full_bank(),
            "drum+melodic mix can't use one extract"
        );
    }

    #[test]
    fn percussion_bank_variation_is_visible() {
        // CC0=8 on the drum channel: effective bank 136, not 128 —
        // rustysynth applies 128 + CC0 on percussion channels.
        let mut t = Vec::new();
        t.extend_from_slice(&[0x00, 0xB9, 0x00, 8]);
        t.extend_from_slice(&[0x00, 0x99, 36, 100]);
        t.extend_from_slice(&[0x00, 0xFF, 0x2F, 0x00]);
        let info = scan(&t).unwrap();
        assert_eq!(info.identities, vec![(136, 0)]);
        assert!(info.drums());
    }

    #[test]
    fn note_before_program_change_needs_full_bank() {
        // A note fires under the default 0:0, THEN the track switches to
        // program 40 and plays again: two effective identities.
        let mut t = Vec::new();
        t.extend_from_slice(&[0x00, 0x90, 60, 100]);
        t.extend_from_slice(&[0x10, 0xC0, 40]);
        t.extend_from_slice(&[0x10, 0x90, 62, 100]);
        t.extend_from_slice(&[0x00, 0xFF, 0x2F, 0x00]);
        let info = scan(&t).unwrap();
        assert_eq!(info.identities, vec![(0, 0), (0, 40)]);
        assert!(info.needs_full_bank());
    }

    #[test]
    fn mid_track_bank_change_needs_full_bank() {
        // Same program, different bank MSB → two identities (codex round-3
        // finding: bank changes must not be invisible).
        let mut t = Vec::new();
        t.extend_from_slice(&[0x00, 0xC0, 10]);
        t.extend_from_slice(&[0x00, 0x90, 60, 100]);
        t.extend_from_slice(&[0x10, 0xB0, 0x00, 8]);
        t.extend_from_slice(&[0x10, 0x90, 62, 100]);
        t.extend_from_slice(&[0x00, 0xFF, 0x2F, 0x00]);
        let info = scan(&t).unwrap();
        assert_eq!(info.identities, vec![(0, 10), (8, 10)]);
        assert!(info.needs_full_bank());
    }

    #[test]
    fn setup_only_track_is_not_sounding() {
        // CC + program change + velocity-0 note-on only: no stem, no drums.
        let mut t = Vec::new();
        t.extend_from_slice(&[0x00, 0xB9, 0x07, 100]); // ch10 volume CC
        t.extend_from_slice(&[0x00, 0xC2, 0x24]);
        t.extend_from_slice(&[0x00, 0x99, 36, 0]); // vel 0 = note off semantics
        t.extend_from_slice(&[0x00, 0xFF, 0x2F, 0x00]);
        let info = scan(&t).unwrap();
        assert!(!info.sounding());
        assert!(!info.drums());
    }

    #[test]
    fn conductor_keeps_tempo_and_folds_deltas() {
        let c = conductor_only(&test_track()).unwrap();
        let info = scan(&c).unwrap();
        assert!(!info.sounding(), "no notes may survive");
        // tempo at delta 0, then end-of-track at folded delta 0x10+0x60+0x20
        // = 0x90 = 144, which varlen-encodes as two bytes 0x81 0x10.
        assert_eq!(&c[0..6], &[0x00, 0xFF, 0x51, 3, 0x07, 0xA1]);
        let tail = &c[c.len() - 5..];
        assert_eq!(tail, &[0x81, 0x10, 0xFF, 0x2F, 0x00]);
    }

    #[test]
    fn scan_song_resolves_cross_track_setup() {
        // Track A sets ch5's bank+program at tick 0; track B plays ch5's
        // note at tick 0x20. Per-track scanning would report (0,0) for the
        // note; the song-wide time-ordered scan must report (8,48).
        let mut setup = Vec::new();
        setup.extend_from_slice(&[0x00, 0xB5, 0x00, 8]);
        setup.extend_from_slice(&[0x00, 0xC5, 48]);
        setup.extend_from_slice(&[0x00, 0xFF, 0x2F, 0x00]);
        let mut notes = Vec::new();
        notes.extend_from_slice(&[0x20, 0x95, 60, 100]);
        notes.extend_from_slice(&[0x00, 0xFF, 0x2F, 0x00]);
        let smf = parse(&assemble([0x01, 0xE0], &[&setup, &notes])).unwrap();
        assert_eq!(scan_song(&smf).unwrap(), vec![(8, 48)]);
    }

    #[test]
    fn assemble_and_parse_roundtrip() {
        let t = test_track();
        let smf = assemble([0x01, 0xE0], &[&t, &t]);
        let parsed = parse(&smf).unwrap();
        assert_eq!(parsed.format, 1);
        assert_eq!(parsed.division, [0x01, 0xE0]);
        assert_eq!(parsed.tracks.len(), 2);
        assert_eq!(parsed.tracks[0], t);
    }

    #[test]
    fn oversized_header_and_chunk_lengths_return_errors_without_overflow() {
        let mut header = vec![0; 14];
        header[..4].copy_from_slice(b"MThd");
        header[4..8].copy_from_slice(&u32::MAX.to_be_bytes());
        assert!(parse(&header).is_err());

        let mut chunk = assemble_format0([1, 0xE0], &[]);
        chunk[18..22].copy_from_slice(&u32::MAX.to_be_bytes());
        assert!(parse(&chunk).is_err());
    }
}
