//! cosmix-musicd CLI.
//!
//! `render` works everywhere (pure, no audio device). `play` / `live` need the
//! `playback` feature + an audio device. `serve` (the Bus daemon) needs the
//! `cosmix` feature. See `lib.rs` for the layering.

use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};

use cosmix_musicd::render::{self, RenderOptions};

#[derive(Parser)]
#[command(
    name = "cosmix-musicd",
    version,
    about = "MIDI → SoundFont render/play + live sine synth"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Render a MIDI file through a SoundFont (pure; no audio device).
    Render {
        /// Input Standard MIDI File (.mid)
        midi: PathBuf,
        /// Output audio path (.wav or .flac)
        out: PathBuf,
        /// SoundFont (.sf2) to synthesize with
        #[arg(short, long)]
        soundfont: PathBuf,
        /// Output sample rate in Hz
        #[arg(long, default_value_t = render::RenderOptions::default().sample_rate)]
        sample_rate: i32,
        /// Output gain multiplier (lower if it clips)
        #[arg(long, default_value_t = 1.0)]
        gain: f32,
        /// Output channel layout: stereo, mono (L+R average), left, or right.
        #[arg(long, value_enum, default_value_t = render::Channels::Stereo)]
        channels: render::Channels,
        /// Output format: wav16, wav24, wavf32, or flac24.
        #[arg(long, value_enum, default_value_t = render::RenderFormat::Wav16)]
        format: render::RenderFormat,
        /// Render one stem per SMF track into OUT (treated as a directory).
        /// Each stem carries track 0's tempo map; a note-bearing track 0
        /// becomes its own stem.
        #[arg(long)]
        per_track: bool,
        /// Resolve fonts from this `split` library ({bank:03}-{prog:03}-*.sf2,
        /// GM fallback). With --per-track: one font per stem. Without: scan
        /// the song's identities, auto-assemble a merged orchestra bank in
        /// memory and render with it (gaps filled from --soundfont).
        #[arg(long, value_name = "DIR")]
        library: Option<PathBuf>,
    },

    /// List a SoundFont's presets (bank, program, name) without extracting.
    Presets {
        /// SoundFont (.sf2, or .sf3 with the `sf3` feature) to inspect
        #[arg(short, long)]
        soundfont: PathBuf,
    },

    /// Merge chosen presets from any fonts into one bank — the inverse of
    /// `split`, for whole-song renders with a mix-and-match orchestra.
    Merge {
        /// Output .sf2 path (its stem becomes the bank name)
        #[arg(short, long)]
        out: PathBuf,
        /// A take: `BANK:PROG@FONT.sf2` picks that preset; add `=NB:NP`
        /// before the `@` to retag it (e.g. `--take 0:0=0:36@bass.sf2`).
        /// Repeatable; resulting identities must be unique.
        #[arg(long, required = true, value_name = "B:P[=NB:NP]@PATH")]
        take: Vec<String>,
    },

    /// Split a SoundFont into one .sf2 per preset (tiny per-track fonts, so a
    /// per-track synth instance no longer loads the whole bank).
    Split {
        /// SoundFont (.sf2, or .sf3 with the `sf3` feature) to split
        #[arg(short, long)]
        soundfont: PathBuf,
        /// Directory for the per-preset .sf2 files (created if absent)
        #[arg(short, long)]
        out_dir: PathBuf,
        /// Only split presets in this bank (0 = GM melodic, 128 = GM drums)
        #[arg(long)]
        bank: Option<u16>,
        /// Only split this program number (0-127)
        #[arg(long)]
        program: Option<u16>,
        /// Retag the extract to BANK:PROG (e.g. `--as 0:36` makes any
        /// single-instrument font answer to GM slap bass). Needs the
        /// bank/program filter to select exactly one preset.
        #[arg(long = "as", value_name = "BANK:PROG")]
        retag: Option<String>,
        /// Normalize each extract's loudness DOWN to this RMS target (dBFS,
        /// e.g. -18): probed by rendering notes through the built-in synth,
        /// corrected by scaling sample PCM (host-independent, can't clip).
        /// Extracts already quieter than the target are left untouched.
        #[arg(long, value_name = "DBFS", allow_hyphen_values = true)]
        normalize: Option<f32>,
        /// Write a TSV of measured levels (bank, program, name, dBFS before,
        /// dBFS after, file) — useful for per-track fader trim instead of,
        /// or in addition to, --normalize.
        #[arg(long, value_name = "FILE")]
        levels: Option<PathBuf>,
    },

    /// Run the standalone 32-channel mixer SIMULATOR (D4/D7): drive the real
    /// block processor with deterministic multitone sources and emit A.6 meter
    /// frames with FLAG_SIMULATOR set. No audio device, no Bus broker — the
    /// scaffold dry-run backend. NEVER a benchmark entrant (mixer/engine =
    /// "simulator").
    MixerSim {
        /// Number of 60 Hz meter frames to generate.
        #[arg(long, default_value_t = 120)]
        frames: u64,
        /// Print a summary line every N frames (0 = silent; first + last always print).
        #[arg(long, default_value_t = 30)]
        print_every: u64,
        /// Write the raw concatenated 465-byte A.6 frames to this file.
        #[arg(long)]
        out: Option<PathBuf>,
    },

    /// Inspect and convert MIDI 2.0 UMP using the cos-internal `.cosump`
    /// working format. Standard MIDI File remains the interchange format.
    Midi2 {
        #[command(subcommand)]
        cmd: Midi2Cmd,
    },

    /// Play a MIDI file to the speakers through a SoundFont (needs an audio device).
    #[cfg(feature = "playback")]
    Play {
        /// Input Standard MIDI File (.mid)
        midi: PathBuf,
        /// SoundFont (.sf2) to synthesize with
        #[arg(short, long)]
        soundfont: PathBuf,
        /// Output gain multiplier
        #[arg(long, default_value_t = 1.0)]
        gain: f32,
    },

    /// Live: play your MIDI keyboard through the built-in sine synth.
    #[cfg(feature = "playback")]
    Live,

    /// Run the Bus citizen daemon (musicd.render / play / stop / status / props).
    #[cfg(feature = "cosmix")]
    Serve {
        /// Path to the musicd config (`.conf.mix`). Defaults to the system /
        /// user search order.
        #[arg(short = 'c', long)]
        config: Option<PathBuf>,
    },

    /// Run the REAL 32-channel mixer Bus daemon (the DSP half of the disp-skia
    /// bake-off): the mixer engine on an RT thread + a writeable `mixer.v1`
    /// control surface (`musicd.props.set`) + `dsp.applied` events + a 60 Hz
    /// meter publisher (`musicd.mixer.meters`). Distinct from `mixer-sim`
    /// (headless, prints frames) — this needs a broker. Registers as `musicd`,
    /// so do not run it alongside `serve`.
    #[cfg(feature = "cosmix")]
    MixerServe {
        /// Demo/test only: seed transport PLAYING at unity at startup, so the
        /// daemon immediately drives the audio device without an external
        /// authenticated write. The bake-off itself drives transport via the
        /// authenticated declarer, not this flag.
        #[arg(long)]
        autoplay: bool,
        /// Select the `stem-session.v1` source profile: load, hash-verify, and
        /// preload this JSON stem manifest (mono 48 kHz WAV per channel) before
        /// the audio stream starts. NON-BENCHMARK — every meter frame carries
        /// FLAG_NON_BENCH_SOURCE and mixer.benchmark_eligible=false. Absent =
        /// the frozen benchmark multitone profile (default). `--autoplay` works
        /// with either.
        #[arg(long, value_name = "MANIFEST")]
        stems: Option<PathBuf>,
    },
}

