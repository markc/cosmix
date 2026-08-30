//! Pure-Rust SFZ → SF2 converter (prototype).
//!
//! Replaces the Polyphone (GPL-3, Qt) shell-out for the SFZ subset the target
//! libraries actually use (Salamander Grand Piano, VSCO-2 CE): the
//! `<global>`/`<group>`/`<region>` cascade with `sample`, `lokey/hikey/key`,
//! `pitch_keycenter`, `lovel/hivel`, `tune/transpose`, `volume`, `pan`,
//! `loop_mode`/`loop_start`/`loop_end`, `offset`, and the `ampeg_*` amplitude
//! envelope. Samples are decoded with hound (WAV) + claxon (FLAC, Apache-2.0);
//! stereo sources are split into linked L/R mono SF2 samples with two hard-
//! panned zones per region (sources with >2 channels are downmixed to mono),
//! and assembled into an uncompressed SF2 image that rustysynth loads unmodified.
//!
//! No fork, no GPL, no C FFI — the same in-process "format conversion" shape as
//! the SF3 decoder ([`crate::sf3`]). Opcodes it doesn't model are recorded and
//! reported (honest degradation), not silently dropped; regions SF2 can't
//! express are skipped — conditionally-triggered groups (`trigger=release`,
//! `on_locc64`, resonance), `*silence`/generator samples, round-robin
//! variants (`seq_position>1`), and non-default keyswitch layers (`sw_last`
//! != `sw_default` — kept together they stack per note), since SF2
//! can't express them (this is what caused the Salamander "thump" via Polyphone).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};

// ── SFZ parse ───────────────────────────────────────────────────────────────

