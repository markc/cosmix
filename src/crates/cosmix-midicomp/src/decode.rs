//! SMF bytes → midicomp text format (the default, `decode` direction).
//!
//! A self-contained, robust SMF reader ported from the (now MIT-licensed) C
//! `midicomp` — its `readtrack` / `metaevent` / `chanmessage` logic — rather
//! than delegating to a parsing library. The port matters for fidelity: the C
//! decoder *tolerates* malformed input (short fixed-length meta events are
//! zero-padded, split SysEx packets are merged, a bad event never discards the
//! rest of its track), whereas a typed-event library instead errors and
//! silently truncates. Output style follows the resolved [`Options`]: `verbose`
//! (aligned columns + note names), `note` (symbolic note names), `time`
//! (absolute `bar:beat:click` instead of ticks), `inc` (incremental/delta
//! time), and `fold` (wrap long hex / strings at N columns).
//!
//! Byte-for-byte output matches the original tool for well-formed input; for
//! malformed input it matches the C's *tolerant* behaviour (the C's earlier
//! uninitialised-buffer garbage on the malformed `ex1.mid` time signature was a
//! bug — see `tests/` and `README.md`).

use crate::Options;
use anyhow::{Result, bail};
use std::fmt::Write as _;

/// Decoder state. The bar:beat:click bookkeeping (`measure`/`beat`/`clicks`/
/// `m0`/`t0`) mirrors the original's globals and, like it, carries across
/// tracks; `currtime` is the absolute tick of the event being printed and
/// `old_currtime` is the previous event's tick (for `-i` incremental output).
struct Dec {
    verbose: bool,
    notes: bool,
    times: bool,
    incs: bool,
    fold: usize,
    measure: i64,
    beat: i64,
    clicks: i64,
    m0: i64,
    t0: i64,
    trk_nr: i64,
    currtime: i64,
    old_currtime: i64,
    out: String,
}

/// A bounds-checked cursor over a byte slice. Every read is fallible and never
/// panics; running off the end yields `None`, which callers turn into a clean
/// error rather than a crash.
struct Cur<'a> {
    b: &'a [u8],
    i: usize,
}

impl<'a> Cur<'a> {
    fn new(b: &'a [u8]) -> Self {
        Cur { b, i: 0 }
    }
    fn remaining(&self) -> usize {
        self.b.len().saturating_sub(self.i)
    }
    fn at_end(&self) -> bool {
        self.i >= self.b.len()
    }
    fn u8(&mut self) -> Option<u8> {
        let v = self.b.get(self.i).copied();
        if v.is_some() {
            self.i += 1;
        }
        v
    }
    fn u16(&mut self) -> Option<u16> {
        let hi = self.u8()? as u16;
        let lo = self.u8()? as u16;
        Some((hi << 8) | lo)
    }
    fn u32(&mut self) -> Option<u32> {
        let a = self.u8()? as u32;
        let b = self.u8()? as u32;
        let c = self.u8()? as u32;
        let d = self.u8()? as u32;
        Some((a << 24) | (b << 16) | (c << 8) | d)
    }
    /// Read exactly `n` bytes, advancing the cursor; `None` if fewer remain.
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let end = self.i.checked_add(n)?;
        let s = self.b.get(self.i..end)?;
        self.i = end;
        Some(s)
    }
    /// Read a four-byte chunk identifier (e.g. `MThd`/`MTrk`) as-is.
    fn chunk_id(&mut self) -> Option<[u8; 4]> {
        let s = self.take(4)?;
        Some([s[0], s[1], s[2], s[3]])
    }
    /// SMF variable-length quantity, capped at four bytes / 28 bits (a longer
    /// run is malformed — reject rather than desync the stream).
    fn varlen(&mut self) -> Result<u32> {
        let mut c = self
            .u8()
            .ok_or_else(|| anyhow::anyhow!("premature EOF in varlen"))?;
        let mut value = (c & 0x7f) as u32;
        let mut n = 1;
        while c & 0x80 != 0 {
            if n >= 4 {
                bail!("invalid variable-length quantity (over 4 bytes)");
            }
            c = self
                .u8()
                .ok_or_else(|| anyhow::anyhow!("premature EOF in varlen"))?;
            value = (value << 7) | (c & 0x7f) as u32;
            n += 1;
        }
        Ok(value)
    }
    /// Read a length-prefixed payload: a variable-length count followed by that
    /// many bytes, clamped to the track bytes that actually remain. Returns the
    /// bytes and whether the declared length exceeded what remained (i.e. the
    /// event was truncated against the track boundary).
    fn lenpayload(&mut self) -> Result<(&'a [u8], bool)> {
        let want = self.varlen()? as usize;
        let avail = self.remaining();
        let over = want > avail;
        let data = self.take(want.min(avail)).unwrap_or(&[]);
        Ok((data, over))
    }
}