#[derive(Subcommand)]
enum Midi2Cmd {
    /// Decode `.cosump` or `.mid` into one stable, human-readable message per
    /// line. Format-1 MIDI files merge all tracks deterministically.
    Dump {
        /// Input `.cosump` working file or Standard MIDI File.
        file: PathBuf,
    },

    /// Up-translate a Standard MIDI File into cos-internal `.cosump`.
    ///
    /// By default all format-1 tracks are merged by tick, track index and
    /// event index. `--track N` selects one zero-based MIDI track while still
    /// retaining the file-global tempo map.
    Up {
        /// Input Standard MIDI File.
        input: PathBuf,
        /// Output cos-internal `.cosump` working file.
        output: PathBuf,
        /// Select one zero-based MIDI track instead of merging all tracks.
        #[arg(long)]
        track: Option<usize>,
    },

    /// Down-translate `.cosump` into canonical format-0 Standard MIDI File.
    ///
    /// MIDI 2-only losses are counted by kind on stderr; lossless conversion
    /// is silent.
    Down {
        /// Input cos-internal `.cosump` working file.
        input: PathBuf,
        /// Output canonical format-0 Standard MIDI File.
        output: PathBuf,
    },

    /// Check the load-bearing invariant over a merged SMF timeline.
    ///
    /// Compares semantic events, ticks-per-quarter division and the tempo map,
    /// not raw SMF bytes or original track topology. Exits 1 and prints the
    /// first divergence on mismatch.
    Roundtrip {
        /// Input Standard MIDI File.
        input: PathBuf,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Render {
            midi,
            out,
            soundfont,
            sample_rate,
            gain,
            channels,
            format,
            per_track,
            library,
        } => {
            let opts = RenderOptions {
                sample_rate,
                gain,
                channels,
                format,
                ..Default::default()
            };
            if per_track {
                let stems = cosmix_musicd::stems::render_per_track(
                    &midi,
                    &soundfont,
                    library.as_deref(),
                    &out,
                    &opts,
                )?;
                for s in &stems {
                    println!(
                        "  {} ({}:{} {}, {:.1}s, peak {:.3}{})",
                        s.file.display(),
                        s.bank,
                        s.program,
                        s.font
                            .file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                            .unwrap_or_default(),
                        s.report.duration_s,
                        s.report.peak,
                        if s.report.clipped { ", CLIPPED" } else { "" },
                    );
                }
                println!("rendered {} stem(s) into {}", stems.len(), out.display());
            } else {
                // Whole-song: with --library, auto-assemble the orchestra
                // bank in memory; else (or when the song needs the full
                // bank) render straight from --soundfont.
                let mut orchestra = None;
                if let Some(lib) = &library {
                    let midi_bytes = std::fs::read(&midi)
                        .map_err(|e| anyhow::anyhow!("open midi {}: {e}", midi.display()))?;
                    let inam = format!(
                        "{} orchestra",
                        midi.file_stem()
                            .map(|s| s.to_string_lossy())
                            .unwrap_or_default()
                    );
                    orchestra = cosmix_musicd::stems::assemble_orchestra(
                        &midi_bytes,
                        lib,
                        &soundfont,
                        &inam,
                    )?;
                    if orchestra.is_none() {
                        eprintln!("  note: song needs the full bank — rendering with --soundfont");
                    }
                }
                let r = match orchestra {
                    Some((bank, rep)) => {
                        println!(
                            "orchestra: {} from library, {} from fallback bank{}",
                            rep.from_library.len(),
                            rep.from_fallback.len(),
                            if rep.omitted.is_empty() {
                                String::new()
                            } else {
                                format!(", {} omitted", rep.omitted.len())
                            },
                        );
                        let sf = cosmix_musicd::synth::load_soundfont_bytes(bank)?;
                        let mid = cosmix_musicd::synth::load_midi(&midi)?;
                        let (left, right, r) = render::render_to_buffers(&sf, &mid, &opts)?;
                        render::write_render(
                            &out,
                            &left,
                            &right,
                            opts.sample_rate,
                            opts.channels,
                            opts.format,
                        )?;
                        r
                    }
                    None => render::render_midi_to_file(&midi, &soundfont, &out, &opts)?,
                };
                println!(
                    "rendered {} → {} ({:.1}s, {} frames @ {} Hz, peak {:.3}{})",
                    midi.display(),
                    out.display(),
                    r.duration_s,
                    r.frames,
                    r.sample_rate,
                    r.peak,
                    if r.clipped {
                        ", CLIPPED — lower --gain"
                    } else {
                        ""
                    },
                );
            }
        }

        Cmd::Presets { soundfont } => {
            let bytes = std::fs::read(&soundfont)
                .map_err(|e| anyhow::anyhow!("open soundfont {}: {e}", soundfont.display()))?;
            #[cfg(feature = "sf3")]
            let bytes = cosmix_musicd::sf3::maybe_decode_sf3(bytes)
                .map_err(|e| anyhow::anyhow!("decode SF3 {}: {e}", soundfont.display()))?;
            let presets = cosmix_musicd::sf2split::list_presets(&bytes)?;
            for p in &presets {
                println!("{:>3}:{:<3} {}", p.bank, p.program, p.name);
            }
            println!("{} preset(s) in {}", presets.len(), soundfont.display());
        }

        Cmd::Merge { out, take } => {
            let mut outs = Vec::new();
            for spec in &take {
                // B:P[=NB:NP]@PATH — identity before the FIRST '@'; the rest
                // is the path verbatim (paths may contain '@' themselves
                // only after the first, which never occurs in the identity).
                let (ident, path) = spec.split_once('@').ok_or_else(|| {
                    anyhow::anyhow!("--take wants B:P[=NB:NP]@PATH, got '{spec}'")
                })?;
                let (src, retag) = match ident.split_once('=') {
                    Some((s, r)) => (s, Some(r)),
                    None => (ident, None),
                };
                let parse_bp = |s: &str| -> Result<(u16, u16)> {
                    let (b, p) = s
                        .split_once(':')
                        .ok_or_else(|| anyhow::anyhow!("'{s}' is not BANK:PROG"))?;
                    Ok((b.trim().parse()?, p.trim().parse()?))
                };
                let (sb, sp) = parse_bp(src)?;
                let retag = retag.map(parse_bp).transpose()?;
                let bytes = std::fs::read(path)
                    .map_err(|e| anyhow::anyhow!("open take font {path}: {e}"))?;
                #[cfg(feature = "sf3")]
                let bytes = cosmix_musicd::sf3::maybe_decode_sf3(bytes)
                    .map_err(|e| anyhow::anyhow!("decode SF3 {path}: {e}"))?;
                let mut got =
                    cosmix_musicd::sf2split::split_presets(&bytes, Some(sb), Some(sp), retag)?;
                if got.is_empty() {
                    anyhow::bail!("{path} has no preset {sb}:{sp}");
                }
                let o = got.remove(0);
                println!(
                    "  take {:>3}:{:<3} '{}' from {path}",
                    o.entry.bank, o.entry.program, o.entry.name
                );
                outs.push(o);
            }
            let inam = out
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| "cosmix merge".into());
            let merged = cosmix_musicd::sf2merge::merge_images(&outs, &inam)?;
            std::fs::write(&out, &merged)?;
            println!(
                "merged {} preset(s) → {} ({:.1} MB)",
                outs.len(),
                out.display(),
                merged.len() as f64 / 1e6
            );
        }

