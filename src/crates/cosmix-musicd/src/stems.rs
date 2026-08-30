//! Per-track stem rendering: one audio file per SMF track, in one process.
//!
//! Replaces the shell loop (midicomp split → N `render` invocations, each
//! loading its own font copy). Track surgery is [`crate::smf`]: every stem is
//! an in-memory format-1 file of [conductor, track] where the conductor is
//! track 0 reduced to its tempo map — so stems can't leak track 0's notes,
//! and a track 0 that has notes renders as its own stem.
//!
//! With `--library DIR` each track's font resolves from a `split` library by
//! the track's detected bank/program (`{bank:03}-{prog:03}-*.sf2`), with the
//! same fallback the Ardour script uses: variation bank → bank 0, drums →
//! the standard kit, else the `--soundfont` fallback bank.

use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, anyhow, bail};
use rustysynth::MidiFile;

use crate::render::{self, RenderFormat, RenderOptions, RenderReport};
use crate::{smf, synth};

/// One rendered stem, for the CLI to report.
pub struct StemReport {
    pub file: PathBuf,
    pub font: PathBuf,
    pub bank: u16,
    pub program: u16,
    pub report: RenderReport,
}

/// `t{NN}-{slug}` from a track's name, matching the Ardour script's slugs.
fn stem_name(idx: usize, name: Option<&str>) -> String {
    let mut slug: String = name
        .unwrap_or("")
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect();
    if slug.is_empty() {
        slug = "part".into();
    }
    format!("t{idx:02}-{slug}")
}

/// Resolve `{bank:03}-{prog:03}-*.sf2` in `dir`, with GM-style fallback.
fn library_font(dir: &Path, bank: u16, program: u16) -> Result<Option<PathBuf>> {
    let find = |bank: u16, program: u16| -> Result<Option<PathBuf>> {
        let prefix = format!("{bank:03}-{program:03}-");
        let mut hits: Vec<PathBuf> = std::fs::read_dir(dir)
            .with_context(|| format!("reading library {}", dir.display()))?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| {
                p.extension().is_some_and(|x| x.eq_ignore_ascii_case("sf2"))
                    && p.file_name()
                        .and_then(|n| n.to_str())
                        .is_some_and(|n| n.starts_with(&prefix))
            })
            .collect();
        hits.sort();
        Ok(hits.into_iter().next())
    };
    if let Some(hit) = find(bank, program)? {
        return Ok(Some(hit));
    }
    // Fallbacks mirror fluidsynth preset selection: variation bank → bank 0;
    // an unknown drum kit → the standard kit.
    if bank == 128 {
        find(128, 0)
    } else {
        find(0, program)
    }
}

/// How each of a song's identities was satisfied when auto-assembling an
/// orchestra bank.
pub struct OrchestraReport {
    pub from_library: Vec<(u16, u16)>,
    pub from_fallback: Vec<(u16, u16)>,
    /// Identities neither source could satisfy (rendered by the synth's own
    /// preset fallback against the merged bank).
    pub omitted: Vec<(u16, u16)>,
}