/// Decode an SMF into the text format. Returns the text and a flag that is true
/// if any track contained malformed/desynced data that stopped its decode early
/// (the text still holds everything decodable up to that point — matching the C
/// tool, which prints what it has and then exits non-zero). Header-level
/// problems (not an SMF at all) are hard errors.
pub fn decode(bytes: &[u8], opts: &Options) -> Result<(String, bool)> {
    let mut cur = Cur::new(bytes);

    // Header: "MThd" <len32==6> <format16> <ntrks16> <division16>.
    match cur.chunk_id() {
        Some(id) if &id == b"MThd" => {}
        _ => bail!("not a Standard MIDI File (missing MThd header)"),
    }
    let hdr_len = cur
        .u32()
        .ok_or_else(|| anyhow::anyhow!("truncated MThd length"))?;
    // A valid MThd is at least the fixed 6 bytes (format, ntrks, division).
    if hdr_len < 6 {
        bail!("MThd header length {hdr_len} too short (need at least 6)");
    }
    let format = cur
        .u16()
        .ok_or_else(|| anyhow::anyhow!("truncated MThd format"))?;
    let ntrks = cur
        .u16()
        .ok_or_else(|| anyhow::anyhow!("truncated MThd ntrks"))?;
    let division = cur
        .u16()
        .ok_or_else(|| anyhow::anyhow!("truncated MThd division"))?;
    if format > 2 {
        bail!("can't deal with format {format} files (must be 0, 1, or 2)");
    }
    // Skip any extra header bytes the (declared) length carries past the fixed 6.
    if hdr_len > 6 {
        let extra = (hdr_len - 6) as usize;
        if extra > cur.remaining() {
            bail!("MThd header length {hdr_len} extends past end of file");
        }
        cur.take(extra);
    }

    let mut d = Dec {
        verbose: opts.verbose,
        notes: opts.note,
        times: opts.time,
        incs: opts.inc,
        fold: opts.fold.unwrap_or(0),
        measure: 4,
        beat: 96,
        clicks: 96,
        m0: 0,
        t0: 0,
        trk_nr: 0,
        currtime: 0,
        old_currtime: 0,
        // Pre-size: text output is a few times the input on average.
        out: String::with_capacity(bytes.len().saturating_mul(4).max(64)),
    };

    // Header line. A negative (SMPTE) division reconstructs the raw fps/ticks
    // bytes exactly as the C tool printed them; absolute-time output is then
    // forced off (no bar:beat:click frame in SMPTE timing).
    if division & 0x8000 != 0 {
        d.times = false;
        // -((-(division>>8)) & 0xff): the signed frames-per-second high byte.
        let fps = -(((!(division >> 8)).wrapping_add(1) & 0xff) as i64);
        let ticks = (division & 0xff) as i64;
        let _ = writeln!(d.out, "MFile {format} {ntrks} {fps} {ticks}");
        // Beat/Clicks irrelevant under SMPTE (times off); keep them >= 1.
        d.beat = 1;
        d.clicks = 1;
    } else {
        let _ = writeln!(d.out, "MFile {format} {ntrks} {division}");
        d.beat = division as i64;
        d.clicks = division as i64;
        // A zero division would make Beat a zero divisor in prtime under -t.
        if d.beat < 1 {
            d.beat = 1;
        }
    }

    // Tracks. We read each `MTrk` chunk, process exactly its declared length of
    // bytes (clamped to what actually remains), and ignore any non-MTrk chunk.
    // Each track is decoded from its own length-bounded slice, so a desync
    // inside one track never affects the framing of the next chunk.
    let mut malformed = false;
    while !cur.at_end() {
        let id = match cur.chunk_id() {
            Some(id) => id,
            None => {
                // 1–3 trailing bytes: too few to form a chunk header.
                malformed = true;
                break;
            }
        };
        let declared = match cur.u32() {
            Some(l) => l as usize,
            None => {
                // A chunk id followed by a truncated length field.
                malformed = true;
                break;
            }
        };
        // A chunk that claims more bytes than the file actually holds is a
        // truncated file: process what remains, but flag it.
        if declared > cur.remaining() {
            malformed = true;
        }
        let len = declared.min(cur.remaining());
        let body = cur.take(len).unwrap_or(&[]);
        if &id == b"MTrk" {
            malformed |= d.read_track(body);
        }
        // Unknown chunk: skipped (already consumed `len` bytes).
    }

    Ok((d.out, malformed))
}