        Cmd::Split {
            soundfont,
            out_dir,
            bank,
            program,
            retag,
            normalize,
            levels,
        } => {
            let retag = retag
                .map(|s| -> Result<(u16, u16)> {
                    let (b, p) = s
                        .split_once(':')
                        .ok_or_else(|| anyhow::anyhow!("--as wants BANK:PROG, got '{s}'"))?;
                    let (b, p): (u16, u16) = (b.trim().parse()?, p.trim().parse()?);
                    if b > 128 || p > 127 {
                        anyhow::bail!("--as {b}:{p} out of range (bank 0-128, program 0-127)");
                    }
                    Ok((b, p))
                })
                .transpose()?;
            let bytes = std::fs::read(&soundfont)
                .map_err(|e| anyhow::anyhow!("open soundfont {}: {e}", soundfont.display()))?;
            let src_len = bytes.len();
            // Mirror synth::load_soundfont: SF3 decodes transparently to SF2.
            #[cfg(feature = "sf3")]
            let bytes = cosmix_musicd::sf3::maybe_decode_sf3(bytes)
                .map_err(|e| anyhow::anyhow!("decode SF3 {}: {e}", soundfont.display()))?;
            let mut outs = cosmix_musicd::sf2split::split_presets(&bytes, bank, program, retag)?;
            if outs.is_empty() {
                anyhow::bail!("no presets match the bank/program filter");
            }
            std::fs::create_dir_all(&out_dir)?;
            if let Some(t) = normalize
                && !t.is_finite()
            {
                anyhow::bail!("--normalize target must be a finite dBFS value, got {t}");
            }
            let mut level_rows = Vec::new();
            let mut total = 0usize;
            for o in &mut outs {
                let path = out_dir.join(cosmix_musicd::sf2split::preset_file_name(
                    o.entry.bank,
                    o.entry.program,
                    &o.entry.name,
                ));
                let mut level_note = String::new();
                if normalize.is_some() || levels.is_some() {
                    let before = cosmix_musicd::sf2gain::measure_dbfs(
                        &o.sf2,
                        o.entry.bank,
                        o.entry.program,
                    )?;
                    let mut after = before;
                    match (before, normalize) {
                        (Some(db), Some(target)) if db > target => {
                            let gain = cosmix_musicd::sf2gain::db_to_gain(target - db);
                            o.sf2 = cosmix_musicd::sf2gain::scale_pcm(&o.sf2, gain)?;
                            after = cosmix_musicd::sf2gain::measure_dbfs(
                                &o.sf2,
                                o.entry.bank,
                                o.entry.program,
                            )?;
                            level_note = format!(
                                "  [{db:.1} → {} dBFS]",
                                after.map_or("?".into(), |a| format!("{a:.1}"))
                            );
                        }
                        (Some(db), Some(_)) => {
                            level_note = format!("  [{db:.1} dBFS, below target — kept]")
                        }
                        (Some(db), None) => level_note = format!("  [{db:.1} dBFS]"),
                        (None, _) => level_note = "  [no probe key sounded — kept]".into(),
                    }
                    level_rows.push(format!(
                        "{}\t{}\t{}\t{}\t{}\t{}",
                        o.entry.bank,
                        o.entry.program,
                        o.entry.name.replace(['\t', '\n', '\r'], " "),
                        before.map_or("-".into(), |v| format!("{v:.2}")),
                        after.map_or("-".into(), |v| format!("{v:.2}")),
                        path.display(),
                    ));
                }
                std::fs::write(&path, &o.sf2)?;
                total += o.sf2.len();
                println!(
                    "{:>3}:{:<3} {:<24} → {} ({:.1} MB){level_note}",
                    o.entry.bank,
                    o.entry.program,
                    o.entry.name,
                    path.display(),
                    o.sf2.len() as f64 / 1e6,
                );
                for w in &o.warnings {
                    eprintln!("  warning: {w}");
                }
            }
            if let Some(levels_path) = levels {
                let mut tsv = String::from("bank\tprogram\tname\tdbfs_before\tdbfs_after\tfile\n");
                tsv.push_str(&level_rows.join("\n"));
                tsv.push('\n');
                std::fs::write(&levels_path, tsv)?;
                println!("levels written to {}", levels_path.display());
            }
            println!(
                "split {} preset(s): {:.1} MB total (source {:.1} MB)",
                outs.len(),
                total as f64 / 1e6,
                src_len as f64 / 1e6,
            );
        }