/// One fully-resolved playable region (global ⊕ group ⊕ region opcodes).
#[derive(Clone, Debug)]
struct Region {
    sample: PathBuf,
    lokey: u8,
    hikey: u8,
    lovel: u8,
    hivel: u8,
    pitch_keycenter: u8,
    tune_cents: i32,
    transpose: i32,
    volume_db: f32,
    pan: f32, // -100..100
    loop_mode: LoopMode,
    loop_start: Option<u32>,
    loop_end: Option<u32>,
    offset: u32,
    amp_delay: f32,
    amp_attack: f32,
    amp_hold: f32,
    amp_decay: f32,
    amp_sustain: f32, // percent 0..100
    amp_release: f32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum LoopMode {
    NoLoop,
    Continuous,
    UntilRelease,
}

/// Expand ARIA preprocessor directives into one flat SFZ text:
/// `#include "file"` (path relative to the *including* file, backslash-tolerant)
/// is inlined recursively, and `#define $VAR value` macros are substituted into
/// following lines. `//` comments are stripped first so a commented-out
/// directive is inert. Include recursion is depth-capped against cycles.
/// Input is assumed to be a trusted, locally-installed SFZ library: `#include`
/// and `sample=` paths (including `..`) are followed as written, not sandboxed.
fn preprocess(path: &Path) -> Result<String> {
    let root_dir = path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .to_path_buf();
    let mut out = String::new();
    let mut defines: Vec<(String, String)> = Vec::new();
    expand_includes(path, &root_dir, 0, &mut out, &mut defines)?;
    Ok(out)
}

/// Substitute every `#define`d name in `s`, token-aware: a name matches only
/// when not followed by another identifier char, so `$hi` can't corrupt
/// `$high`. Longest names are tried first at each position.
fn apply_defines(s: &str, defines: &[(String, String)]) -> String {
    if defines.is_empty() || !s.contains('$') {
        return s.to_string();
    }
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    'scan: while i < s.len() {
        if bytes[i] == b'$' {
            for (name, val) in defines {
                if s[i..].starts_with(name.as_str()) {
                    let end = i + name.len();
                    // A macro name ends at any non-alphanumeric char: ARIA uses
                    // `_` as a separator (`$MIC_$ART`), so `_` terminates a name.
                    // A defined name that itself contains `_` still matches
                    // exactly, since defines are tried longest-first.
                    let boundary = end >= s.len() || !bytes[end].is_ascii_alphanumeric();
                    if boundary {
                        out.push_str(val);
                        i = end;
                        continue 'scan;
                    }
                }
            }
        }
        let ch = s[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

fn expand_includes(
    path: &Path,
    root_dir: &Path,
    depth: usize,
    out: &mut String,
    defines: &mut Vec<(String, String)>,
) -> Result<()> {
    if depth > 25 {
        bail!("SFZ #include nested too deep (>25): {}", path.display());
    }
    let text =
        std::fs::read_to_string(path).with_context(|| format!("read sfz {}", path.display()))?;
    for raw in text.lines() {
        let line = match raw.find("//") {
            Some(i) => &raw[..i],
            None => raw,
        };
        let trimmed = line.trim_start();
        if let Some(rest) = trimmed.strip_prefix("#define") {
            let rest = rest.trim();
            let (name, val) = match rest.split_once(char::is_whitespace) {
                Some((n, v)) => (n.trim().to_string(), v.trim().to_string()),
                None => (rest.to_string(), String::new()),
            };
            if !name.is_empty() {
                // A later redefinition wins; otherwise insert keeping the list
                // sorted longest-name-first for `apply_defines`.
                if let Some(e) = defines.iter_mut().find(|(n, _)| *n == name) {
                    e.1 = val;
                } else {
                    defines.push((name, val));
                    defines.sort_by_key(|d| std::cmp::Reverse(d.0.len()));
                }
            }
            continue;
        }
        // `#include` may appear anywhere in a line, any number of times —
        // Salamander V3 authors whole groups as
        // `<group> #include "Data/vel_01.txt" lovel=1 hivel=26 #include "Data/region.txt"`
        // — so scan the line and inline each include at its position in the
        // token stream (a line-leading directive is just the first match).
        // Substitute defines in the path too — libraries build include paths
        // from `#define $DIR ...`. ARIA resolves #include relative to the
        // ROOT sfz's directory at every nesting level (not the including
        // file), e.g. a file in `Data/stereo/` includes `../Data/group/x` —
        // which only resolves from the root, not the includer.
        let mut rest = line;
        while let Some(pos) = rest.find("#include") {
            let before = &rest[..pos];
            if !before.trim().is_empty() {
                out.push_str(&apply_defines(before, defines));
                out.push('\n');
            }
            let after = rest["#include".len() + pos..].trim_start();
            // Quoted path (`#include "a b.txt"`); tolerate an unquoted
            // path running to the next whitespace.
            let (raw_path, remainder) = if let Some(q) = after.strip_prefix('"') {
                match q.find('"') {
                    Some(end) => (&q[..end], &q[end + 1..]),
                    None => (q, ""),
                }
            } else {
                match after.find(char::is_whitespace) {
                    Some(end) => (&after[..end], &after[end..]),
                    None => (after, ""),
                }
            };
            let inc = apply_defines(raw_path, defines);
            let inc = inc.trim();
            if !inc.is_empty() {
                expand_includes(
                    &resolve_path(root_dir, inc),
                    root_dir,
                    depth + 1,
                    out,
                    defines,
                )?;
            }
            rest = remainder;
        }
        if !rest.trim().is_empty() || rest == line {
            out.push_str(&apply_defines(rest, defines));
            out.push('\n');
        }
        if out.len() > 64 * 1024 * 1024 {
            bail!("SFZ preprocess output exceeds 64 MiB — #include fan-out too large");
        }
    }
    Ok(())
}

/// Parse an SFZ file into resolved regions. `ignored` collects opcode names we
/// don't model, and `skipped` counts regions SF2 cannot express (release/pedal
/// groups, `*` generators, round-robin variants).
fn parse_sfz(path: &Path, ignored: &mut Vec<String>, skipped: &mut usize) -> Result<Vec<Region>> {
    // Expand ARIA `#include`/`#define` first, so the tokenizer sees one flat text.
    let text = preprocess(path)?;
    let base = path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .to_path_buf();

    // Strip `//` line comments, then whitespace-tokenize the whole file.
    let mut stripped = String::with_capacity(text.len());
    for line in text.lines() {
        let line = match line.find("//") {
            Some(i) => &line[..i],
            None => line,
        };
        stripped.push_str(line);
        stripped.push('\n');
    }
    let toks: Vec<&str> = stripped.split_whitespace().collect();

    let mut global: HashMap<String, String> = HashMap::new();
    let mut master: HashMap<String, String> = HashMap::new();
    let mut group: HashMap<String, String> = HashMap::new();
    let mut region: HashMap<String, String> = HashMap::new();
    let mut control: HashMap<String, String> = HashMap::new(); // unmodelled set_ccN etc.
    // Captured eagerly (not from `control`, which unknown headers clear).
    let mut default_path = String::new();
    #[derive(PartialEq)]
    enum Scope {
        Global,
        Master,
        Group,
        Region,
        Control,
    }
    let mut scope = Scope::Global;
    let mut resolved: Vec<HashMap<String, String>> = Vec::new();

    // Resolve one region: global ⊕ master ⊕ group ⊕ region (later wins).
    let finalize = |region: &HashMap<String, String>,
                    group: &HashMap<String, String>,
                    master: &HashMap<String, String>,
                    global: &HashMap<String, String>|
     -> HashMap<String, String> {
        let mut m = global.clone();
        for src in [master, group, region] {
            for (k, v) in src {
                m.insert(k.clone(), v.clone());
            }
        }
        m
    };

    let mut i = 0;
    while i < toks.len() {
        let t = toks[i];
        if t.starts_with('<') && t.ends_with('>') {
            // A new header ends any pending region.
            if scope == Scope::Region {
                resolved.push(finalize(&region, &group, &master, &global));
            }
            match t {
                // A new scope resets everything strictly below it in the
                // global > master > group > region hierarchy.
                "<global>" => {
                    scope = Scope::Global;
                    global.clear();
                    master.clear();
                    group.clear();
                }
                "<master>" => {
                    scope = Scope::Master;
                    master.clear();
                    group.clear();
                }
                "<group>" => {
                    scope = Scope::Group;
                    group.clear();
                }
                "<region>" => {
                    scope = Scope::Region;
                    region.clear();
                }
                // <control> (default_path/set_ccN) and any other/unknown header
                // (<curve>, <effect>, …): route to the dead control scope. The
                // pending region was already finalized above; without switching
                // scope, their opcodes would leak into it and it would be
                // pushed a second time at EOF (a duplicated layer).
                _ => {
                    scope = Scope::Control;
                    control.clear();
                }
            }
            i += 1;
            continue;
        }
        // opcode: key=value, value continues over following non-`=`/non-`<` tokens
        if let Some(eq) = t.find('=') {
            let key = t[..eq].to_string();
            let mut val = t[eq + 1..].to_string();
            let mut j = i + 1;
            while j < toks.len() && !toks[j].contains('=') && !toks[j].starts_with('<') {
                val.push(' ');
                val.push_str(toks[j]);
                j += 1;
            }
            if key == "default_path" {
                default_path = val.replace('\\', "/"); // capture before any clear
            }
            let bucket = match scope {
                Scope::Global => &mut global,
                Scope::Master => &mut master,
                Scope::Group => &mut group,
                Scope::Region => &mut region,
                Scope::Control => &mut control,
            };
            bucket.insert(key, val);
            i = j;
            continue;
        }
        i += 1;
    }
    if scope == Scope::Region {
        resolved.push(finalize(&region, &group, &master, &global));
    }

    // `default_path` (captured above) prefixes every `sample=`, relative to root.

    // Resolve each opcode map into a Region, skipping SF2-inexpressible ones.
    let mut out = Vec::new();
    // First-seen `sw_last` value — the fallback "default layer" when the file
    // declares keyswitch layers but no `sw_default`.
    let mut default_keyswitch: Option<String> = None;
    for m in &resolved {
        // Skip conditionally-triggered regions — SF2 can't gate on them, and
        // baking them into note-ons is the Salamander "thump" bug.
        let trig = m.get("trigger").map(|s| s.as_str()).unwrap_or("attack");
        if trig != "attack" && trig != "first" {
            *skipped += 1;
            continue;
        }
        if m.contains_key("on_locc64") || m.contains_key("on_hicc64") || m.contains_key("on_locc66")
        {
            *skipped += 1;
            continue;
        }
        // Keyswitch layers: SF2 has no keyswitches, so keeping every `sw_last`
        // layer would STACK them per note (Salamander's Natural + Retuned
        // masters each include the full note set — kept together every note
        // sounds twice, detuned). Keep exactly one layer: the one matching
        // `sw_default` when declared, else the first `sw_last` value seen.
        if let Some(last) = m.get("sw_last") {
            let norm = |s: &str| {
                parse_note(s)
                    .map(|n| n.to_string())
                    .unwrap_or_else(|| s.to_ascii_lowercase())
            };
            let keep = match m.get("sw_default") {
                Some(def) => norm(def) == norm(last),
                None => {
                    let l = norm(last);
                    let first = default_keyswitch.get_or_insert(l.clone());
                    *first == l
                }
            };
            if !keep {
                *skipped += 1;
                continue;
            }
        }
        let Some(sample) = m.get("sample") else {
            continue;
        };
        let sample = sample.trim();
        // SFZ built-in generators (`*silence`, `*sine`, `*noise`, …) have no SF2
        // equivalent — drum kits use `*silence` for muted/placeholder keys. Skip.
        if sample.starts_with('*') {
            *skipped += 1;
            continue;
        }
        // SFZ round-robins alternate samples on successive hits at the SAME
        // key/velocity; SF2 has no round-robin, so keeping every variant stacks
        // (sums) them per note — wrong and loud. Keep only the first. This
        // assumes `seq_position>1` is a true RR duplicate (as well-formed
        // libraries author it); a library that (mis)uses `seq_position` across
        // distinct keys would lose those regions.
        if m.get("seq_position")
            .and_then(|v| v.trim().parse::<i32>().ok())
            .is_some_and(|p| p > 1)
        {
            *skipped += 1;
            continue;
        }
        // note ignored opcodes (once each) for the probe report
        for k in m.keys() {
            if !MODELLED.contains(&k.as_str()) && !ignored.contains(k) {
                ignored.push(k.clone());
            }
        }

        let getf = |k: &str| m.get(k).and_then(|v| v.trim().parse::<f32>().ok());
        let geti = |k: &str| m.get(k).and_then(|v| v.trim().parse::<i32>().ok());

        // key range: `key` is shorthand for lokey=hikey=pitch_keycenter
        let key_shorthand = m.get("key").and_then(|v| parse_note(v));
        let lokey = m
            .get("lokey")
            .and_then(|v| parse_note(v))
            .or(key_shorthand)
            .unwrap_or(0);
        let hikey = m
            .get("hikey")
            .and_then(|v| parse_note(v))
            .or(key_shorthand)
            .unwrap_or(127);
        let pitch_keycenter = m
            .get("pitch_keycenter")
            .and_then(|v| parse_note(v))
            .or(key_shorthand)
            .unwrap_or(60);

        let loop_mode = match m.get("loop_mode").map(|s| s.as_str()) {
            Some("one_shot") | Some("no_loop") => LoopMode::NoLoop,
            Some("loop_continuous") => LoopMode::Continuous,
            Some("loop_sustain") => LoopMode::UntilRelease,
            _ => LoopMode::NoLoop,
        };

        out.push(Region {
            sample: resolve_sample(&base, &default_path, sample),
            lokey,
            hikey,
            lovel: geti("lovel").unwrap_or(0).clamp(0, 127) as u8,
            hivel: geti("hivel").unwrap_or(127).clamp(0, 127) as u8,
            pitch_keycenter,
            tune_cents: geti("tune").unwrap_or_else(|| geti("pitch").unwrap_or(0)),
            transpose: geti("transpose").unwrap_or(0),
            volume_db: getf("volume").unwrap_or(0.0),
            pan: getf("pan").unwrap_or(0.0),
            loop_mode,
            loop_start: m
                .get("loop_start")
                .or(m.get("loopstart"))
                .and_then(|v| v.trim().parse().ok()),
            loop_end: m
                .get("loop_end")
                .or(m.get("loopend"))
                .and_then(|v| v.trim().parse().ok()),
            offset: geti("offset").unwrap_or(0).max(0) as u32,
            amp_delay: getf("ampeg_delay").unwrap_or(0.0),
            amp_attack: getf("ampeg_attack").unwrap_or(0.0),
            amp_hold: getf("ampeg_hold").unwrap_or(0.0),
            amp_decay: getf("ampeg_decay").unwrap_or(0.0),
            amp_sustain: getf("ampeg_sustain").unwrap_or(100.0),
            amp_release: getf("ampeg_release").unwrap_or(0.0),
        });
    }
    if out.is_empty() {
        bail!("no playable <region>s found in {}", path.display());
    }
    Ok(out)
}

/// Opcodes the converter maps into SF2 (anything else is reported as ignored).
const MODELLED: &[&str] = &[
    "sample",
    "lokey",
    "hikey",
    "key",
    "pitch_keycenter",
    "lovel",
    "hivel",
    "tune",
    "pitch",
    "transpose",
    "volume",
    "pan",
    "loop_mode",
    "loop_start",
    "loop_end",
    "loopstart",
    "loopend",
    "offset",
    "ampeg_delay",
    "ampeg_attack",
    "ampeg_hold",
    "ampeg_decay",
    "ampeg_sustain",
    "ampeg_release",
    "trigger",
    "seq_length",
    "seq_position",
];

/// Parse an SFZ note: either a MIDI number (0-127) or a note name like `c4`,
/// `f#3`, `Bb2` (SFZ default octave: c4 = 60).
fn parse_note(s: &str) -> Option<u8> {
    let s = s.trim();
    if let Ok(n) = s.parse::<i32>() {
        return u8::try_from(n.clamp(0, 127)).ok();
    }
    let b = s.as_bytes();
    if b.is_empty() {
        return None;
    }
    let step = match b[0].to_ascii_lowercase() {
        b'c' => 0,
        b'd' => 2,
        b'e' => 4,
        b'f' => 5,
        b'g' => 7,
        b'a' => 9,
        b'b' => 11,
        _ => return None,
    };
    let mut idx = 1;
    let mut semis: i32 = step;
    while idx < b.len() && (b[idx] == b'#' || b[idx] == b'b') {
        semis += if b[idx] == b'#' { 1 } else { -1 };
        idx += 1;
    }
    let oct: i32 = s[idx..].trim().parse().ok()?;
    let midi = (oct + 1) * 12 + semis; // c4=60 → octave+1
    u8::try_from(midi.clamp(0, 127)).ok()
}

/// Resolve a `sample=` path (forward/backslash tolerant) against the SFZ dir,
/// with a case-insensitive fallback on case-sensitive filesystems.
fn resolve_path(base: &Path, raw: &str) -> PathBuf {
    let rel = raw.replace('\\', "/");
    let direct = base.join(&rel);
    if direct.exists() {
        return direct;
    }
    // case-insensitive component walk
    let mut cur = base.to_path_buf();
    for comp in rel.split('/') {
        if comp.is_empty() || comp == "." {
            continue;
        }
        let want = comp.to_ascii_lowercase();
        let mut hit = cur.join(comp);
        if !hit.exists()
            && let Ok(rd) = std::fs::read_dir(&cur)
        {
            for e in rd.flatten() {
                if e.file_name().to_string_lossy().to_ascii_lowercase() == want {
                    hit = e.path();
                    break;
                }
            }
        }
        cur = hit;
    }
    cur
}

/// Resolve a `sample=` value, prefixed by `<control> default_path` when set.
/// Both are backslash-tolerant and resolve against the root SFZ dir.
fn resolve_sample(base: &Path, default_path: &str, sample: &str) -> PathBuf {
    if default_path.is_empty() {
        resolve_path(base, sample)
    } else {
        let dp = default_path.trim_end_matches(['/', '\\']);
        resolve_path(base, &format!("{dp}/{sample}"))
    }
}

// ── sample decode (→ per-channel i16) ────────────────────────────────────────

struct LoadedSample {
    name: String,
    /// Decoded PCM per channel: 1 (mono) or 2 (stereo L, R). Sources with more
    /// than two channels are downmixed to a single mono channel.
    channels: Vec<Vec<i16>>,
    sample_rate: u32,
}

/// Split interleaved PCM into per-channel buffers. Mono passes through; stereo
/// deinterleaves to `[L, R]`; >2 channels downmix to a single mono channel.
fn deinterleave(inter: Vec<i16>, ch: usize) -> Vec<Vec<i16>> {
    if ch <= 1 {
        return vec![inter];
    }
    if ch == 2 {
        let frames = inter.len() / 2;
        let mut l = Vec::with_capacity(frames);
        let mut r = Vec::with_capacity(frames);
        for f in inter.chunks_exact(2) {
            l.push(f[0]);
            r.push(f[1]);
        }
        return vec![l, r];
    }
    let mut mono = Vec::with_capacity(inter.len() / ch);
    for f in inter.chunks_exact(ch) {
        let sum: i64 = f.iter().map(|&x| x as i64).sum();
        mono.push((sum / ch as i64) as i16);
    }
    vec![mono]
}

fn scale_to_i16(sample: i32, bits: u32) -> i16 {
    if bits >= 16 {
        (sample >> (bits - 16)) as i16
    } else {
        // Low-bit-depth (8/12-bit): left-shift up to full 16-bit range.
        (sample << (16 - bits)) as i16
    }
}

fn load_sample(path: &Path) -> Result<LoadedSample> {
    let name = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "sample".into());
    let ext = path
        .extension()
        .map(|e| e.to_ascii_lowercase())
        .unwrap_or_default();
    // Decode to interleaved i16 (per-sample scaled), then deinterleave.
    let (inter, ch, sample_rate): (Vec<i16>, usize, u32) = if ext == "flac" {
        let mut r = claxon::FlacReader::open(path)
            .with_context(|| format!("open flac {}", path.display()))?;
        let info = r.streaminfo();
        let ch = (info.channels as usize).max(1);
        let bits = info.bits_per_sample;
        let mut inter = Vec::new();
        for s in r.samples() {
            inter.push(scale_to_i16(
                s.with_context(|| format!("decode flac {}", path.display()))?,
                bits,
            ));
        }
        (inter, ch, info.sample_rate)
    } else {
        let mut r =
            hound::WavReader::open(path).with_context(|| format!("open wav {}", path.display()))?;
        let spec = r.spec();
        let ch = (spec.channels as usize).max(1);
        let bits = spec.bits_per_sample as u32;
        let inter: Vec<i16> = match spec.sample_format {
            hound::SampleFormat::Int => r
                .samples::<i32>()
                .map(|s| s.map(|v| scale_to_i16(v, bits)))
                .collect::<std::result::Result<_, _>>()
                .with_context(|| format!("decode wav {}", path.display()))?,
            hound::SampleFormat::Float => r
                .samples::<f32>()
                .map(|s| s.map(|f| (f * 32767.0) as i16))
                .collect::<std::result::Result<_, _>>()
                .with_context(|| format!("decode wav {}", path.display()))?,
        };
        (inter, ch, spec.sample_rate)
    };
    if inter.is_empty() {
        bail!("sample {} decoded to zero frames", path.display());
    }
    if inter.len() % ch != 0 {
        bail!(
            "sample {}: {} samples not divisible by {ch} channels",
            path.display(),
            inter.len()
        );
    }
    Ok(LoadedSample {
        name,
        channels: deinterleave(inter, ch),
        sample_rate,
    })
}

// ── SF2 assembly ─────────────────────────────────────────────────────────────

// SF2 generator operators.
const GEN_START_FINE: u16 = 0; // startAddrsOffset (samples)
const GEN_START_COARSE: u16 = 4; // startAddrsCoarseOffset (×32768 samples)
const GEN_PAN: u16 = 17;
const GEN_DELAY_VOL: u16 = 33;
const GEN_ATTACK_VOL: u16 = 34;
const GEN_HOLD_VOL: u16 = 35;
const GEN_DECAY_VOL: u16 = 36;
const GEN_SUSTAIN_VOL: u16 = 37;
const GEN_RELEASE_VOL: u16 = 38;
const GEN_KEYRANGE: u16 = 43;
const GEN_VELRANGE: u16 = 44;
const GEN_INITIAL_ATTEN: u16 = 48;
const GEN_COARSE_TUNE: u16 = 51;
const GEN_FINE_TUNE: u16 = 52;
const GEN_SAMPLE_ID: u16 = 53;
const GEN_SAMPLE_MODES: u16 = 54;
const GEN_ROOT_KEY: u16 = 58;
const GEN_INSTRUMENT: u16 = 41;

fn sec_to_timecents(s: f32) -> i16 {
    if s <= 0.0 {
        -12000
    } else {
        (1200.0 * s.log2()).round().clamp(-12000.0, 8000.0) as i16
    }
}

fn sustain_pct_to_cb(pct: f32) -> i16 {
    if pct >= 100.0 {
        0
    } else if pct <= 0.0 {
        1440
    } else {
        (-200.0 * (pct / 100.0).log10()).round().clamp(0.0, 1440.0) as i16
    }
}

/// Emit a RIFF chunk (id + LE size + data + word-pad) into `out`. Fallible so
/// the u32 size field can never silently wrap — the invariant is "every chunk
/// payload (leaf data or enclosing LIST body) fits u32".
fn leaf(out: &mut Vec<u8>, id: &[u8; 4], data: &[u8]) -> Result<()> {
    let len = u32::try_from(data.len()).map_err(|_| {
        anyhow!(
            "RIFF chunk '{}' exceeds u32 size ({} bytes)",
            String::from_utf8_lossy(id),
            data.len()
        )
    })?;
    out.extend_from_slice(id);
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(data);
    if data.len() % 2 == 1 {
        out.push(0);
    }
    Ok(())
}

fn emit_gen(buf: &mut Vec<u8>, op: u16, amount: u16) {
    buf.extend_from_slice(&op.to_le_bytes());
    buf.extend_from_slice(&amount.to_le_bytes());
}

fn fixed20(name: &str) -> [u8; 20] {
    let mut b = [0u8; 20];
    let bytes = name.as_bytes();
    let n = bytes.len().min(19);
    b[..n].copy_from_slice(&bytes[..n]);
    b
}

// SF2 sampleType flags.
const ST_MONO: u16 = 1;
const ST_RIGHT: u16 = 2;
const ST_LEFT: u16 = 4;

/// Default headroom baked into every zone as SF2 `initialAttenuation`, in
/// centibels. Matches Polyphone's SFZ-import default (7.5 dB) so converted
/// banks are level-compatible drop-ins for existing Polyphone-made SF2s — a bank
/// swap keeps the same mix balance. rustysynth renders this via the SF2 spec's
/// 0.4× attenuation factor, i.e. 75 cB → 3.0 dB of actual level reduction.
const HEADROOM_CB: i32 = 75;

/// SFZ region `volume` (dB) → SF2 `initialAttenuation` (centibels), offset by
/// [`HEADROOM_CB`]: negative volume adds attenuation (cut), positive volume
/// spends the headroom (boost) toward 0 cB. SF2 can't go below 0, so boost
/// saturates once the headroom is exhausted (~+7.5 dB).
fn atten_cb(volume_db: f32) -> i16 {
    (HEADROOM_CB - (volume_db * 10.0).round() as i32).clamp(0, 1440) as i16
}

/// How a decoded source sample maps into SF2 sample headers.
#[derive(Clone, Copy)]
enum SampleRef {
    Mono(u16),
    Stereo { left: u16, right: u16 },
}

/// Append one shdr record + its PCM (with the 46-frame trailing guard) to the
/// pools. `link` is the partner shdr index for a stereo channel (0 for mono).
#[allow(clippy::too_many_arguments)]
fn push_shdr(
    pcm: &mut Vec<i16>,
    shdr: &mut Vec<u8>,
    name: &str,
    data: &[i16],
    sample_rate: u32,
    loop_pts: Option<(u32, u32)>,
    sample_type: u16,
    link: u16,
) {
    let start = pcm.len() as u32;
    pcm.extend_from_slice(data);
    let end = pcm.len() as u32;
    pcm.extend(std::iter::repeat_n(0i16, 46)); // SF2 trailing guard
    let (loop_lo, loop_hi) = match loop_pts {
        Some((ls, le)) => (
            start + ls.min(data.len() as u32),
            start + le.min(data.len() as u32),
        ),
        None => (start, end),
    };
    shdr.extend_from_slice(&fixed20(name));
    shdr.extend_from_slice(&start.to_le_bytes());
    shdr.extend_from_slice(&end.to_le_bytes());
    shdr.extend_from_slice(&loop_lo.to_le_bytes());
    shdr.extend_from_slice(&loop_hi.to_le_bytes());
    shdr.extend_from_slice(&sample_rate.to_le_bytes());
    shdr.push(60); // original pitch (root comes from the zone's overridingRootKey)
    shdr.push(0); // pitch correction
    shdr.extend_from_slice(&link.to_le_bytes());
    shdr.extend_from_slice(&sample_type.to_le_bytes());
}

/// Emit one instrument zone (an ibag record + its generators) for `region`,
/// referencing sample header `sample_id` and panned to `pan_cb` (-500..500).
/// keyRange is emitted first and sampleID last (SF2 rule); `*igen_records` is
/// advanced to the running generator count for the next zone's ibag.genNdx.
fn emit_zone(
    igen: &mut Vec<u8>,
    ibag: &mut Vec<u8>,
    igen_records: &mut u16,
    r: &Region,
    sample_id: u16,
    pan_cb: i16,
) -> Result<()> {
    ibag.extend_from_slice(&igen_records.to_le_bytes()); // genNdx = gens so far
    ibag.extend_from_slice(&0u16.to_le_bytes()); // modNdx
    emit_gen(
        igen,
        GEN_KEYRANGE,
        (r.lokey as u16) | ((r.hikey as u16) << 8),
    );
    emit_gen(
        igen,
        GEN_VELRANGE,
        (r.lovel as u16) | ((r.hivel as u16) << 8),
    );
    if r.offset > 0 {
        emit_gen(igen, GEN_START_COARSE, (r.offset / 32768) as u16);
        emit_gen(igen, GEN_START_FINE, (r.offset % 32768) as u16);
    }
    emit_gen(igen, GEN_ROOT_KEY, r.pitch_keycenter as u16);
    // Clamp to i16 so an absurd transpose/tune can't wrap the signed amount.
    let coarse = (r.transpose + r.tune_cents / 100).clamp(i16::MIN as i32, i16::MAX as i32);
    let fine = (r.tune_cents % 100).clamp(i16::MIN as i32, i16::MAX as i32);
    if coarse != 0 {
        emit_gen(igen, GEN_COARSE_TUNE, (coarse as i16) as u16);
    }
    if fine != 0 {
        emit_gen(igen, GEN_FINE_TUNE, (fine as i16) as u16);
    }
    let atten = atten_cb(r.volume_db);
    if atten != 0 {
        emit_gen(igen, GEN_INITIAL_ATTEN, atten as u16);
    }
    if pan_cb != 0 {
        emit_gen(igen, GEN_PAN, pan_cb as u16);
    }
    if r.amp_delay > 0.0 {
        emit_gen(igen, GEN_DELAY_VOL, sec_to_timecents(r.amp_delay) as u16);
    }
    if r.amp_attack > 0.0 {
        emit_gen(igen, GEN_ATTACK_VOL, sec_to_timecents(r.amp_attack) as u16);
    }
    if r.amp_hold > 0.0 {
        emit_gen(igen, GEN_HOLD_VOL, sec_to_timecents(r.amp_hold) as u16);
    }
    if r.amp_decay > 0.0 {
        emit_gen(igen, GEN_DECAY_VOL, sec_to_timecents(r.amp_decay) as u16);
    }
    if r.amp_sustain < 100.0 {
        emit_gen(
            igen,
            GEN_SUSTAIN_VOL,
            sustain_pct_to_cb(r.amp_sustain) as u16,
        );
    }
    if r.amp_release > 0.0 {
        emit_gen(
            igen,
            GEN_RELEASE_VOL,
            sec_to_timecents(r.amp_release) as u16,
        );
    }
    if r.loop_mode != LoopMode::NoLoop {
        let mode = if r.loop_mode == LoopMode::Continuous {
            1
        } else {
            3
        };
        emit_gen(igen, GEN_SAMPLE_MODES, mode);
    }
    emit_gen(igen, GEN_SAMPLE_ID, sample_id); // MUST be last
    if igen.len() / 4 > u16::MAX as usize {
        bail!("SFZ instrument exceeds the SF2 generator-index limit (65535)");
    }
    *igen_records = (igen.len() / 4) as u16;
    Ok(())
}

/// Build a complete uncompressed SF2 byte image from the parsed regions.
fn build_sf2(inst_name: &str, regions: &[Region]) -> Result<Vec<u8>> {
    // Load each distinct sample once.
    let mut sample_idx: HashMap<PathBuf, usize> = HashMap::new();
    let mut samples: Vec<LoadedSample> = Vec::new();
    for r in regions {
        if !sample_idx.contains_key(&r.sample) {
            let s = load_sample(&r.sample)?;
            sample_idx.insert(r.sample.clone(), samples.len());
            samples.push(s);
        }
    }

    // SF2 zone / sample / generator indices are all u16; a huge SFZ would
    // silently wrap and corrupt the pdta tables. Refuse instead of miscompiling.
    if regions.len() > u16::MAX as usize {
        bail!(
            "SFZ has {} regions; SF2 caps at 65535 instrument zones",
            regions.len()
        );
    }
    if samples.len() > u16::MAX as usize {
        bail!(
            "SFZ references {} distinct samples; SF2 caps at 65535",
            samples.len()
        );
    }
    // Sample addresses and the smpl chunk size are u32; refuse >4 GiB of PCM
    // rather than wrap shdr offsets / the RIFF size (each channel adds a
    // 46-frame guard). Stereo samples emit two shdr records, so bound those too.
    let total_frames: usize = samples
        .iter()
        .map(|s| s.channels.iter().map(|c| c.len() + 46).sum::<usize>())
        .sum();
    if total_frames.saturating_mul(2) > u32::MAX as usize {
        bail!("sample data ({total_frames} frames) exceeds the SF2 4 GiB RIFF limit");
    }
    let total_shdr: usize = samples.iter().map(|s| s.channels.len()).sum();
    if total_shdr > u16::MAX as usize {
        bail!("SFZ expands to {total_shdr} sample headers; SF2 caps at 65535");
    }

    // Per-sample loop points, taken from the first looping region that uses it
    // (frame offsets within the sample; applied to both channels of a stereo
    // sample).
    let mut sample_loop: Vec<Option<(u32, u32)>> = vec![None; samples.len()];
    for r in regions {
        if let (Some(ls), Some(le)) = (r.loop_start, r.loop_end) {
            let idx = sample_idx[&r.sample];
            sample_loop[idx].get_or_insert((ls, le));
        }
    }

    // ── sample pool (smpl) + shdr ── mono → 1 record; stereo → linked L/R pair.
    let mut pcm: Vec<i16> = Vec::new();
    let mut shdr: Vec<u8> = Vec::new();
    let mut sample_ref: Vec<SampleRef> = Vec::with_capacity(samples.len());
    let mut shdr_count: u16 = 0;
    for (si, s) in samples.iter().enumerate() {
        let loop_pts = sample_loop[si];
        if s.channels.len() >= 2 {
            let left = shdr_count;
            let right = shdr_count + 1;
            push_shdr(
                &mut pcm,
                &mut shdr,
                &format!("{}_L", s.name),
                &s.channels[0],
                s.sample_rate,
                loop_pts,
                ST_LEFT,
                right,
            );
            push_shdr(
                &mut pcm,
                &mut shdr,
                &format!("{}_R", s.name),
                &s.channels[1],
                s.sample_rate,
                loop_pts,
                ST_RIGHT,
                left,
            );
            shdr_count += 2;
            sample_ref.push(SampleRef::Stereo { left, right });
        } else {
            let idx = shdr_count;
            push_shdr(
                &mut pcm,
                &mut shdr,
                &s.name,
                &s.channels[0],
                s.sample_rate,
                loop_pts,
                ST_MONO,
                0,
            );
            shdr_count += 1;
            sample_ref.push(SampleRef::Mono(idx));
        }
    }
    // shdr terminal ("EOS")
    shdr.extend_from_slice(&fixed20("EOS"));
    shdr.extend_from_slice(&[0u8; 26]); // start..sampleType all zero

    let mut smpl: Vec<u8> = Vec::with_capacity(pcm.len() * 2);
    for s in &pcm {
        smpl.extend_from_slice(&s.to_le_bytes());
    }

    // ── igen + ibag ── 1 zone per mono region, 2 hard-panned zones per stereo.
    let mut igen: Vec<u8> = Vec::new();
    let mut ibag: Vec<u8> = Vec::new();
    let mut igen_records: u16 = 0;
    let mut zone_count: usize = 0;
    for r in regions {
        // SF2 pan: -500 = full left, +500 = full right. A mono region takes the
        // region pan directly; a stereo region's two channels sit hard L/R, and
        // a non-zero region pan shifts that whole image by `base` (both channels
        // move together, clamped). The target libraries pan their stereo samples
        // at 0, so that image-shift path is best-effort, not parity-verified.
        let base = (r.pan * 5.0).round();
        match sample_ref[sample_idx[&r.sample]] {
            SampleRef::Mono(id) => {
                let pan = base.clamp(-500.0, 500.0) as i16;
                emit_zone(&mut igen, &mut ibag, &mut igen_records, r, id, pan)?;
                zone_count += 1;
            }
            SampleRef::Stereo { left, right } => {
                let lp = (base - 500.0).clamp(-500.0, 500.0) as i16;
                let rp = (base + 500.0).clamp(-500.0, 500.0) as i16;
                emit_zone(&mut igen, &mut ibag, &mut igen_records, r, left, lp)?;
                emit_zone(&mut igen, &mut ibag, &mut igen_records, r, right, rp)?;
                zone_count += 2;
            }
        }
    }
    if zone_count > u16::MAX as usize {
        bail!("SFZ expands to {zone_count} instrument zones; SF2 caps at 65535");
    }
    // ibag terminal + igen terminal
    ibag.extend_from_slice(&igen_records.to_le_bytes());
    ibag.extend_from_slice(&0u16.to_le_bytes());
    emit_gen(&mut igen, 0, 0); // terminal generator

    // ── inst (single instrument) ──
    let mut inst: Vec<u8> = Vec::new();
    inst.extend_from_slice(&fixed20(inst_name));
    inst.extend_from_slice(&0u16.to_le_bytes()); // instBagNdx = 0
    inst.extend_from_slice(&fixed20("EOI"));
    inst.extend_from_slice(&(zone_count as u16).to_le_bytes()); // terminal → total ibag zones

    // ── preset layer: 1 preset → 1 zone → instrument 0 ──
    let mut pgen: Vec<u8> = Vec::new();
    emit_gen(&mut pgen, GEN_INSTRUMENT, 0);
    emit_gen(&mut pgen, 0, 0); // terminal
    let mut pbag: Vec<u8> = Vec::new();
    pbag.extend_from_slice(&0u16.to_le_bytes()); // genNdx 0
    pbag.extend_from_slice(&0u16.to_le_bytes()); // modNdx 0
    pbag.extend_from_slice(&1u16.to_le_bytes()); // terminal genNdx = 1 real pgen
    pbag.extend_from_slice(&0u16.to_le_bytes());
    let mut phdr: Vec<u8> = Vec::new();
    phdr.extend_from_slice(&fixed20(inst_name));
    phdr.extend_from_slice(&0u16.to_le_bytes()); // preset
    phdr.extend_from_slice(&0u16.to_le_bytes()); // bank
    phdr.extend_from_slice(&0u16.to_le_bytes()); // presetBagNdx 0
    phdr.extend_from_slice(&[0u8; 12]); // library/genre/morphology
    phdr.extend_from_slice(&fixed20("EOP"));
    phdr.extend_from_slice(&0u16.to_le_bytes()); // patch
    phdr.extend_from_slice(&0u16.to_le_bytes()); // bank
    phdr.extend_from_slice(&1u16.to_le_bytes()); // presetBagNdx = 1 (past the real zone)
    phdr.extend_from_slice(&[0u8; 12]);

    let pmod = vec![0u8; 10]; // single terminal modulator
    let imod = vec![0u8; 10];

    // ── assemble RIFF: sfbk { INFO, sdta{smpl}, pdta{...} } ──
    let mut info = Vec::new();
    info.extend_from_slice(b"INFO");
    leaf(&mut info, b"ifil", &[2, 0, 1, 0])?; // SF2 v2.1
    leaf(&mut info, b"isng", b"EMU8000\0")?;
    let mut nm = inst_name.as_bytes().to_vec();
    nm.push(0);
    if nm.len() % 2 == 1 {
        nm.push(0);
    }
    leaf(&mut info, b"INAM", &nm)?;

    let mut sdta = Vec::new();
    sdta.extend_from_slice(b"sdta");
    leaf(&mut sdta, b"smpl", &smpl)?;

    let mut pdta = Vec::new();
    pdta.extend_from_slice(b"pdta");
    leaf(&mut pdta, b"phdr", &phdr)?;
    leaf(&mut pdta, b"pbag", &pbag)?;
    leaf(&mut pdta, b"pmod", &pmod)?;
    leaf(&mut pdta, b"pgen", &pgen)?;
    leaf(&mut pdta, b"inst", &inst)?;
    leaf(&mut pdta, b"ibag", &ibag)?;
    leaf(&mut pdta, b"imod", &imod)?;
    leaf(&mut pdta, b"igen", &igen)?;
    leaf(&mut pdta, b"shdr", &shdr)?;

    let mut body = Vec::new();
    body.extend_from_slice(b"sfbk");
    leaf(&mut body, b"LIST", &info)?;
    leaf(&mut body, b"LIST", &sdta)?;
    leaf(&mut body, b"LIST", &pdta)?;

    let total = u32::try_from(body.len())
        .map_err(|_| anyhow!("SF2 exceeds RIFF u32 size ({} bytes)", body.len()))?;
    let mut out = Vec::with_capacity(body.len() + 8);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&total.to_le_bytes());
    out.extend_from_slice(&body);
    Ok(out)
}

/// Convert an `.sfz` file into an uncompressed SF2 byte image (in memory).
/// Returns the SF2 bytes plus a `(ignored_opcodes, skipped_regions)` report.
pub fn sfz_to_sf2(path: &Path) -> Result<(Vec<u8>, Vec<String>, usize)> {
    let mut ignored = Vec::new();
    let mut skipped = 0usize;
    let regions = parse_sfz(path, &mut ignored, &mut skipped)?;
    let name = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "SFZ".into());
    let sf2 = build_sf2(&name, &regions)?;
    Ok((sf2, ignored, skipped))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn note_names_parse() {
        assert_eq!(parse_note("60"), Some(60));
        assert_eq!(parse_note("c4"), Some(60));
        assert_eq!(parse_note("A4"), Some(69));
        assert_eq!(parse_note("f#3"), Some(54));
        assert_eq!(parse_note("Bb2"), Some(46));
    }

