//! midicomp text format → SMF bytes (the `-c` / compile direction).
//!
//! Tokenises with [`crate::lex`], parses the line grammar from `README.md` into
//! an owned intermediate form, then builds a [`midly::Smf`] and serialises it.
//! Channel numbers are 1-based on input; numbers may be decimal, `0x`hex, or
//! `$`/`'`bank; notes may be numeric or symbolic; binary payloads may be hex or
//! quoted strings.
//!
//! All meta events except end-of-track are emitted through
//! [`MetaMessage::Unknown`] with the raw type byte and payload — byte-identical
//! to the typed variants on the wire, and the simplest faithful encoding.

use crate::Options;
use crate::lex::{Lexer, Tok};
use anyhow::{Result, bail};
use midly::num::{u4, u7, u14, u28};
use midly::{
    Format, Header, MetaMessage, MidiMessage, PitchBend, Smf, Timing, TrackEvent, TrackEventKind,
};

/// One parsed event in owned form. Byte-slice payloads are kept here as `Vec`
/// so they outlive the borrowed `midly` view built just before writing.
enum RawKind {
    Midi(u8, MidiMessage),
    /// `(meta type byte, payload)` → `MetaMessage::Unknown`.
    Meta(u8, Vec<u8>),
    Eot,
    /// SysEx payload *without* the leading `0xF0` (midly re-adds it).
    SysEx(Vec<u8>),
    /// `0xF7` escape / arbitrary bytes.
    Escape(Vec<u8>),
}

struct RawEvent {
    delta: u32,
    kind: RawKind,
}

struct Parser<'a> {
    lex: Lexer<'a>,
    peeked: Option<Tok>,
    /// `-i`: event times in the text are incremental (delta) values, not absolute.
    incs: bool,
    // bar:beat:click reference frame (mirrors the original globals).
    measure: i64,
    beat: i64,
    clicks: i64,
    m0: i64,
    t0: i64,
}