impl Dec {
    /// Decode one track body (the bytes *inside* an `MTrk` chunk), emitting the
    /// `MTrk` … `TrkEnd` text block. Mirrors the C `readtrack`: running status,
    /// the channel-message length table, SysEx merge/continuation, and meta
    /// dispatch. Tolerant of malformed input — a byte-stream desync (a bad
    /// variable-length quantity, a data byte with no running status, an
    /// unexpected status byte, or a truncated message) stops *this* track
    /// cleanly (the `TrkEnd` is still emitted) and is reported via the returned
    /// flag, rather than aborting the whole decode or discarding what was read.
    /// Returns `true` if the track stopped early on malformed data.
    fn read_track(&mut self, body: &[u8]) -> bool {
        // Channel-message data-byte counts, indexed by (status >> 4) & 0xf.
        const CHANTYPE: [u8; 16] = [0, 0, 0, 0, 0, 0, 0, 0, 2, 2, 2, 2, 1, 1, 2, 0];

        self.trk_nr += 1;
        self.currtime = 0;
        self.old_currtime = 0;
        self.out.push_str("MTrk\n");

        let mut cur = Cur::new(body);
        let mut status: u8 = 0; // running status (0 = none active)
        let mut sysex_cont = false; // mid a split SysEx (F0 without terminating F7)
        let mut sysex_buf: Vec<u8> = Vec::new();
        let mut malformed = false;

        while !cur.at_end() {
            self.old_currtime = self.currtime;
            let delta = match cur.varlen() {
                Ok(d) => d as i64,
                Err(_) => {
                    malformed = true;
                    break;
                }
            };
            self.currtime = self.currtime.saturating_add(delta);

            let mut c = match cur.u8() {
                Some(c) => c,
                None => {
                    // A delta with no following event byte = a truncated track.
                    malformed = true;
                    break;
                }
            };

            if sysex_cont && c != 0xf7 {
                malformed = true;
                break;
            }

            let mut c1 = 0u8;
            let running;
            if c & 0x80 == 0 {
                // Running status: `c` is actually the first data byte.
                if status == 0 {
                    malformed = true;
                    break;
                }
                running = true;
                c1 = c;
                c = status;
            } else if c < 0xf0 {
                // New channel status.
                status = c;
                running = false;
            } else {
                // System message (F0/F7/FF) — cancels running status.
                running = false;
                status = 0;
            }

            let needed = CHANTYPE[((c >> 4) & 0xf) as usize];
            if needed > 0 {
                // Channel data bytes are 0x00..=0x7f; a high-bit byte here means
                // a missing byte or a status where data was expected — a desync.
                if !running {
                    match cur.u8() {
                        Some(b) if b & 0x80 == 0 => c1 = b,
                        _ => {
                            malformed = true;
                            break;
                        }
                    }
                }
                let c2 = if needed > 1 {
                    match cur.u8() {
                        Some(b) if b & 0x80 == 0 => b,
                        _ => {
                            malformed = true;
                            break;
                        }
                    }
                } else {
                    0
                };
                self.chan_message(c, c1, c2);
                continue;
            }

            match c {
                0xff => {
                    let type_ = match cur.u8() {
                        Some(t) => t,
                        None => {
                            malformed = true;
                            break;
                        }
                    };
                    let (payload, over) = match cur.lenpayload() {
                        Ok(p) => p,
                        Err(_) => {
                            malformed = true;
                            break;
                        }
                    };
                    if over {
                        malformed = true;
                    }
                    let payload = payload.to_vec();
                    self.meta_event(type_, &payload);
                }
                0xf0 => {
                    let (data, over) = match cur.lenpayload() {
                        Ok(p) => p,
                        Err(_) => {
                            malformed = true;
                            break;
                        }
                    };
                    if over {
                        malformed = true;
                    }
                    sysex_buf.clear();
                    sysex_buf.push(0xf0);
                    sysex_buf.extend_from_slice(data);
                    if data.last() == Some(&0xf7) {
                        let buf = std::mem::take(&mut sysex_buf);
                        self.sysex(&buf);
                    } else {
                        // Split SysEx: wait for the F7 continuation packet(s).
                        sysex_cont = true;
                    }
                }
                0xf7 => {
                    let (data, over) = match cur.lenpayload() {
                        Ok(p) => p,
                        Err(_) => {
                            malformed = true;
                            break;
                        }
                    };
                    if over {
                        malformed = true;
                    }
                    if !sysex_cont {
                        // A standalone F7 escape = arbitrary bytes.
                        self.arbitrary(data);
                    } else {
                        sysex_buf.extend_from_slice(data);
                        if data.last() == Some(&0xf7) {
                            let buf = std::mem::take(&mut sysex_buf);
                            self.sysex(&buf);
                            sysex_cont = false;
                        }
                    }
                }
                _ => {
                    // An undefined status byte (e.g. F1–F6) desyncs the stream.
                    malformed = true;
                    break;
                }
            }
        }

        self.out.push_str("TrkEnd\n");
        malformed
    }