        Cmd::MixerSim {
            frames,
            print_every,
            out,
        } => {
            cosmix_musicd::mixer::run_simulator(frames, print_every, out.as_deref())?;
        }

        Cmd::Midi2 { cmd } => run_midi2(cmd)?,

        #[cfg(feature = "playback")]
        Cmd::Play {
            midi,
            soundfont,
            gain,
        } => {
            cosmix_musicd::play::play_blocking(&midi, &soundfont, gain)?;
        }

        #[cfg(feature = "playback")]
        Cmd::Live => {
            cosmix_musicd::live::run()?;
        }

        #[cfg(feature = "cosmix")]
        Cmd::Serve { config } => {
            let rt = tokio::runtime::Runtime::new()?;
            rt.block_on(cosmix_musicd::daemon::serve(config))?;
        }

        #[cfg(feature = "cosmix")]
        Cmd::MixerServe { autoplay, stems } => {
            let rt = tokio::runtime::Runtime::new()?;
            rt.block_on(cosmix_musicd::mixer_daemon::serve(autoplay, stems))?;
        }
    }
    Ok(())
}

fn run_midi2(cmd: Midi2Cmd) -> Result<()> {
    use cosmix_musicd::midi2::{smfio, umpfile};

    match cmd {
        Midi2Cmd::Dump { file } => {
            use std::io::Write;

            let bytes = std::fs::read(&file)
                .map_err(|error| anyhow::anyhow!("open {}: {error}", file.display()))?;
            let timeline = if bytes.starts_with(umpfile::MAGIC) {
                let file = umpfile::read(&bytes)?;
                smfio::from_ump_file(&file)?
            } else if bytes.starts_with(b"MThd") {
                smfio::import_smf(&bytes, None)?
            } else {
                anyhow::bail!(
                    "{} is neither a .cosump file nor a Standard MIDI File",
                    file.display()
                );
            };
            let stdout = std::io::stdout();
            let mut stdout = stdout.lock();
            for line in smfio::dump_lines(&timeline) {
                if let Err(error) = writeln!(stdout, "{line}") {
                    if error.kind() == std::io::ErrorKind::BrokenPipe {
                        return Ok(());
                    }
                    return Err(error.into());
                }
            }
        }
        Midi2Cmd::Up {
            input,
            output,
            track,
        } => {
            let bytes = std::fs::read(&input)
                .map_err(|error| anyhow::anyhow!("open {}: {error}", input.display()))?;
            let timeline = smfio::import_smf(&bytes, track)?;
            let file = smfio::to_ump_file(&timeline)?;
            let encoded = umpfile::write(&file)?;
            std::fs::write(&output, encoded)
                .map_err(|error| anyhow::anyhow!("write {}: {error}", output.display()))?;
            println!(
                "up: {} event(s), {} tempo change(s), TPQ {} → {}",
                timeline.events.len(),
                timeline.tempos.len(),
                timeline.ticks_per_quarter,
                output.display()
            );
        }
        Midi2Cmd::Down { input, output } => {
            let bytes = std::fs::read(&input)
                .map_err(|error| anyhow::anyhow!("open {}: {error}", input.display()))?;
            let file = umpfile::read(&bytes)?;
            let timeline = smfio::from_ump_file(&file)?;
            let exported = smfio::export_smf(&timeline)?;
            std::fs::write(&output, exported.bytes)
                .map_err(|error| anyhow::anyhow!("write {}: {error}", output.display()))?;
            for line in smfio::dropped_lines(exported.dropped) {
                eprintln!("{line}");
            }
            println!(
                "down: {} event(s) → {}",
                timeline.events.len(),
                output.display()
            );
        }
        Midi2Cmd::Roundtrip { input } => {
            let bytes = std::fs::read(&input)
                .map_err(|error| anyhow::anyhow!("open {}: {error}", input.display()))?;
            let report = smfio::roundtrip_smf(&bytes, None)?;
            if let Some(divergence) = report.first_divergence() {
                anyhow::bail!("MIDI 2 roundtrip mismatch: {divergence}");
            }
            for line in smfio::dropped_lines(report.dropped) {
                eprintln!("{line}");
            }
            println!(
                "roundtrip ok: {} event(s), {} tempo change(s), TPQ {}",
                report.expected.events.len(),
                report.expected.tempos.len(),
                report.expected.ticks_per_quarter
            );
        }
    }
    Ok(())
}