pub fn encode(text: &str, opts: &Options) -> Result<Vec<u8>> {
    let mut p = Parser {
        lex: Lexer::new(text.as_bytes()),
        peeked: None,
        incs: opts.inc,
        measure: 4,
        beat: 96,
        clicks: 96,
        m0: 0,
        t0: 0,
    };

    // Header: MFile <format> <ntrks> <division> [<smpte subframe>]
    p.skip_eols();
    match p.next() {
        Tok::MThd => {}
        t => bail!("missing MFile header (got {t:?})"),
    }
    let format = p.expect_int("MFile format")?;
    let ntrks_declared = p.expect_int("MFile #tracks")?;
    let division = p.expect_int("MFile division")?;
    let timing = if division < 0 {
        // The header's low byte is ticks-per-frame (resolution), a full u8 —
        // not the SMPTE-offset subframe.
        let sub = p.expect_int("MFile SMPTE ticks/frame")?;
        if !(0..=255).contains(&sub) {
            bail!("SMPTE ticks/frame {sub} out of byte range 0..=255");
        }
        let neg = division
            .checked_neg()
            .ok_or_else(|| anyhow::anyhow!("illegal SMPTE division {division}"))?;
        let fps = u8::try_from(neg)
            .ok()
            .and_then(midly::Fps::from_int)
            .ok_or_else(|| anyhow::anyhow!("illegal SMPTE fps {neg} (want 24/25/29/30)"))?;
        p.clicks = 1;
        Timing::Timecode(fps, sub as u8)
    } else {
        if division > 0x7fff {
            bail!("MFile division {division} exceeds the 15-bit PPQN range (max 32767)");
        }
        p.clicks = division;
        Timing::Metrical(midly::num::u15::new(division as u16))
    };
    p.beat = p.clicks;
    p.checkeol();

    let format = match format {
        0 => Format::SingleTrack,
        1 => Format::Parallel,
        2 => Format::Sequential,
        other => bail!("MFile format {other} unsupported (must be 0, 1, or 2)"),
    };

    // Tracks.
    let mut raw_tracks: Vec<Vec<RawEvent>> = Vec::new();
    while let Some(evs) = p.track()? {
        raw_tracks.push(evs);
    }
    if p.lex.str_err {
        bail!("unterminated quoted string in input");
    }
    if p.lex.hex_err {
        bail!("malformed hex payload (non-hex garbage before end of line)");
    }
    // Honour the declared track count, and the format-0 single-track rule.
    if ntrks_declared != raw_tracks.len() as i64 {
        bail!(
            "MFile declares {ntrks_declared} track(s) but {} were found",
            raw_tracks.len()
        );
    }
    if format == Format::SingleTrack && raw_tracks.len() != 1 {
        bail!(
            "format 0 requires exactly one track, found {}",
            raw_tracks.len()
        );
    }

    // Build the borrowed midly view from the owned events and serialise.
    let tracks = raw_tracks
        .iter()
        .map(|t| {
            t.iter()
                .map(|re| TrackEvent {
                    delta: u28::new(re.delta),
                    kind: match &re.kind {
                        RawKind::Midi(ch, msg) => TrackEventKind::Midi {
                            channel: u4::new(*ch),
                            message: *msg,
                        },
                        RawKind::Meta(t, v) => TrackEventKind::Meta(MetaMessage::Unknown(*t, v)),
                        RawKind::Eot => TrackEventKind::Meta(MetaMessage::EndOfTrack),
                        RawKind::SysEx(v) => TrackEventKind::SysEx(v),
                        RawKind::Escape(v) => TrackEventKind::Escape(v),
                    },
                })
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();

    let smf = Smf {
        header: Header { format, timing },
        tracks,
    };
    // Pre-size: header (14) + per-track chunk header (8) + a few bytes/event.
    let event_count: usize = raw_tracks.iter().map(|t| t.len()).sum();
    let mut out = Vec::with_capacity(14 + raw_tracks.len() * 8 + event_count * 4);
    smf.write(&mut out)
        .map_err(|e| anyhow::anyhow!("writing SMF: {e}"))?;
    Ok(out)
}

impl Parser<'_> {
    fn next(&mut self) -> Tok {
        self.peeked.take().unwrap_or_else(|| self.lex.next())
    }

    fn peek(&mut self) -> Tok {
        if self.peeked.is_none() {
            self.peeked = Some(self.lex.next());
        }
        self.peeked.clone().unwrap()
    }

    fn skip_eols(&mut self) {
        while matches!(self.peek(), Tok::Eol) {
            self.next();
        }
    }

    fn expect_int(&mut self, what: &str) -> Result<i64> {
        match self.next() {
            Tok::Int(n) => Ok(n),
            t => bail!("expected integer for {what}, got {t:?}"),
        }
    }

    /// Consume the end of a line, tolerating and discarding trailing garbage
    /// (matching the original's lenient recovery).
    fn checkeol(&mut self) {
        loop {
            match self.next() {
                Tok::Eol | Tok::Eof => break,
                _ => {}
            }
        }
    }

    /// Parse one `MTrk … TrkEnd` block. Returns `Ok(None)` at end of input.
    fn track(&mut self) -> Result<Option<Vec<RawEvent>>> {
        self.skip_eols();
        match self.next() {
            Tok::Eof => return Ok(None),
            Tok::MTrk => {}
            t => bail!("expected MTrk, got {t:?}"),
        }
        self.checkeol();

        let mut evs: Vec<RawEvent> = Vec::new();
        let mut currtime = 0i64;
        loop {
            match self.next() {
                Tok::Eol => continue,
                Tok::Eof => bail!("unexpected EOF (missing TrkEnd)"),
                Tok::MTrk => bail!("unexpected MTrk inside a track"),
                Tok::TrkEnd => {
                    self.checkeol();
                    break;
                }
                Tok::Int(t0) => {
                    let mut newtime = t0;
                    let mut op = self.next();
                    if op == Tok::Slash {
                        // bar:beat:click form. Saturating arithmetic keeps a huge
                        // bar number from overflowing; the delta bound below then
                        // rejects it cleanly.
                        let b = self.expect_int("time beat")?;
                        newtime = newtime
                            .saturating_sub(self.m0)
                            .saturating_mul(self.measure)
                            .saturating_add(b);
                        if self.next() != Tok::Slash {
                            bail!("illegal bar:beat:click time");
                        }
                        let c = self.expect_int("time click")?;
                        newtime = self
                            .t0
                            .saturating_add(newtime.saturating_mul(self.beat))
                            .saturating_add(c);
                        op = self.next();
                    }
                    // With `-i` the parsed time is already the delta; otherwise
                    // it is absolute and the delta is the difference from the
                    // previous event. `saturating_sub` because `newtime` may be a
                    // valid i64::MIN literal; the bounds below then reject it.
                    let delta = if self.incs {
                        newtime
                    } else {
                        newtime.saturating_sub(currtime)
                    };
                    if delta < 0 {
                        bail!("event time {newtime} goes backwards (before {currtime})");
                    }
                    if delta > 0x0fff_ffff {
                        bail!("event time delta {delta} exceeds the 28-bit SMF range");
                    }
                    // An empty SysEx/Arb yields `None` (skipped); the timeline
                    // still advances so following deltas stay correct.
                    if let Some(kind) = self.event(op, newtime)? {
                        evs.push(RawEvent {
                            delta: delta as u32,
                            kind,
                        });
                    }
                    self.checkeol();
                    currtime = newtime;
                }
                t => bail!("unexpected token at start of line: {t:?}"),
            }
        }

        // Ensure exactly one end-of-track meta terminates the track.
        if !matches!(evs.last().map(|e| &e.kind), Some(RawKind::Eot)) {
            evs.push(RawEvent {
                delta: 0,
                kind: RawKind::Eot,
            });
        }
        Ok(Some(evs))
    }

    /// Parse a single event body given its leading opcode token and the event's
    /// absolute time (needed for the time-signature frame update).
    fn event(&mut self, op: Tok, newtime: i64) -> Result<Option<RawKind>> {
        let kind = match op {
            Tok::On => {
                let ch = self.checkchan()?;
                let key = self.checknote()?;
                let vel = self.checkval()?;
                RawKind::Midi(ch, MidiMessage::NoteOn { key, vel })
            }
            Tok::Off => {
                let ch = self.checkchan()?;
                let key = self.checknote()?;
                let vel = self.checkval()?;
                RawKind::Midi(ch, MidiMessage::NoteOff { key, vel })
            }
            Tok::PoPr => {
                let ch = self.checkchan()?;
                let key = self.checknote()?;
                let vel = self.checkval()?;
                RawKind::Midi(ch, MidiMessage::Aftertouch { key, vel })
            }
            Tok::Par => {
                let ch = self.checkchan()?;
                let controller = self.checkcon()?;
                let value = self.checkval()?;
                RawKind::Midi(ch, MidiMessage::Controller { controller, value })
            }
            Tok::Pb => {
                let ch = self.checkchan()?;
                let bend = self.splitval()?;
                RawKind::Midi(ch, MidiMessage::PitchBend { bend })
            }
            Tok::PrCh => {
                let ch = self.checkchan()?;
                let program = self.checkprog()?;
                RawKind::Midi(ch, MidiMessage::ProgramChange { program })
            }
            Tok::ChPr => {
                let ch = self.checkchan()?;
                let vel = self.checkval()?;
                RawKind::Midi(ch, MidiMessage::ChannelAftertouch { vel })
            }
            Tok::SysEx => {
                let mut bytes = self.lex.read_hex();
                // An empty SysEx would serialise to an unterminated `F0 00` that
                // can't be decoded back; reject it like the C tool's
                // `mf_w_sysex_event` (size < 1).
                if bytes.is_empty() {
                    eprintln!("warning: empty SysEx event ignored");
                    return Ok(None);
                }
                // Drop the leading 0xF0; midly stores/re-adds it implicitly.
                if bytes.first() == Some(&0xf0) {
                    bytes.remove(0);
                }
                RawKind::SysEx(bytes)
            }
            Tok::Arb => {
                let bytes = self.lex.read_hex();
                if bytes.is_empty() {
                    eprintln!("warning: empty Arb event ignored");
                    return Ok(None);
                }
                RawKind::Escape(bytes)
            }
            Tok::Tempo => {
                let n = self.expect_int("Tempo")?;
                if !(0..=0xff_ffff).contains(&n) {
                    bail!("Tempo {n} out of range 0..=16777215 (24-bit µs/quarter)");
                }
                let n = n as u32;
                RawKind::Meta(0x51, vec![(n >> 16) as u8, (n >> 8) as u8, n as u8])
            }
            Tok::TimeSig => {
                let nn = self.expect_int("TimeSig numerator")?;
                if !(1..=255).contains(&nn) {
                    bail!("TimeSig numerator {nn} out of range 1..=255");
                }
                if self.next() != Tok::Slash {
                    bail!("TimeSig expects <n>/<n>");
                }
                let denom = self.expect_int("TimeSig denominator")?;
                let cc = self.getbyte("TimeSig clocks")?;
                let bb = self.getbyte("TimeSig 32nds")?;
                let dd = log2_exact(denom).ok_or_else(|| {
                    anyhow::anyhow!("TimeSig denominator {denom} not a power of 2")
                })?;
                let kind = RawKind::Meta(0x58, vec![nn as u8, dd, cc, bb]);
                // Update the bar:beat:click frame for subsequent events.
                if self.beat > 0 && self.measure > 0 {
                    self.m0 += (newtime - self.t0) / (self.beat * self.measure);
                }
                self.t0 = newtime;
                self.measure = nn;
                if denom > 0 {
                    self.beat = 4 * self.clicks / denom;
                }
                // Keep Beat >= 1 (matching the C / decoder) so a tiny division
                // can't make later bar:beat:click time arithmetic degenerate.
                if self.beat < 1 {
                    self.beat = 1;
                }
                kind
            }
            Tok::Smpte => {
                let mut bytes = Vec::with_capacity(5);
                for _ in 0..5 {
                    bytes.push(self.getbyte("SMPTE")?);
                }
                RawKind::Meta(0x54, bytes)
            }
            Tok::KeySig => {
                let sf = self.expect_int("KeySig")?;
                if !(-7..=7).contains(&sf) {
                    bail!("KeySig must be between -7 and 7");
                }
                let minor = match self.next() {
                    Tok::Minor => 1u8,
                    Tok::Major => 0u8,
                    t => bail!("KeySig expects minor or major, got {t:?}"),
                };
                RawKind::Meta(0x59, vec![sf as u8, minor])
            }
            Tok::SeqNr => {
                // Accept `SeqNr <n>` or `SeqNr v=<n>` (the decoder emits the
                // former; the original lexer expected the latter).
                if matches!(self.peek(), Tok::Val) {
                    self.next();
                }
                let n = self.expect_int("SeqNr")?;
                if !(0..=65535).contains(&n) {
                    bail!("SeqNr must be between 0 and 65535");
                }
                RawKind::Meta(0x00, vec![(n >> 8) as u8, n as u8])
            }
            Tok::Meta => {
                let type_ = match self.next() {
                    Tok::TrkEnd => return Ok(Some(RawKind::Eot)),
                    Tok::Text => 1,
                    Tok::Copyright => 2,
                    Tok::SeqName => 3,
                    Tok::InstrName => 4,
                    Tok::Lyric => 5,
                    Tok::Marker => 6,
                    Tok::Cue => 7,
                    Tok::Int(n) if (0..=255).contains(&n) => n as u8,
                    Tok::Int(n) => bail!("Meta type {n} out of byte range 0..=255"),
                    t => bail!("illegal Meta type {t:?}"),
                };
                RawKind::Meta(type_, self.lex.read_hex())
            }
            Tok::SeqSpec => RawKind::Meta(0x7f, self.lex.read_hex()),
            t => bail!("unknown event opcode {t:?}"),
        };
        Ok(Some(kind))
    }

    fn checkchan(&mut self) -> Result<u8> {
        if self.next() != Tok::Ch {
            bail!("expected ch=");
        }
        let n = self.expect_int("channel")?;
        if !(1..=16).contains(&n) {
            bail!("channel must be between 1 and 16");
        }
        Ok((n - 1) as u8)
    }

    fn checknote(&mut self) -> Result<u7> {
        if self.next() != Tok::NoteP {
            bail!("expected n=/note=");
        }
        let v = match self.next() {
            Tok::Int(n) => n,
            Tok::NoteVal(p) => p,
            t => bail!("expected note value, got {t:?}"),
        };
        if !(0..=127).contains(&v) {
            bail!("note must be between 0 and 127");
        }
        Ok(u7::new(v as u8))
    }

    fn checkval(&mut self) -> Result<u7> {
        if self.next() != Tok::Val {
            bail!("expected v=/val=/vol=");
        }
        let v = self.expect_int("value")?;
        if !(0..=127).contains(&v) {
            bail!("value must be between 0 and 127");
        }
        Ok(u7::new(v as u8))
    }

    fn checkcon(&mut self) -> Result<u7> {
        if self.next() != Tok::Con {
            bail!("expected c=/con=");
        }
        let v = self.expect_int("controller")?;
        if !(0..=127).contains(&v) {
            bail!("controller must be between 0 and 127");
        }
        Ok(u7::new(v as u8))
    }

    fn checkprog(&mut self) -> Result<u7> {
        if self.next() != Tok::Prog {
            bail!("expected p=/prog=");
        }
        let v = self.expect_int("program")?;
        if !(0..=127).contains(&v) {
            bail!("program must be between 0 and 127");
        }
        Ok(u7::new(v as u8))
    }

    fn splitval(&mut self) -> Result<PitchBend> {
        if self.next() != Tok::Val {
            bail!("expected v=/val=");
        }
        let v = self.expect_int("pitch bend")?;
        if !(0..=16383).contains(&v) {
            bail!("pitch bend must be between 0 and 16383");
        }
        Ok(PitchBend(u14::new(v as u16)))
    }

    /// An integer constrained to a single MIDI data byte (0..=127).
    fn getbyte(&mut self, what: &str) -> Result<u8> {
        let v = self.expect_int(what)?;
        if !(0..=127).contains(&v) {
            bail!("{what} value {v} out of range 0..=127");
        }
        Ok(v as u8)
    }
}

/// Exact base-2 logarithm of a power of two (`4` → `2`), else `None`.
fn log2_exact(mut n: i64) -> Option<u8> {
    if n <= 0 {
        return None;
    }
    let mut e = 0u8;
    while n > 1 {
        if n & 1 != 0 {
            return None;
        }
        n >>= 1;
        e += 1;
    }
    Some(e)
}
