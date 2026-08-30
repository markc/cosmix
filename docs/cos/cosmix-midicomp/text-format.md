# cosmix-midicomp text format

The format represents an SMF header, one or more tracks, and one event per line.
Keywords are case-insensitive. Strings retain their original case.

## File structure

Every input begins with an `MFile` header:

```text
MFile FORMAT TRACKS DIVISION
```

`FORMAT` is `0`, `1`, or `2`. `TRACKS` is the number of following track
blocks. A non-negative `DIVISION` is the PPQN value and must fit the SMF
15-bit metrical range.

A negative division selects SMPTE timing and requires a ticks-per-frame value:

```text
MFile 1 2 -25 40
```

The supported frame rates are 24, 25, 29, and 30 frames per second. The
ticks-per-frame value is in the range 0 through 255.

Each track is delimited by `MTrk` and `TrkEnd`:

```text
MFile 0 1 480
MTrk
0 Meta TrkName "Example"
0 PrCh ch=1 p=0
0 On ch=1 n=60 v=96
480 Off ch=1 n=60 v=0
480 Meta TrkEnd
TrkEnd
```

The declared track count must match the number of track blocks. Format 0
requires exactly one track.

The compiler adds an end-of-track meta event when the track does not already
end with `Meta TrkEnd`.

## Event time

Every event line starts with a time value. Without `--inc`, the value is an
absolute tick position and must not move backwards.

With `--inc`, each value is the event's delta from the preceding event:

```text
0 On ch=1 n=60 v=96
480 Off ch=1 n=60 v=0
```

Bar, beat, and click time uses either `:` or `/` separators:

```text
1:0:0 On ch=1 n=c4 v=96
2/0/0 Off ch=1 n=c4 v=0
```

The time-signature events seen so far define the bar and beat calculation.
`--time` makes the decoder emit this form.

## Channel events

| Event | Syntax |
|---|---|
| Note on | `On ch=N n=NOTE v=N` |
| Note off | `Off ch=N n=NOTE v=N` |
| Polyphonic pressure | `PoPr ch=N n=NOTE v=N` |
| Channel pressure | `ChPr ch=N v=N` |
| Controller change | `Par ch=N c=N v=N` |
| Pitch bend | `Pb ch=N v=N` |
| Program change | `PrCh ch=N p=N` |

Channels are numbered 1 through 16. Notes, velocities, pressures, controllers,
and programs use the MIDI data-byte range 0 through 127. Pitch bend uses the
combined 14-bit range 0 through 16383.

Parameter aliases are accepted:

| Canonical | Accepted forms |
|---|---|
| Channel | `ch=` |
| Note | `n=`, `note=` |
| Value or velocity | `v=`, `val=`, `vol=` |
| Controller | `c=`, `con=` |
| Program | `p=`, `prog=` |

The lexer also accepts `PolyPr` for `PoPr`, `Param` for `Par`, `ProgCh` for
`PrCh`, and `ChanPr` for `ChPr`.

## System and meta events

| Event | Syntax |
|---|---|
| System exclusive | `SysEx HEX` |
| Arbitrary F7 escape bytes | `Arb HEX` |
| Sequence number | `SeqNr N` |
| Key signature | `KeySig N major` or `KeySig N minor` |
| Tempo | `Tempo N` |
| Time signature | `TimeSig NUM/DEN CLOCKS THIRTYSECONDS` |
| SMPTE offset | `SMPTE H M S F FF` |
| Text-family meta | `Meta TYPE "STRING"` |
| End of track meta | `Meta TrkEnd` |
| Sequencer-specific meta | `SeqSpec HEX` |
| Other meta | `Meta 0xNN HEX` |

`KeySig` accepts values from `-7` through `7`.

`Tempo` is microseconds per quarter note and occupies the SMF 24-bit range.

The time-signature numerator is 1 through 255. Its denominator must be a
positive power of two. The clocks and thirty-seconds values are MIDI data
bytes.

`SMPTE` takes five MIDI data-byte values.

The named text-family types are:

```text
Text Copyright SeqName TrkName InstrName Lyric Marker Cue
```

`SeqName` and `TrkName` select the same meta-event type.

## Numbers and notes

Integers can be decimal, such as `127`, or hexadecimal, such as `0x7f`.

Bank notation begins with `$` or `'`. It uses base 8 with `1` through `8` or
`a` through `h` representing digits 0 through 7.

Notes can be numeric or symbolic, such as `n=60`, `n=c5`, `n=f#4`, or `n=eb3`.

A symbolic note uses `A` through `G`, an optional accidental, and an octave
with no separating space. `#` or `+` sharpens the note. `b` or `-` flattens
it. The crate's convention maps `c4` to MIDI note 48.

## Payloads and strings

Hexadecimal payloads may contain spaces or run together as pairs:

```text
0 SysEx f0 7e 00 09 01 f7
0 SeqSpec 010203
```

Where a payload is accepted, a quoted string can supply the bytes instead.
Strings support these escapes:

| Escape | Value |
|---|---|
| `\"` | Double quote |
| `\\` | Backslash |
| `\0` | Zero byte |
| `\r` | Carriage return |
| `\n` | Line feed |
| `\t` | Tab |
| `\xHH` | Byte written in hexadecimal |

The decoder escapes quotes, backslashes, zero bytes, line endings, and other
non-printable bytes when it renders a string.

`--fold N` wraps long decoded strings and hexadecimal payloads. A folded line
ends with a backslash and continues on the following indented line. The
compiler joins this form before parsing the event.

## Lexical rules

Blank lines are accepted. Input lines must be newline-terminated.

`#` starts a comment outside strings and symbolic note accidentals. The comment
continues to the end of the physical line.

Keywords and parameter names are case-insensitive. Quoted string contents are
not case-folded.

An unterminated string, malformed hexadecimal tail, out-of-range value,
unsupported format, mismatched track count, backwards time, or excessive SMF
delta causes compilation to fail.

## Canonical output

Plain decode output uses absolute ticks, numeric notes, compact spacing, and no
folding. This is the canonical text form used by the crate's round-trip corpus.

The compiler writes minimal running status and canonical variable-length
quantities through `midly`. Decoding and recompiling compiler-produced files is
byte-stable; arbitrary equivalent SMF encodings may be normalised.

[Back to cosmix-midicomp](README.md)