    /// Emit the per-event time prefix, in the same shapes as the original
    /// `prtime()`: tick or `bar:beat:click`, absolute or `-i` incremental, each
    /// plain or column-padded.
    fn prtime(&mut self) {
        // `beat`/`measure` are kept >= 1, but guard anyway so a degenerate file
        // can't divide-by-zero.
        if self.times && self.beat != 0 && self.measure != 0 {
            let reltime = if self.incs {
                self.currtime - self.old_currtime
            } else {
                self.currtime - self.t0
            };
            let m = reltime / self.beat;
            let bar = m / self.measure + self.m0;
            let bt = m % self.measure;
            let cl = reltime % self.beat;
            if self.verbose {
                let _ = write!(self.out, "{bar:03}:{bt:02}:{cl:03} ");
            } else {
                let _ = write!(self.out, "{bar}:{bt}:{cl} ");
            }
        } else {
            let val = if self.incs {
                self.currtime - self.old_currtime
            } else {
                self.currtime
            };
            if self.verbose {
                let _ = write!(self.out, "{val:<10} ");
            } else {
                let _ = write!(self.out, "{val} ");
            }
        }
    }

    /// Render a MIDI pitch as a decimal number, or as a `c4`/`f#3` note name
    /// when note names are enabled (`pitch/12` octave, matching the C `mknote`).
    fn mknote(&self, pitch: u8) -> String {
        const N: [&str; 12] = [
            "c", "c#", "d", "d#", "e", "f", "f#", "g", "g#", "a", "a#", "b",
        ];
        if self.notes {
            format!("{}{}", N[(pitch % 12) as usize], pitch / 12)
        } else {
            format!("{pitch}")
        }
    }

