# Example songs — the round-trip test corpus

A dozen short (~30 s) pieces spanning musical styles — plus `aurora`, a longer
(~3 min) ambient showcase — used by `tests/examples.rs` to pin the
**byte-in == byte-out** guarantee of the converter. Each song is a **canonical
pair**:

| File | What it is |
|---|---|
| `<name>.asc` | the plain-text form **exactly as `cosmix-midicomp` decodes it** |
| `<name>.mid` | the Standard MIDI File `cosmix-midicomp -c` compiles that text into |

Each `.mid` is produced by compiling its own `.asc`, so the pair is a fixed
point: `decode(mid) == asc` **and** `compile(asc) == mid` (byte-for-byte). The
`.asc` files are hand-authored as real (if short) music, then normalised to the
decoder's canonical output so both directions hold.

Regenerate the pair for one song after editing its `.asc`:

```sh
cosmix-midicomp -c songs/<name>.asc songs/<name>.mid   # text -> canonical SMF
cosmix-midicomp songs/<name>.mid > songs/<name>.asc     # SMF -> canonical text
```

(run the second line twice if the first edit wasn't already canonical, then
confirm `cargo test -p cosmix-midicomp` is green).

## Coverage matrix

The corpus is chosen so that, between them, the songs exercise **every event
type and option** the grammar supports — not just common note traffic.

| Song | Style | Fmt | Metre | Notable events exercised |
|---|---|---|---|---|
| `rock-anthem`     | Rock          | 1 | 4/4  | multi-track, PrCh, NoteOn/Off, drums (ch10) |
| `blues-shuffle`   | Blues shuffle | 1 | 4/4  | Pb (pitch bend), Par (CC7 volume), shuffle timing |
| `jazz-swing`      | Jazz          | 1 | 4/4  | ChPr (channel pressure), walking bass, swing |
| `bossa-nova`      | Bossa nova    | 1 | 4/4  | PoPr (poly pressure), nylon guitar, soft drums |
| `techno-pulse`    | Techno        | 0 | 4/4  | single-track, CC filter sweeps, Pb, four-on-floor |
| `vienna-waltz`    | Waltz         | 1 | 3/4  | 3/4 TimeSig + bar:beat:click maths, strings |
| `funk-groove`     | Funk          | 1 | 4/4  | 16th ghost notes (low vel), clavinet, slap bass |
| `classical-minuet`| Classical     | 1 | 3/4  | KeySig (sharps **and** a minor key), harpsichord |
| `chiptune-8bit`   | Chiptune      | 0 | 4/4  | SysEx (GM reset), Tempo change, Pb vibrato, arps |
| `ambient-drift`   | Ambient       | 1 | 4/4  | multiple Tempo events, CC64 sustain, long notes, Marker/Lyric |
| `military-march`  | March         | 1 | 2/4  | 2/4 TimeSig, snare rolls, brass, Copyright/InstrName |
| `reggae-skank`    | Reggae        | 1 | 4/4  | offbeat skank, SeqSpec, Arb (F7 escape), MidiPort meta |
| `aurora`          | Ambient (long)| 1 | 4/4  | 17-track layering, ~24 GM programs w/ mid-track PrCh swaps, CC pan/reverb/chorus/volume/sustain, ~3 min |

Rarer metadata is spread across the corpus so the suite touches the long tail:
`SeqNr`, `SMPTE` offset, channel-prefix (`Meta 0x20`), MIDI-port (`Meta 0x21`),
misc meta (`Meta 0xNN`), text-family metas (Text/Copyright/TrkName/InstrName/
Lyric/Marker/Cue), `KeySig` minor, and a SMPTE-division header all appear in at
least one song. See the per-song coverage noted at the top of each `.asc`.