    #[test]
    fn builds_sf2_that_rustysynth_loads() {
        use std::io::Cursor;
        let dir = std::env::temp_dir().join(format!("cosmix_sfz_test_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let wav = dir.join("tone.wav");
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 44100,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut w = hound::WavWriter::create(&wav, spec).unwrap();
        for i in 0..2000 {
            w.write_sample(((i as f32 * 0.1).sin() * 8000.0) as i16)
                .unwrap();
        }
        w.finalize().unwrap();
        // Two regions (key-split) so the pdta tables have >1 instrument zone.
        let sfz = dir.join("t.sfz");
        std::fs::write(
            &sfz,
            "<global> ampeg_release=0.3\n\
             <region> sample=tone.wav lokey=0 hikey=59 pitch_keycenter=48 lovel=1 hivel=127\n\
             <region> sample=tone.wav lokey=60 hikey=127 pitch_keycenter=72 lovel=1 hivel=127\n",
        )
        .unwrap();
        let (sf2, _ignored, _skipped) = sfz_to_sf2(&sfz).unwrap();
        rustysynth::SoundFont::new(&mut Cursor::new(sf2))
            .expect("rustysynth must load the converted SF2");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn include_and_define_expand() {
        use std::io::Cursor;
        let root = std::env::temp_dir().join(format!("cosmix_sfz_inc_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("Samples")).unwrap();
        std::fs::create_dir_all(root.join("maps")).unwrap();
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 44100,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut w = hound::WavWriter::create(root.join("Samples/tone.wav"), spec).unwrap();
        for i in 0..1000 {
            w.write_sample(((i as f32 * 0.1).sin() * 8000.0) as i16)
                .unwrap();
        }
        w.finalize().unwrap();
        // map file references the sample via a #define'd path with a backslash
        std::fs::write(
            root.join("maps/m.sfz"),
            "<region> sample=$DIR\\tone.wav key=60 lovel=1 hivel=127\n",
        )
        .unwrap();
        // root defines $DIR and includes the map
        std::fs::write(
            root.join("root.sfz"),
            "#define $DIR Samples\n<global> ampeg_release=0.2\n#include \"maps/m.sfz\"\n",
        )
        .unwrap();
        let (mut ign, mut skip) = (Vec::new(), 0usize);
        let regions = parse_sfz(&root.join("root.sfz"), &mut ign, &mut skip).unwrap();
        assert_eq!(regions.len(), 1, "#include + #define must yield the region");
        assert!(
            regions[0].sample.ends_with("tone.wav") && regions[0].sample.exists(),
            "sample path resolves via $DIR + backslash: {:?}",
            regions[0].sample
        );
        let (sf2, _, _) = sfz_to_sf2(&root.join("root.sfz")).unwrap();
        rustysynth::SoundFont::new(&mut Cursor::new(sf2)).expect("converted SF2 loads");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn midline_includes_expand() {
        // Salamander V3 shape: a <group> line carrying TWO mid-line #includes —
        // the velocity-layer opcodes and the whole region list are include files,
        // so a line-leading-only expander yields zero regions.
        let root = std::env::temp_dir().join(format!("cosmix_sfz_midinc_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("Data")).unwrap();
        std::fs::create_dir_all(root.join("Samples")).unwrap();
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 44100,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut w = hound::WavWriter::create(root.join("Samples/A0v1.wav"), spec).unwrap();
        for i in 0..1000 {
            w.write_sample(((i as f32 * 0.1).sin() * 8000.0) as i16)
                .unwrap();
        }
        w.finalize().unwrap();
        std::fs::write(root.join("Data/vel_01.txt"), "volume=-3\n").unwrap();
        std::fs::write(
            root.join("Data/region.txt"),
            "<region> sample=A0v1.$EXT lokey=21 hikey=22 pitch_keycenter=21\n",
        )
        .unwrap();
        std::fs::write(
            root.join("root.sfz"),
            "#define $EXT wav\n<control> default_path=Samples/\n\
             <group> #include \"Data/vel_01.txt\" lovel=1 hivel=26 #include \"Data/region.txt\"\n",
        )
        .unwrap();
        let (mut ign, mut skip) = (Vec::new(), 0usize);
        let regions = parse_sfz(&root.join("root.sfz"), &mut ign, &mut skip).unwrap();
        assert_eq!(regions.len(), 1, "mid-line #includes must yield the region");
        assert!(
            regions[0].sample.ends_with("A0v1.wav") && regions[0].sample.exists(),
            "sample resolves via default_path + $EXT define: {:?}",
            regions[0].sample
        );
        // Group opcodes around the includes must still apply to the region.
        assert_eq!((regions[0].lovel, regions[0].hivel), (1, 26));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn keyswitch_layers_keep_only_default() {
        // Salamander shape: two <master> layers over the same notes, selected
        // by sw_last, with sw_default naming one. SF2 has no keyswitches —
        // keeping both stacks (detuned-doubles) every note. Only the default
        // layer may survive.
        let root = std::env::temp_dir().join(format!("cosmix_sfz_ks_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 44100,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut w = hound::WavWriter::create(root.join("tone.wav"), spec).unwrap();
        for i in 0..1000 {
            w.write_sample(((i as f32 * 0.1).sin() * 8000.0) as i16)
                .unwrap();
        }
        w.finalize().unwrap();
        std::fs::write(
            root.join("root.sfz"),
            "<global> sw_lokey=C0 sw_hikey=C#0 sw_default=C0\n\
             <master> sw_last=C0\n<region> sample=tone.wav key=60\n\
             <master> sw_last=C#0\n<region> sample=tone.wav key=60 tune=-30\n",
        )
        .unwrap();
        let (mut ign, mut skip) = (Vec::new(), 0usize);
        let regions = parse_sfz(&root.join("root.sfz"), &mut ign, &mut skip).unwrap();
        assert_eq!(regions.len(), 1, "only the sw_default layer survives");
        assert_eq!(skip, 1, "the non-default layer counts as skipped");
        assert_eq!(
            regions[0].tune_cents, 0,
            "surviving region is the C0 (natural) layer"
        );
        // No sw_default declared → the FIRST sw_last layer is the default.
        std::fs::write(
            root.join("nodef.sfz"),
            "<master> sw_last=c#0\n<region> sample=tone.wav key=60 tune=-30\n\
             <master> sw_last=C#0\n<region> sample=tone.wav key=61 tune=-30\n\
             <master> sw_last=D0\n<region> sample=tone.wav key=60\n",
        )
        .unwrap();
        let (mut ign2, mut skip2) = (Vec::new(), 0usize);
        let regions = parse_sfz(&root.join("nodef.sfz"), &mut ign2, &mut skip2).unwrap();
        assert_eq!(
            regions.len(),
            2,
            "first-seen sw_last layer survives (both spellings)"
        );
        assert!(regions.iter().all(|r| r.tune_cents == -30));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn aria_macros_generators_roundrobins() {
        let root = std::env::temp_dir().join(format!("cosmix_sfz_aria_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("Samples")).unwrap();
        std::fs::create_dir_all(root.join("inc")).unwrap();
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 44100,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut w = hound::WavWriter::create(root.join("Samples/kit_hit.wav"), spec).unwrap();
        for i in 0..1000 {
            w.write_sample(((i as f32 * 0.1).sin() * 8000.0) as i16)
                .unwrap();
        }
        w.finalize().unwrap();
        // Deepest include uses a ROOT-relative path ("inc/more.sfz" from inc/) +
        // a round-robin variant (seq_position=2 → skipped).
        std::fs::write(
            root.join("inc/more.sfz"),
            "<region> sample=Samples/$NAME_hit.wav key=62 seq_position=2\n",
        )
        .unwrap();
        // Mid file: a real region ($NAME_hit → kit_hit via `_` separator) + a
        // *silence generator (skipped) + the nested root-relative include.
        std::fs::write(
            root.join("inc/part.sfz"),
            "<region> sample=Samples/$NAME_hit.wav key=60\n\
             <region> sample=*silence key=61\n\
             #include \"inc/more.sfz\"\n",
        )
        .unwrap();
        std::fs::write(
            root.join("root.sfz"),
            "#define $NAME kit\n<global> ampeg_release=0.2\n#include \"inc/part.sfz\"\n",
        )
        .unwrap();
        let (mut ign, mut skip) = (Vec::new(), 0usize);
        let regions = parse_sfz(&root.join("root.sfz"), &mut ign, &mut skip).unwrap();
        assert_eq!(regions.len(), 1, "only the one real region survives");
        assert!(
            regions[0].sample.ends_with("kit_hit.wav") && regions[0].sample.exists(),
            "$NAME_hit resolves across the `_` separator: {:?}",
            regions[0].sample
        );
        assert!(skip >= 2, "*silence + round-robin skipped, got {skip}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn unknown_header_does_not_duplicate_region() {
        let dir = std::env::temp_dir().join(format!("cosmix_sfz_curve_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let sfz = dir.join("c.sfz");
        // A <curve> after a <region> must not leak into the region or re-push it.
        std::fs::write(
            &sfz,
            "<region> sample=a.wav key=60\n<curve> curve_index=1 v000=0 v127=1\n",
        )
        .unwrap();
        let (mut ign, mut skip) = (Vec::new(), 0usize);
        let regions = parse_sfz(&sfz, &mut ign, &mut skip).unwrap();
        assert_eq!(regions.len(), 1, "unknown header must not duplicate region");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn envelope_conversions() {
        assert_eq!(sec_to_timecents(1.0), 0); // 1s → 0 timecents
        assert!(sec_to_timecents(0.05) < -4000); // fast attack
        assert_eq!(sustain_pct_to_cb(100.0), 0);
        assert!(sustain_pct_to_cb(50.0) > 0);
        assert_eq!(atten_cb(0.0), 75); // headroom only
        assert_eq!(atten_cb(-6.0), 135); // headroom + 6 dB cut
        assert_eq!(atten_cb(6.0), 15); // spends headroom (boost)
        assert_eq!(atten_cb(20.0), 0); // boost saturates at 0
    }
}
