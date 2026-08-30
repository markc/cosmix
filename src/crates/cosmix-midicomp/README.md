# cosmix-midicomp

SMF (Standard MIDI File) ⇄ plain-text converter — an **MIT-licensed** Rust port
of [`midicomp`](https://github.com/markc/midicomp) (the MIT C tool by the same
author, Mark Constable, after mf2t/t2mf by Piet van Oostrum).

The **decode** path (SMF → text) is a faithful port of the C reader's *tolerant*
behaviour: short fixed-length meta events are zero-padded, split SysEx packets
are merged, and a malformed event never discards the rest of its track. The
**encode** path (text → SMF) tokenises and parses the documented text grammar
and serialises through the [`midly`](https://crates.io/crates/midly) SMF writer,
whose minimal running-status output makes the canonical round-trip byte-exact.

## Usage

```
cosmix-midicomp song.mid              # SMF → text (stdout)
cosmix-midicomp song.mid > song.asc   # SMF → text (file)
cosmix-midicomp -c song.asc song.mid  # text → SMF
cosmix-midicomp -c song.mid < song.asc# text → SMF (text on stdin)
```

Flags (from the original): `-d/--debug` (trace to stderr), `-v/--verbose`
(columns + note names), `-c/--compile` (text→SMF), `-n/--note` (symbolic note
names), `-t/--time` (absolute time not ticks), `-i/--inc` (incremental/delta
time instead of absolute), `-fN/--fold=N` (fold sysex/strings at N columns).

## Text format (the spec to implement against)

```
File header:          MFile <format> <ntrks> <division>
Start of track:       MTrk
End of track:         TrkEnd

Note On:              On  <ch> <note> <vol>
Note Off:             Off <ch> <note> <vol>
Poly Pressure:        PoPr  <ch> <note> <val>
Channel Pressure:     ChPr  <ch> <val>
Controller:           Par   <ch> <con> <val>
Pitch bend:           Pb    <ch> <val>          (val = combined 14-bit)
Program change:       PrCh  <ch> <prog>
Sysex:                SysEx <hex>               (incl. leading F0, trailing F7)
Arbitrary bytes:      Arb   <hex>

Sequence nr:          Seqnr  <num>
Key signature:        KeySig <num> <major|minor>
Tempo:                Tempo  <num>              (32-bit µs/quarter)
Time signature:       TimeSig <n>/<n> <n> <n>
SMPTE offset:         SMPTE  <n> <n> <n> <n> <n>
Meta text:            Meta <texttype> "<string>"
Meta end of track:    Meta TrkEnd
Sequencer specific:   SeqSpec <type> <hex>
Misc meta:            Meta <type> <hex>
```

Param forms: `<ch>`=`ch=N`; `<note>`=`n=NOTE`/`note=NOTE`; `<vol>`=`v=N`/`vol=N`;
`<val>`=`v=N`/`val=N`; `<con>`=`c=N`/`con=N`; `<prog>`=`p=N`/`prog=N`.
`<texttype>` ∈ Text/Copyright/SeqName/TrkName/InstrName/Lyric/Marker/Cue.
`<type>` = `0xab`. `<hex>` = space-separated 2-digit hex (or no spaces if exactly
2 digits each). `<string>` = double-quoted.

Input rules (text→SMF): channels 1-based; numbers decimal, `0x`hex, or `'`bank
(octal-ish, digits 1-8, letters a-h=1-8); symbolic notes `A-G` + `#`/`+` sharp,
`b`/`-` flat, then octave with no space; string escapes `\" \\ \0 \r \n \xHH \t`;
`#` starts a comment to end-of-line (except in strings / as a note sharp); bar:beat:click
time may use `:` or `/`; folded lines end with `\` and continue on a `\t` line;
case-insensitive except inside strings; blank lines OK, newlines required.

String output escaping (SMF→text): `"` and `\` → `\"`/`\\`; zero → `\0`; CR/LF →
`\r`/`\n`; other non-printables → `\xHH`; with `-f`, fold long strings/hex with
`\<newline><tab>`.

## Byte-in == byte-out

A Standard MIDI File is decoded to text and recompiled back to a byte-identical
SMF — **for any file already in the tool's own canonical form** (the form it
emits when it compiles). `midly`'s writer always uses minimal running status, so
a file authored *without* running status is re-emitted *with* it, and a
non-canonical variable-length delta is re-canonicalised; those classes do not
round-trip byte-for-byte and that is by design (the output is canonical, not a
verbatim copy). The `examples/songs/` corpus is a dozen short pieces across
musical styles, each a canonical `.asc`/`.mid` pair, and `tests/examples.rs`
asserts the round-trip is byte-exact in both directions for every one.

## MIDI 1.0 / MIDI 2.0

This is a Standard MIDI File tool, and SMF is a **MIDI 1.0** container — every
event type here is MIDI 1.0. MIDI 2.0 (the Universal MIDI Packet and the MIDI
Clip File / "SMF2") is a different binary format with no widely-deployed file
form yet, and is out of scope; `midly` does not implement UMP. The goal here is
exhaustive, accurate MIDI-1.0 SMF handling.

## Status

Both directions are implemented and tested. `cargo test -p cosmix-midicomp`
runs the four-mode suite from the original project, the ported `smpte` and
`security` adversarial fixtures, a synthetic all-events round-trip, and the
`examples/songs/` byte-in == byte-out corpus:

- [x] MIT license, workspace member, builds clean (zero clippy warnings).
- [x] CLI surface (all flags incl. `-i/--inc`) + file/stdin/stdout plumbing + mode dispatch.
- [x] `decode` (`src/decode.rs`) — a tolerant SMF reader ported from the C, with `-t` time, `-i` incremental time, `-n` note names, `-v` columns, `-f` fold.
- [x] `encode` (`src/encode.rs` + `src/lex.rs`) — tokeniser + parser → `midly::Smf` → bytes.
- [x] Number parsing: decimal / `0x`hex / `$`|`'`bank; symbolic note ⇄ number.
- [x] String escape/unescape and hex folding (both directions).
- [x] Round-trip tests: plain/verbose golden match, text idempotence, byte-stable SMF, 12-song corpus.
- [x] Malformed-input robustness: short/zero-length metas zero-padded, no panics, ported adversarial corpus.
- [x] Format 0/1/2; division as PPQN or SMPTE (negative fps + ticks/frame).

### A note on `tests/ex1.mid`

The upstream `ex1.mid` carried a **malformed 2-byte time signature**
(`FF 58 02 04 02` — length 2, where SMF requires 4). The original C tool read
two bytes past the event into a reused buffer, so its golden text recorded
`TimeSig 4/4 32 99` where `32 99` were leftover bytes from the preceding events.
This crate's tolerant decoder zero-pads a short meta (decoding the same 2-byte
event as `TimeSig 4/4 0 0`), so it cannot reproduce that garbage. This crate's
`ex1.mid` corrects the event to its well-formed 4-byte form
`FF 58 04 04 02 20 63` — `0x20`=32, `0x63`=99, the exact numbers the golden text
already records — so the golden files stay byte-identical to upstream while the
input is now a valid SMF.

> Reference: `~/.gh/midicomp/` has the original C (`midicomp.c`, `t2mf.fl`) and
> the fixtures. Both projects are MIT and share an author; the decode path is a
> direct port of the C reader's behaviour, the encode path is built on `midly`.
