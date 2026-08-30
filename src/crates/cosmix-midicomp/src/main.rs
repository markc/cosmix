//! cosmix-midicomp — SMF ⇄ plain-text converter.
//!
//! An MIT-licensed Rust port of `midicomp` (the MIT C tool by Mark Constable,
//! after mf2t/t2mf by Piet van Oostrum). The decode (SMF → text) path is a
//! faithful port of the C reader's tolerant behaviour; the encode (text → SMF)
//! path tokenises and parses the documented text grammar and serialises through
//! the `midly` SMF writer (whose minimal running-status output makes the
//! canonical round-trip byte-exact). See `README.md` for the text format.
//!
//! Two directions:
//!   * SMF → text  (default): `cosmix-midicomp song.mid > song.asc`
//!   * text → SMF  (`-c`):    `cosmix-midicomp -c song.asc song.mid`
//!
//! This is the thin CLI over the library crate ([`cosmix_midicomp`]), which
//! holds both converter cores and the shared [`Options`].

use anyhow::{Context, Result, bail};
use clap::Parser;
use cosmix_midicomp::{Options, decode_smf_to_text, encode_text_to_smf};
use std::io::{self, Read, Write};
use std::path::PathBuf;

/// SMF (Standard MIDI File) ⇄ plain-text converter.
#[derive(Parser, Debug)]
#[command(name = "cosmix-midicomp", version, about)]
struct Cli {
    /// Send debug output to stderr.
    #[arg(short, long)]
    debug: bool,
    /// Verbose text: aligned columns, note names on.
    #[arg(short, long)]
    verbose: bool,
    /// Compile ascii text input into an SMF.
    #[arg(short, long)]
    compile: bool,
    /// Note on/off values as symbolic note|octave.
    #[arg(short, long)]
    note: bool,
    /// Use absolute time instead of ticks.
    #[arg(short, long)]
    time: bool,
    /// Read/write incremental (delta) time values instead of absolute.
    #[arg(short, long)]
    inc: bool,
    /// Fold sysex / long strings at N columns.
    #[arg(short, long, value_name = "N")]
    fold: Option<usize>,
    /// Positional file args (meaning depends on `-c`; see README).
    #[arg(value_name = "FILE")]
    files: Vec<PathBuf>,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let debug = cli.debug;
    if debug {
        eprintln!("main()");
    }
    let opts = Options {
        verbose: cli.verbose,
        note: cli.note || cli.verbose, // -v implies note names
        time: cli.time,
        inc: cli.inc,
        fold: cli.fold,
    };

    if cli.compile {
        // text → SMF.  `-c IN.asc OUT.mid`  OR  `-c OUT.mid` (text on stdin).
        let (text, output): (String, PathBuf) = match cli.files.as_slice() {
            [inp, outp] => {
                if debug {
                    eprintln!("compiling {} to {}", inp.display(), outp.display());
                }
                let text = std::fs::read_to_string(inp)
                    .with_context(|| format!("opening input {}", inp.display()))?;
                (text, outp.clone())
            }
            [outp] => {
                if debug {
                    eprintln!("compiling stdin to {}", outp.display());
                }
                let mut text = String::new();
                io::stdin().read_to_string(&mut text)?;
                (text, outp.clone())
            }
            _ => bail!("compile mode: expected `IN.asc OUT.mid`, or `OUT.mid` with text on stdin"),
        };
        let smf = encode_text_to_smf(&text, &opts)?;
        std::fs::write(&output, smf)
            .with_context(|| format!("writing SMF {}", output.display()))?;
    } else {
        // SMF → text.  `IN.mid` (→ stdout)  OR  stdin.
        let bytes = match cli.files.as_slice() {
            [inp] => {
                if debug {
                    eprintln!("decoding {}", inp.display());
                }
                std::fs::read(inp).with_context(|| format!("reading SMF {}", inp.display()))?
            }
            [] => {
                if debug {
                    eprintln!("decoding stdin");
                }
                let mut b = Vec::new();
                io::stdin().read_to_end(&mut b)?;
                b
            }
            _ => bail!("decode mode: expected a single SMF file argument (or stdin)"),
        };
        let (text, malformed) = decode_smf_to_text(&bytes, &opts)?;
        let mut out = io::stdout();
        out.write_all(text.as_bytes())?;
        out.flush()?;
        if malformed {
            // The decodable output has already been written; signal the
            // corruption with a warning and a non-zero exit (as the C tool does).
            bail!("input contained malformed MIDI data; decoded output may be incomplete");
        }
    }
    Ok(())
}
