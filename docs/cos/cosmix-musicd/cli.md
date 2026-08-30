# cosmix-musicd command-line interface

## Name

`cosmix-musicd` — render and play MIDI, inspect and reshape SoundFonts, simulate the mixer, or run the Bus service.

## Synopsis

```text
cosmix-musicd <command> [options]
```

Available commands depend on Cargo features.

## Core commands

The following commands build without the `cosmix` feature.

### render

```text
cosmix-musicd render <midi> <out> --soundfont <file> [options]
```

Renders a Standard MIDI File through a SoundFont.

Options:

| Option | Meaning |
|---|---|
| `-s, --soundfont <file>` | Required fallback or complete SF2 bank. |
| `--sample-rate <hz>` | Output sample rate; defaults to 44100. |
| `--gain <factor>` | Output gain multiplier; defaults to 1.0. |
| `--channels <layout>` | `stereo`, `mono`, `left`, or `right`. |
| `--format <format>` | `wav16`, `wav24`, `wavf32`, or `flac24`. |
| `--per-track` | Treat `<out>` as a directory and render one file per note-bearing SMF track. |
| `--library <dir>` | Resolve presets from a split SoundFont library. |

Without `--per-track`, `--library` scans the whole song and assembles an in-memory orchestra bank. Missing presets come from `--soundfont`. If the required identity cannot be represented in SF2, rendering falls back to the full bank.

With `--per-track`, each stem carries the tempo and time-signature conductor data. Track 0 becomes a stem only when it contains sounding notes.

Examples:

```text
cosmix-musicd render song.mid song.wav --soundfont general.sf2
cosmix-musicd render song.mid stems --soundfont general.sf2 --per-track
cosmix-musicd render song.mid mix.flac --soundfont general.sf2 --format flac24
```

### presets

```text
cosmix-musicd presets --soundfont <file>
```

Prints each preset as bank, program, and name, followed by the total count. SF3 input requires the `sf3` feature.

### merge

```text
cosmix-musicd merge --out <file.sf2> --take <spec> [--take <spec> ...]
```

Builds one multi-preset SF2 bank from selected presets.

A take has this form:

```text
BANK:PROGRAM[=NEW_BANK:NEW_PROGRAM]@PATH
```

The optional new identity retags the extracted preset. Output identities must be unique.

Example:

```text
cosmix-musicd merge --out orchestra.sf2 \
  --take 0:0@piano.sf2 \
  --take 0:32=0:36@bass.sf2
```

### split

```text
cosmix-musicd split --soundfont <file> --out-dir <dir> [options]
```

Extracts each selected preset into a standalone SF2 file.

Options:

| Option | Meaning |
|---|---|
| `-s, --soundfont <file>` | Input SF2, or SF3 when the `sf3` feature is enabled. |
| `-o, --out-dir <dir>` | Output directory; created when absent. |
| `--bank <number>` | Select one bank. |
| `--program <number>` | Select one program in the range 0 through 127. |
| `--as <bank:program>` | Retag the single selected preset. |
| `--normalize <dbfs>` | Reduce loudness to the given rendered RMS target. |
| `--levels <file>` | Write a TSV report of measured levels and output files. |

`--as` requires the filters to select exactly one preset. `--normalize` never boosts sample data; presets already below the target remain unchanged.

Generated names use:

```text
{bank:03}-{program:03}-{slug}.sf2
```

### mixer-sim

```text
cosmix-musicd mixer-sim [options]
```

Runs the fixed 32-channel block processor with deterministic multitone sources. It requires neither an audio device nor a Bus broker.

Options:

| Option | Meaning |
|---|---|
| `--frames <count>` | Number of 60 Hz meter frames; defaults to 120. |
| `--print-every <count>` | Summary interval; defaults to 30. Zero suppresses intermediate summaries. |
| `--out <file>` | Write concatenated 465-byte meter frames. |

Simulator frames carry the simulator flag and are not real mixer-daemon output.

## Playback commands

These commands require `playback`, which is enabled by default.

### play

```text
cosmix-musicd play <midi> --soundfont <file> [--gain <factor>]
```

Streams the MIDI file through the SoundFont to the default output device and returns at end of playback.

### live

```text
cosmix-musicd live
```

Connects an available MIDI input to the built-in 16-voice sine synthesiser. Pressing Enter ends the session.

## Bus commands

These commands require `cosmix`.

### serve

```text
cosmix-musicd serve [-c <config.conf.mix>]
```

Runs the MIDI render and playback Bus citizen as service `musicd`.

When `-c` or `--config` is absent, the daemon checks its system configuration and then its user service configuration.

See [Bus service and configuration](bus.md).

### mixer-serve

```text
cosmix-musicd mixer-serve [--autoplay] [--stems <session>]
```

Runs the real 32-channel mixer Bus citizen as service `musicd`. Do not run it beside `serve` on the same broker because both register the same identity.

Options:

| Option | Meaning |
|---|---|
| `--autoplay` | Starts transport immediately for demonstrations or tests. This mode is not benchmark-eligible. |
| `--stems <session>` | Loads and verifies a stem-session source before the audio thread starts. |

Without `--stems`, the mixer uses its deterministic multitone source profile.

Stem sessions accept `stem-session.v1` JSON or `stem-session.v2` strict-data `.mix` files. Sources must be mono 48 kHz WAV files with matching SHA-256 values.

## Exit status

The process returns success after a command completes.

Invalid arguments are rejected by the command-line parser. Runtime failures include unreadable files, invalid MIDI or SoundFont data, output errors, missing audio devices, rejected stem sessions, and Bus startup errors.

