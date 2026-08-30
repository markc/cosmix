# cosmix-midicomp

`cosmix-midicomp` converts Standard MIDI Files (SMF) to a plain-text event
format and compiles that text back to SMF bytes. It provides a Rust library and
a thin command-line program. In the `bus <- mix <- cos` dependency chain, the
crate belongs to the `cos` layer; it does not itself depend on `bus` or `mix`.

## Synopsis

Decode an SMF file to standard output:

```text
cosmix-midicomp song.mid
cosmix-midicomp song.mid > song.asc
```

Decode SMF data from standard input:

```text
cosmix-midicomp < song.mid
```

Compile text to an SMF file:

```text
cosmix-midicomp --compile song.asc song.mid
```

Compile text from standard input:

```text
cosmix-midicomp --compile song.mid < song.asc
```

See [Text format](text-format.md) for the accepted event grammar.

## Description

The default direction decodes SMF bytes to text. The decoder is a
bounds-checked implementation of the tolerant `midicomp` reader behaviour. It
zero-pads short fixed-length meta events, joins split SysEx packets, and
continues through the rest of a track after a malformed event where possible.

Compile mode tokenises the text grammar, parses it into owned events, builds a
`midly::Smf`, and serialises the result. It supports SMF formats 0, 1, and 2,
metrical PPQN division, and SMPTE timecode division.

The plain output form is canonical. Decoding an SMF produced by the compiler
and compiling it again preserves the generated text and SMF bytes. Arbitrary
input files are not promised to round-trip byte for byte: the writer uses
minimal running status and canonical variable-length quantities.

The crate handles MIDI 1.0 Standard MIDI Files. Universal MIDI Packets and
MIDI Clip Files are outside its format surface.

## Library interface

The library crate is named `cosmix_midicomp`.

| Item | Purpose |
|---|---|
| `Options` | Selects text rendering and time parsing behaviour. |
| `decode_smf_to_text` | Converts an SMF byte slice to text and reports malformed input. |
| `encode_text_to_smf` | Compiles text into a new SMF byte vector. |

### `Options`

```rust
use cosmix_midicomp::{Options, decode_smf_to_text, encode_text_to_smf};

let options = Options::default();
let (text, malformed) = decode_smf_to_text(&smf_bytes, &options)?;
let canonical_bytes = encode_text_to_smf(&text, &options)?;
```

`Options::default()` selects absolute tick positions, numeric notes, compact
columns, and no payload folding.

| Field | Type | Effect |
|---|---|---|
| `verbose` | `bool` | Aligns decoded columns and renders note names. |
| `note` | `bool` | Renders decoded note values as symbolic names. |
| `time` | `bool` | Renders decoded time as absolute bar, beat, and click values. |
| `inc` | `bool` | Decodes and parses event time as an incremental delta. |
| `fold` | `Option<usize>` | Folds decoded SysEx, string, and hexadecimal payloads at the given column. |

`decode_smf_to_text` returns `(String, bool)`. The Boolean is `true` when the
input contains malformed MIDI data, even when useful text was recovered.
Structural failures are returned as `anyhow::Error`.

`encode_text_to_smf` returns the complete SMF as `Vec<u8>`. Invalid headers,
track counts, event ranges, strings, payloads, and time sequences return an
error.

## Internal modules

The implementation modules are private.

| Module | Responsibility |
|---|---|
| `decode` | Reads SMF chunks and events and renders canonical text. |
| `encode` | Parses event lines, validates values, builds `midly` events, and writes SMF bytes. |
| `lex` | Tokenises keywords, parameters, numbers, notes, strings, comments, and folded lines. |

Library callers use the two crate-level conversion functions rather than these
modules directly.

## Command-line interface

The command has no subcommands, Bus verbs, or configuration file. Flags,
positional files, and standard streams define each conversion.

The binary accepts zero or one positional file in decode mode:

| Form | Input | Output |
|---|---|---|
| `cosmix-midicomp IN.mid` | `IN.mid` | Text on standard output |
| `cosmix-midicomp` | Standard input | Text on standard output |

Compile mode accepts one or two positional files:

| Form | Input | Output |
|---|---|---|
| `cosmix-midicomp -c IN.asc OUT.mid` | `IN.asc` | `OUT.mid` |
| `cosmix-midicomp -c OUT.mid` | Standard input | `OUT.mid` |

| Option | Long form | Meaning |
|---|---|---|
| `-c` | `--compile` | Compile text to SMF instead of decoding SMF. |
| `-d` | `--debug` | Write execution tracing to standard error. |
| `-v` | `--verbose` | Align output columns and enable note names. |
| `-n` | `--note` | Render decoded note values symbolically. |
| `-t` | `--time` | Render decoded absolute time as bar, beat, and click. |
| `-i` | `--inc` | Use incremental event times when decoding or compiling. |
| `-f N` | `--fold N` | Fold decoded payloads at column `N`. |

Verbose mode implies symbolic note rendering. Debug output does not replace
normal conversion output.

On malformed SMF input, the command writes all recovered text to standard
output, emits an error, and exits unsuccessfully. Compile mode also exits
unsuccessfully when the text cannot be validated or written.

## Cargo features

| Feature | Default | Effect |
|---|---|---|
| `cli` | Yes | Enables `clap` and builds the `cosmix-midicomp` binary. |

Library-only users can disable default features to omit the command-line
dependency:

```toml
[dependencies]
cosmix-midicomp = { version = "0.3", default-features = false }
```

## Dependencies

| Dependency | Role |
|---|---|
| `midly` | Represents and writes compiled SMF data. |
| `anyhow` | Carries conversion and I/O errors. |
| `clap` | Parses command-line arguments when the `cli` feature is enabled. |

The decode path reads SMF bytes directly to preserve its tolerant behaviour.
The encode path delegates SMF serialisation to `midly`.

## Text stability

The examples shipped with the crate contain canonical `.asc` and `.mid`
pairs. For those pairs:

```text
decode(mid) == asc
compile(asc) == mid
```

This property applies to files in the compiler's canonical form. It does not
claim that every valid SMF encoding preserves its original byte layout.