/// Auto-assemble a whole-song orchestra bank: scan the SMF's effective
/// identities (time-ordered across tracks), pull each from the split
/// library — re-extracted with a retag whenever the chosen file's internal
/// identity differs from the requested one (GM fallback picks) — fill gaps
/// from the fallback bank, and merge in memory. Returns `None` when the song
/// uses an exotic identity (percussion bank > 128, unrepresentable in SF2):
/// the caller must render with the full fallback bank for exact behavior.
pub fn assemble_orchestra(
    midi_bytes: &[u8],
    library: &Path,
    fallback_font: &Path,
    inam: &str,
) -> Result<Option<(Vec<u8>, OrchestraReport)>> {
    use crate::sf2split::{PresetEntry, SplitOutput, list_presets, split_presets};

    let parsed = smf::parse(midi_bytes)?;
    let identities = smf::scan_song(&parsed)?;
    if identities.is_empty() || identities.iter().any(|&(b, _)| b > 128) {
        return Ok(None);
    }

    let mut report = OrchestraReport {
        from_library: Vec::new(),
        from_fallback: Vec::new(),
        omitted: Vec::new(),
    };
    let mut fallback_bytes: Option<Vec<u8>> = None;
    let mut takes: Vec<SplitOutput> = Vec::new();
    for &(bank, program) in &identities {
        if let Some(path) = library_font(library, bank, program)? {
            let bytes = std::fs::read(&path)
                .with_context(|| format!("reading library extract {}", path.display()))?;
            // A compressed SF3 hiding under a .sf2 name would corrupt the
            // merge (its sample offsets are bytes, not words) — decode first,
            // like every other font-loading path.
            #[cfg(feature = "sf3")]
            let bytes = crate::sf3::maybe_decode_sf3(bytes)
                .with_context(|| format!("decoding {}", path.display()))?;
            // The file's INTERNAL identity is authoritative (filenames lie);
            // a GM-fallback pick answers a different identity than requested
            // and must be re-extracted with a retag so the merged bank
            // responds to the exact bank/program the notes select.
            let presets =
                list_presets(&bytes).with_context(|| format!("inspecting {}", path.display()))?;
            let take = match presets.as_slice() {
                [p] if (p.bank, p.program) == (bank, program) => SplitOutput {
                    entry: PresetEntry {
                        bank,
                        program,
                        name: p.name.clone(),
                    },
                    sf2: bytes,
                    warnings: Vec::new(),
                },
                [p] => split_presets(&bytes, Some(p.bank), Some(p.program), Some((bank, program)))
                    .with_context(|| format!("retagging {}", path.display()))?
                    .remove(0),
                _ => bail!(
                    "{} is not a single-preset extract ({} presets)",
                    path.display(),
                    presets.len()
                ),
            };
            report.from_library.push((bank, program));
            takes.push(take);
            continue;
        }
        // Not in the library: pull the exact identity from the fallback bank.
        let fb = match &fallback_bytes {
            Some(b) => b,
            None => {
                let b = std::fs::read(fallback_font).with_context(|| {
                    format!("reading fallback bank {}", fallback_font.display())
                })?;
                #[cfg(feature = "sf3")]
                let b = crate::sf3::maybe_decode_sf3(b)?;
                fallback_bytes = Some(b);
                fallback_bytes.as_ref().unwrap()
            }
        };
        let mut got = split_presets(fb, Some(bank), Some(program), None)?;
        if got.is_empty() {
            eprintln!("  warning: no source for {bank}:{program} — omitted from orchestra");
            report.omitted.push((bank, program));
        } else {
            report.from_fallback.push((bank, program));
            takes.push(got.remove(0));
        }
    }
    if takes.is_empty() {
        return Ok(None);
    }
    let bank = crate::sf2merge::merge_images(&takes, inam)?;
    Ok(Some((bank, report)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sf2split::tests::build_test_font;
    use crate::sf2split::{list_presets, split_presets};

    /// A format-1 song whose ch2 notes play under bank 8 / program 48.
    fn song_8_48() -> Vec<u8> {
        let mut t = Vec::new();
        t.extend_from_slice(&[0x00, 0xB2, 0x00, 8]);
        t.extend_from_slice(&[0x00, 0xC2, 48]);
        t.extend_from_slice(&[0x00, 0x92, 60, 100]);
        t.extend_from_slice(&[0x00, 0xFF, 0x2F, 0x00]);
        smf::assemble([0x01, 0xE0], &[&t])
    }

    #[test]
    fn orchestra_retags_gm_fallback_picks_and_fills_from_fallback_bank() {
        let dir = std::env::temp_dir().join(format!("musicd-orch-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let font = build_test_font();
        // Library holds ONLY the test font's Strings preset retagged to
        // 0:48 (a GM-slot file), plus the full test font as fallback bank.
        let strings = split_presets(&font, Some(8), Some(48), Some((0, 48)))
            .unwrap()
            .remove(0);
        std::fs::write(dir.join("000-048-strings.sf2"), &strings.sf2).unwrap();
        let fallback = dir.join("fallback.sf2");
        std::fs::write(&fallback, &font).unwrap();

        // Song asks 8:48 — the library satisfies it only via the GM
        // fallback pick 000-048, whose internal identity is 0:48, so the
        // assembler must re-extract it retagged to 8:48.
        let (bank, rep) = assemble_orchestra(&song_8_48(), &dir, &fallback, "t")
            .unwrap()
            .expect("orchestra expected");
        assert_eq!(rep.from_library, vec![(8, 48)]);
        let ids: Vec<(u16, u16)> = list_presets(&bank)
            .unwrap()
            .iter()
            .map(|p| (p.bank, p.program))
            .collect();
        assert_eq!(ids, vec![(8, 48)]);

        // A song needing 0:0 too: not in the library → filled from the
        // fallback bank (the test font's Piano).
        let mut t2 = Vec::new();
        t2.extend_from_slice(&[0x00, 0x90, 60, 100]); // ch0, default 0:0
        t2.extend_from_slice(&[0x00, 0xB2, 0x00, 8]);
        t2.extend_from_slice(&[0x00, 0xC2, 48]);
        t2.extend_from_slice(&[0x10, 0x92, 60, 100]);
        t2.extend_from_slice(&[0x00, 0xFF, 0x2F, 0x00]);
        let song2 = smf::assemble([0x01, 0xE0], &[&t2]);
        let (bank2, rep2) = assemble_orchestra(&song2, &dir, &fallback, "t")
            .unwrap()
            .expect("orchestra");
        assert_eq!(rep2.from_fallback, vec![(0, 0)]);
        assert_eq!(list_presets(&bank2).unwrap().len(), 2);
        rustysynth::SoundFont::new(&mut std::io::Cursor::new(bank2)).unwrap();

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn orchestra_declines_exotic_percussion_banks() {
        let dir = std::env::temp_dir().join(format!("musicd-orch-exotic-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let fallback = dir.join("fallback.sf2");
        std::fs::write(&fallback, build_test_font()).unwrap();
        // ch10 with CC0=8 → effective bank 136 → whole-render fallback.
        let mut t = Vec::new();
        t.extend_from_slice(&[0x00, 0xB9, 0x00, 8]);
        t.extend_from_slice(&[0x00, 0x99, 36, 100]);
        t.extend_from_slice(&[0x00, 0xFF, 0x2F, 0x00]);
        let song = smf::assemble([0x01, 0xE0], &[&t]);
        assert!(
            assemble_orchestra(&song, &dir, &fallback, "t")
                .unwrap()
                .is_none()
        );
        std::fs::remove_dir_all(&dir).ok();
    }
}

/// Render every note-bearing track of `midi_path` into `out_dir`.
pub fn render_per_track(
    midi_path: &Path,
    fallback_font: &Path,
    library: Option<&Path>,
    out_dir: &Path,
    opts: &RenderOptions,
) -> Result<Vec<StemReport>> {
    let bytes =
        std::fs::read(midi_path).map_err(|e| anyhow!("open midi {}: {e}", midi_path.display()))?;
    let parsed = smf::parse(&bytes)?;
    if parsed.format == 2 {
        // Format-2 tracks are independent sequences — track 0 is NOT a
        // conductor for the others, so per-track stems don't apply.
        bail!("SMF format 2 is not supported by --per-track");
    }
    if parsed.division[0] & 0x80 != 0 {
        bail!("SMPTE-timed SMF is not supported by --per-track");
    }
    let ext = match opts.format {
        RenderFormat::Flac24 => "flac",
        _ => "wav",
    };

    // Which tracks become stems, each paired with its conductor.
    let conductor = smf::conductor_only(&parsed.tracks[0])?;
    let mut jobs: Vec<(usize, smf::TrackInfo, Vec<u8>)> = Vec::new();
    for (i, track) in parsed.tracks.iter().enumerate() {
        let info = smf::scan(track).with_context(|| format!("scanning track {i}"))?;
        if !info.sounding() {
            continue; // meta-only or setup-only track — not a stem
        }
        let stem_smf = if i == 0 {
            // Track 0 with notes: its own stem, tempo map already inside.
            smf::assemble(parsed.division, &[track])
        } else {
            smf::assemble(parsed.division, &[&conductor, track])
        };
        jobs.push((i, info, stem_smf));
    }
    if jobs.is_empty() {
        bail!("no tracks with channel events to render");
    }

    std::fs::create_dir_all(out_dir)?;
    let mut out = Vec::new();
    for (n, (i, info, stem_smf)) in jobs.iter().enumerate() {
        // The identity the track's sounding notes actually play under —
        // jobs are filtered to sounding tracks, so the list is never empty.
        let (bank, program) = info.identities.first().copied().unwrap_or((0, 0));
        let font = match library {
            // A track that changes program mid-way, mixes drum + melodic
            // channels, or selects a percussion bank variation (CC0 on the
            // drum channel → bank > 128) can't be covered by one library
            // extract — use the full fallback bank so it renders exactly
            // like the source.
            Some(_) if info.needs_full_bank() || bank > 128 => {
                eprintln!("  note: track {i} uses multiple or exotic presets — full bank");
                fallback_font.to_path_buf()
            }
            Some(dir) => library_font(dir, bank, program)?.unwrap_or_else(|| {
                eprintln!(
                    "  warning: no library font for {bank}:{program} (track {i}) — fallback bank"
                );
                fallback_font.to_path_buf()
            }),
            None => fallback_font.to_path_buf(),
        };
        let sf = synth::load_soundfont(&font)?;
        let midi = Arc::new(
            MidiFile::new(&mut Cursor::new(stem_smf.clone()))
                .map_err(|e| anyhow!("stem SMF for track {i}: {e}"))?,
        );
        let (left, right, report) = render::render_to_buffers(&sf, &midi, opts)?;
        let file = out_dir.join(format!("{}.{ext}", stem_name(n + 1, info.name.as_deref())));
        render::write_render(
            &file,
            &left,
            &right,
            opts.sample_rate,
            opts.channels,
            opts.format,
        )?;
        out.push(StemReport {
            file,
            font,
            bank,
            program,
            report,
        });
    }
    Ok(out)
}