    /// A channel voice message. `status` carries the type nibble and channel;
    /// `c1`/`c2` are the (already-read) data bytes (`c2` unused for the 1-byte
    /// program-change / channel-pressure messages). The verbose format strings
    /// carry trailing padding that matches the original column layout
    /// byte-for-byte; `writeln!` supplies the newline.
    fn chan_message(&mut self, status: u8, c1: u8, c2: u8) {
        let ch = (status & 0x0f) as i64 + 1;
        self.prtime();
        match status & 0xf0 {
            0x80 => {
                let n = self.mknote(c1);
                if self.verbose {
                    let _ = writeln!(self.out, "Off     ch={ch:<2}  note={n:<3}  vol={c2:<3}");
                } else {
                    let _ = writeln!(self.out, "Off ch={ch} n={n} v={c2}");
                }
            }
            0x90 => {
                let n = self.mknote(c1);
                if self.verbose {
                    let _ = writeln!(self.out, "On      ch={ch:<2}  note={n:<3}  vol={c2:<3}");
                } else {
                    let _ = writeln!(self.out, "On ch={ch} n={n} v={c2}");
                }
            }
            0xa0 => {
                let n = self.mknote(c1);
                if self.verbose {
                    let _ = writeln!(self.out, "PolyPr  ch={ch:<2}  note={n:<3}  val={c2:<3}");
                } else {
                    let _ = writeln!(self.out, "PoPr ch={ch} n={n} v={c2}");
                }
            }
            0xb0 => {
                if self.verbose {
                    let _ = writeln!(self.out, "Param   ch={ch:<2}  con={c1:<3}   val={c2:<3}");
                } else {
                    let _ = writeln!(self.out, "Par ch={ch} c={c1} v={c2}");
                }
            }
            0xe0 => {
                let bend = 128 * c2 as i64 + c1 as i64;
                if self.verbose {
                    let _ = writeln!(self.out, "Pb      ch={ch:<2}  val={bend:<3}");
                } else {
                    let _ = writeln!(self.out, "Pb ch={ch} v={bend}");
                }
            }
            0xc0 => {
                if self.verbose {
                    let _ = writeln!(self.out, "ProgCh  ch={ch:<2}  prog={c1:<3}");
                } else {
                    let _ = writeln!(self.out, "PrCh ch={ch} p={c1}");
                }
            }
            0xd0 => {
                if self.verbose {
                    let _ = writeln!(self.out, "ChanPr  ch={ch:<2}  val={c1:<3}");
                } else {
                    let _ = writeln!(self.out, "ChPr ch={ch} v={c1}");
                }
            }
            _ => {}
        }
    }

    /// Decode a meta event by its type byte, mirroring the C `metaevent` +
    /// callbacks. Short fixed-layout payloads are tolerated: each field reads
    /// the stored byte or 0 when absent (`mfield`), so a crafted short event
    /// can never read past the payload and never discards the rest of the track.
    fn meta_event(&mut self, type_: u8, m: &[u8]) {
        match type_ {
            0x00 => {
                self.prtime();
                let n = ((mfield(m, 0) as i64) << 8) + mfield(m, 1) as i64;
                let _ = writeln!(self.out, "SeqNr {n}");
            }
            0x01..=0x0f => self.text_meta(type_, m),
            0x2f => {
                self.prtime();
                self.out.push_str("Meta TrkEnd\n");
            }
            0x51 => {
                self.prtime();
                let t = ((mfield(m, 0) as i64) << 16)
                    + ((mfield(m, 1) as i64) << 8)
                    + mfield(m, 2) as i64;
                let _ = writeln!(self.out, "Tempo {t}");
            }
            0x54 => {
                self.prtime();
                let _ = writeln!(
                    self.out,
                    "SMPTE {} {} {} {} {}",
                    mfield(m, 0),
                    mfield(m, 1),
                    mfield(m, 2),
                    mfield(m, 3),
                    mfield(m, 4)
                );
            }
            0x58 => {
                self.prtime();
                let nn = mfield(m, 0) as i64;
                // `dd` is a denominator *power*; clamp it (like the C) so a
                // hostile byte yields a sane positive divisor.
                let dd = mfield(m, 1).min(24);
                let cc = mfield(m, 2);
                let bb = mfield(m, 3);
                let denom = 1i64 << dd;
                let _ = writeln!(self.out, "TimeSig {nn}/{denom} {cc} {bb}");
                self.timesig_update(nn, denom);
            }
            0x59 => {
                self.prtime();
                let sf = mfield(m, 0) as i8 as i64; // signed: -7..=7
                let mi = mfield(m, 1);
                let _ = writeln!(
                    self.out,
                    "KeySig {sf} {}",
                    if mi != 0 { "minor" } else { "major" }
                );
            }
            0x7f => {
                self.prtime();
                self.out.push_str("SeqSpec");
                self.prhex(m);
            }
            _ => self.misc_meta(type_, m),
        }
    }

    /// A text meta event (`Meta Text "…"`, `Meta SeqName "…"`, etc.). Types
    /// 1–7 print a keyword label; type 3 prints `SeqName` on the first track and
    /// `TrkName` thereafter. Types 8–15 (no standard label) print the numeric
    /// `Meta 0xNN "…"` form, which — unlike the C's non-reparseable `Unrec` —
    /// round-trips back through the compiler.
    fn text_meta(&mut self, type_: u8, bytes: &[u8]) {
        const TT: [&str; 8] = [
            "",
            "Text",
            "Copyright",
            "TrkName",
            "InstrName",
            "Lyric",
            "Marker",
            "Cue",
        ];
        self.prtime();
        if type_ == 3 && self.trk_nr == 1 {
            self.out.push_str("Meta SeqName ");
        } else if (1..=7).contains(&type_) {
            let _ = write!(self.out, "Meta {} ", TT[type_ as usize]);
        } else {
            let _ = write!(self.out, "Meta 0x{type_:02x} ");
        }
        self.prtext(bytes);
    }

    /// A non-text meta event, printed as `Meta 0xNN <hex>`.
    fn misc_meta(&mut self, type_: u8, bytes: &[u8]) {
        self.prtime();
        let _ = write!(self.out, "Meta 0x{type_:02x}");
        self.prhex(bytes);
    }

    fn sysex(&mut self, mess: &[u8]) {
        self.prtime();
        self.out.push_str("SysEx");
        self.prhex(mess);
    }

    fn arbitrary(&mut self, mess: &[u8]) {
        self.prtime();
        self.out.push_str("Arb");
        self.prhex(mess);
    }

    /// Update the bar:beat:click reference frame after a time signature, so
    /// subsequent `-t` timestamps are measured from the new meter. Beat and
    /// Measure are kept >= 1 (matching the C) so later divisions never hit zero.
    fn timesig_update(&mut self, nn: i64, denom: i64) {
        if self.beat > 0 && self.measure > 0 {
            self.m0 += (self.currtime - self.t0) / (self.beat * self.measure);
        }
        self.t0 = self.currtime;
        self.measure = nn;
        if self.measure < 1 {
            self.measure = 1;
        }
        if denom > 0 {
            self.beat = 4 * self.clicks / denom;
        }
        if self.beat < 1 {
            self.beat = 1;
        }
    }

    /// Emit a double-quoted, escaped string (`prtext`): `" \ \r \n \0` escaped,
    /// other non-printables as `\xHH`, with optional `-f` column folding.
    fn prtext(&mut self, p: &[u8]) {
        let mut pos = 25usize;
        self.out.push('"');
        for &c in p {
            if self.fold > 0 && pos >= self.fold {
                self.out.push_str("\\\n\t");
                pos = 13;
                if c == b' ' || c == b'\t' {
                    self.out.push('\\');
                    pos += 1;
                }
            }
            match c {
                b'\\' | b'"' => {
                    self.out.push('\\');
                    self.out.push(c as char);
                    pos += 2;
                }
                b'\r' => {
                    self.out.push_str("\\r");
                    pos += 2;
                }
                b'\n' => {
                    self.out.push_str("\\n");
                    pos += 2;
                }
                0 => {
                    self.out.push_str("\\0");
                    pos += 2;
                }
                _ => {
                    if c == b' ' || c.is_ascii_graphic() {
                        self.out.push(c as char);
                        pos += 1;
                    } else {
                        let _ = write!(self.out, "\\x{c:02x}");
                        pos += 4;
                    }
                }
            }
        }
        self.out.push_str("\"\n");
    }

    /// Emit a space-separated run of hex bytes (`prhex`), each ` %02x`, with
    /// optional `-f` column folding onto `\<newline><tab>`-prefixed lines.
    fn prhex(&mut self, p: &[u8]) {
        let mut pos = 25usize;
        for &b in p {
            if self.fold > 0 && pos >= self.fold {
                let _ = write!(self.out, "\\\n\t{b:02x}");
                pos = 14;
            } else {
                let _ = write!(self.out, " {b:02x}");
                pos += 3;
            }
        }
        self.out.push('\n');
    }
}

/// Return data byte `i` of a meta payload, or 0 when the event is shorter than
/// the fixed-size layout requires (the C `metafield`). Reading the stored byte
/// or a zero pad means a crafted short or zero-length meta event can never read
/// past the payload.
fn mfield(m: &[u8], i: usize) -> u8 {
    m.get(i).copied().unwrap_or(0)
}
