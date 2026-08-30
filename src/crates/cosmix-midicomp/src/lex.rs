//! Tokeniser for the midicomp text format (the `text → SMF` / `-c` direction).
//!
//! Implemented from the documented grammar (see `README.md`), cross-checked
//! against the original flex scanner's *behaviour* (not its generated code):
//! case-insensitive keywords, `ch=`-style params, decimal / `0x`hex / `$`|`'`bank
//! numbers, symbolic notes, `#` comments to end-of-line, `\`-folded continuation
//! lines, and double-quoted strings with C-style escapes.
//!
//! The scanner is line-state-light: `next()` lexes a single INITIAL-mode token;
//! binary/string payloads (SysEx / Meta / SeqSpec hex or strings) are pulled by
//! the parser through [`Lexer::read_hex`], which mirrors the original `gethex()`
//! — a quoted string *or* a run of hex bytes, interchangeably.

/// A single lexical token. Keyword spellings are folded to one variant each
/// (e.g. `Par`/`Param` → [`Tok::Par`], `TrkName`/`SeqName` → [`Tok::SeqName`]).
#[derive(Debug, Clone, PartialEq)]
pub enum Tok {
    MThd,
    MTrk,
    TrkEnd,
    On,
    Off,
    PoPr,
    Par,
    Pb,
    PrCh,
    ChPr,
    SysEx,
    Meta,
    SeqSpec,
    Text,
    Copyright,
    SeqName,
    InstrName,
    Lyric,
    Marker,
    Cue,
    SeqNr,
    KeySig,
    Tempo,
    TimeSig,
    Smpte,
    Arb,
    /// `:` or `/` (the bar:beat:click separator).
    Slash,
    Minor,
    Major,
    /// `ch=`
    Ch,
    /// `n=` / `note=`
    NoteP,
    /// `v=` / `vol=` / `val=`
    Val,
    /// `c=` / `con=`
    Con,
    /// `p=` / `prog=`
    Prog,
    /// A decimal / `0x`hex / `$`|`'`bank integer.
    Int(i64),
    /// A resolved symbolic note (`c4`, `f#3`, `eb2`) as a MIDI pitch 0..=127.
    NoteVal(i64),
    /// End of a (logical) line.
    Eol,
    /// End of input.
    Eof,
    /// An unrecognised lexeme (one or more chars); the parser reports it.
    Bad(String),
}

/// Keyword table: `(lowercase spelling, token)`. Matched case-insensitively and
/// by longest-prefix against the symbolic-note and error alternatives.
const KEYWORDS: &[(&str, Tok)] = &[
    ("mfile", Tok::MThd),
    ("mtrk", Tok::MTrk),
    ("trkend", Tok::TrkEnd),
    ("on", Tok::On),
    ("off", Tok::Off),
    ("polypr", Tok::PoPr),
    ("popr", Tok::PoPr),
    ("param", Tok::Par),
    ("par", Tok::Par),
    ("pb", Tok::Pb),
    ("progch", Tok::PrCh),
    ("prch", Tok::PrCh),
    ("chanpr", Tok::ChPr),
    ("chpr", Tok::ChPr),
    ("sysex", Tok::SysEx),
    ("meta", Tok::Meta),
    ("seqspec", Tok::SeqSpec),
    ("text", Tok::Text),
    ("copyright", Tok::Copyright),
    ("trkname", Tok::SeqName),
    ("seqname", Tok::SeqName),
    ("instrname", Tok::InstrName),
    ("lyric", Tok::Lyric),
    ("marker", Tok::Marker),
    ("cue", Tok::Cue),
    ("seqnr", Tok::SeqNr),
    ("keysig", Tok::KeySig),
    ("tempo", Tok::Tempo),
    ("timesig", Tok::TimeSig),
    ("smpte", Tok::Smpte),
    ("arb", Tok::Arb),
    ("minor", Tok::Minor),
    ("major", Tok::Major),
];

/// Param table: `(lowercase name, token)`. A param is the name immediately
/// followed by `=`.
const PARAMS: &[(&str, Tok)] = &[
    ("ch", Tok::Ch),
    ("note", Tok::NoteP),
    ("n", Tok::NoteP),
    ("vol", Tok::Val),
    ("val", Tok::Val),
    ("v", Tok::Val),
    ("con", Tok::Con),
    ("c", Tok::Con),
    ("prog", Tok::Prog),
    ("p", Tok::Prog),
];

pub struct Lexer<'a> {
    b: &'a [u8],
    i: usize,
    pub line: usize,
    /// Set when a quoted string hit a newline or EOF before its closing quote;
    /// the parser rejects the input rather than encode a truncated payload.
    pub str_err: bool,
    /// Set when a hex payload run stopped at non-hex garbage before EOL (e.g.
    /// `f0 7g`); the parser rejects it rather than silently drop the tail.
    pub hex_err: bool,
}

fn is_hex(c: u8) -> bool {
    c.is_ascii_hexdigit()
}

impl<'a> Lexer<'a> {
    pub fn new(s: &'a [u8]) -> Self {
        Lexer {
            b: s,
            i: 0,
            line: 1,
            str_err: false,
            hex_err: false,
        }
    }

    /// Skip spaces, tabs, CRs, `#`-comments (to but not including the newline,
    /// so the newline still yields [`Tok::Eol`]), and `\`-folded line
    /// continuations (`\` + optional whitespace + newline).
    fn skip_ws_comments(&mut self) {
        loop {
            while self.i < self.b.len() && matches!(self.b[self.i], b' ' | b'\t' | b'\r') {
                self.i += 1;
            }
            // Line continuation: backslash, optional whitespace, newline.
            if self.i < self.b.len() && self.b[self.i] == b'\\' {
                let mut j = self.i + 1;
                while j < self.b.len() && matches!(self.b[j], b' ' | b'\t' | b'\r') {
                    j += 1;
                }
                if j < self.b.len() && self.b[j] == b'\n' {
                    self.line += 1;
                    self.i = j + 1;
                    continue;
                }
            }
            // Comment: `#` to end of line (newline left for the Eol token).
            if self.i < self.b.len() && self.b[self.i] == b'#' {
                while self.i < self.b.len() && self.b[self.i] != b'\n' {
                    self.i += 1;
                }
                continue;
            }
            break;
        }
    }

    /// Lex one INITIAL-mode token.
    pub fn next(&mut self) -> Tok {
        self.skip_ws_comments();
        if self.i >= self.b.len() {
            return Tok::Eof;
        }
        let c = self.b[self.i];

        if c == b'\n' {
            self.i += 1;
            self.line += 1;
            return Tok::Eol;
        }
        if c == b':' || c == b'/' {
            self.i += 1;
            return Tok::Slash;
        }
        if c == b'"' {
            // A bare string in INITIAL mode (rare); return as a 0x01-style
            // payload is the parser's job, so surface raw bytes via Bad-free
            // path is not needed — strings are normally read through read_hex.
            let s = self.read_string();
            return Tok::Bad(String::from_utf8_lossy(&s).into_owned());
        }

        // 0x hex literal.
        if c == b'0'
            && self.i + 2 < self.b.len()
            && (self.b[self.i + 1] == b'x' || self.b[self.i + 1] == b'X')
            && is_hex(self.b[self.i + 2])
        {
            let mut j = self.i + 2;
            while j < self.b.len() && is_hex(self.b[j]) {
                j += 1;
            }
            let text = std::str::from_utf8(&self.b[self.i + 2..j]).unwrap_or("");
            let bad = String::from_utf8_lossy(&self.b[self.i..j]).into_owned();
            self.i = j;
            // Overflow is a hard error, not a silent clamp — a clamped value
            // would otherwise flow into negation/multiplication unchecked.
            return match i64::from_str_radix(text, 16) {
                Ok(v) => Tok::Int(v),
                Err(_) => Tok::Bad(bad),
            };
        }

        // Decimal (optionally signed).
        if c.is_ascii_digit()
            || ((c == b'-' || c == b'+')
                && self.i + 1 < self.b.len()
                && self.b[self.i + 1].is_ascii_digit())
        {
            let start = self.i;
            if c == b'-' || c == b'+' {
                self.i += 1;
            }
            while self.i < self.b.len() && self.b[self.i].is_ascii_digit() {
                self.i += 1;
            }
            let text = std::str::from_utf8(&self.b[start..self.i]).unwrap_or("");
            // Overflow is a hard error, not a silent clamp.
            return match text.parse::<i64>() {
                Ok(v) => Tok::Int(v),
                Err(_) => Tok::Bad(text.to_string()),
            };
        }

        // Bank number: `$` or `'` followed by [A-H1-8] (octal-ish, base 8).
        if c == b'$' || c == b'\'' {
            let mut j = self.i + 1;
            let start = j;
            while j < self.b.len() && matches!(self.b[j], b'A'..=b'H' | b'a'..=b'h' | b'1'..=b'8') {
                j += 1;
            }
            if j > start {
                let v = bankno(&self.b[start..j]);
                self.i = j;
                return Tok::Int(v);
            }
        }

        if c.is_ascii_alphabetic() {
            return self.alpha();
        }

        // Anything else: a one-char bad token.
        self.i += 1;
        Tok::Bad((c as char).to_string())
    }

    /// Dispatch an alphabetic lexeme: param (`name=`), keyword, symbolic note,
    /// or an error run — resolved by longest match (keyword beats note beats
    /// error on ties, mirroring the flex rule order).
    fn alpha(&mut self) -> Tok {
        // Param: maximal letter run immediately followed by `=`.
        let run_end = {
            let mut j = self.i;
            while j < self.b.len() && self.b[j].is_ascii_alphabetic() {
                j += 1;
            }
            j
        };
        if run_end < self.b.len() && self.b[run_end] == b'=' {
            let name = &self.b[self.i..run_end];
            if let Some((_, tok)) = PARAMS
                .iter()
                .find(|(n, _)| name.eq_ignore_ascii_case(n.as_bytes()))
            {
                self.i = run_end + 1;
                return tok.clone();
            }
        }

        // Longest keyword prefix (case-insensitive). The comparison is
        // allocation-free (`eq_ignore_ascii_case` on the raw bytes), so an
        // opcode token costs one byte compare per keyword, not a heap String.
        let mut kw_len = 0;
        let mut kw_tok = Tok::Eof;
        for (name, tok) in KEYWORDS {
            let n = name.len();
            if self.i + n <= self.b.len()
                && self.b[self.i..self.i + n].eq_ignore_ascii_case(name.as_bytes())
                && n > kw_len
            {
                kw_len = n;
                kw_tok = tok.clone();
            }
        }

        // Symbolic note: [a-g] [accidental]? [0-9]+
        let nv = self.note_match();
        let nv_len = nv.map(|(len, _)| len).unwrap_or(0);

        // Error run: maximal letters.
        let err_len = run_end - self.i;

        if kw_len >= nv_len && kw_len >= err_len && kw_len > 0 {
            self.i += kw_len;
            return kw_tok;
        }
        if nv_len >= err_len && nv_len > 0 {
            let (len, pitch) = nv.unwrap();
            self.i += len;
            return Tok::NoteVal(pitch);
        }
        let bad = String::from_utf8_lossy(&self.b[self.i..run_end]).into_owned();
        self.i = run_end;
        Tok::Bad(bad)
    }

    /// Match a symbolic note at the cursor, returning `(byte_len, pitch)`.
    /// Letters a–g map to semitones; `#`/`+` sharpen, `b`/`B`/`-` flatten;
    /// the trailing integer is the octave (`c4` = 48, matching `mknote`).
    fn note_match(&self) -> Option<(usize, i64)> {
        const SEMI: [i64; 7] = [9, 11, 0, 2, 4, 5, 7]; // a..g
        let s = self.b;
        let mut j = self.i;
        if j >= s.len() {
            return None;
        }
        let c = s[j].to_ascii_lowercase();
        if !(b'a'..=b'g').contains(&c) {
            return None;
        }
        let mut pitch = SEMI[(c - b'a') as usize];
        j += 1;
        if j < s.len() {
            match s[j] {
                b'#' | b'+' => {
                    pitch += 1;
                    j += 1;
                }
                b'b' | b'B' | b'-' => {
                    pitch -= 1;
                    j += 1;
                }
                _ => {}
            }
        }
        let oct_start = j;
        while j < s.len() && s[j].is_ascii_digit() {
            j += 1;
        }
        if j == oct_start {
            return None; // no octave digits → not a note
        }
        let oct: i64 = std::str::from_utf8(&s[oct_start..j])
            .unwrap_or("0")
            .parse()
            .unwrap_or(0);
        Some((j - self.i, pitch + 12 * oct))
    }

    /// Read a double-quoted string at the cursor, applying escapes, and return
    /// the decoded bytes. Mirrors the original `gethex()` string handling:
    /// `\0 \n \r \t \xHH`, `\<newline>` line-fold (leading whitespace on the
    /// continuation skipped), and `\<c>` → literal `c` otherwise.
    pub fn read_string(&mut self) -> Vec<u8> {
        let mut out = Vec::new();
        if self.i < self.b.len() && self.b[self.i] == b'"' {
            self.i += 1;
        }
        let mut terminated = false;
        while self.i < self.b.len() {
            let c = self.b[self.i];
            self.i += 1;
            match c {
                b'"' => {
                    terminated = true;
                    break;
                }
                b'\n' => {
                    // Unterminated; leave the newline for the Eol token.
                    self.i -= 1;
                    break;
                }
                b'\\' => {
                    if self.i >= self.b.len() {
                        break;
                    }
                    let e = self.b[self.i];
                    self.i += 1;
                    match e {
                        b'0' => out.push(0),
                        b'n' => out.push(b'\n'),
                        b'r' => out.push(b'\r'),
                        b't' => out.push(b'\t'),
                        b'x' => {
                            let mut val = 0u8;
                            let mut k = 0;
                            while k < 2 && self.i < self.b.len() && is_hex(self.b[self.i]) {
                                val = val * 16 + hexval(self.b[self.i]);
                                self.i += 1;
                                k += 1;
                            }
                            // `\xHH` needs at least one hex digit; with none,
                            // treat `\x` as a literal `x` (don't emit a stray 0).
                            if k == 0 {
                                out.push(b'x');
                            } else {
                                out.push(val);
                            }
                        }
                        b'\r' | b'\n' => {
                            // Line fold: skip the newline run and any leading
                            // whitespace on the continuation line.
                            if e == b'\r' && self.i < self.b.len() && self.b[self.i] == b'\n' {
                                self.i += 1;
                            }
                            while self.i < self.b.len()
                                && matches!(self.b[self.i], b' ' | b'\t' | b'\r' | b'\n')
                            {
                                if self.b[self.i] == b'\n' {
                                    self.line += 1;
                                }
                                self.i += 1;
                            }
                        }
                        other => out.push(other),
                    }
                }
                _ => out.push(c),
            }
        }
        if !terminated {
            self.str_err = true;
        }
        out
    }

    /// Read a binary payload: a quoted string *or* a whitespace-/contiguous run
    /// of 1–2 digit hex bytes, up to end-of-line. Mirrors `gethex()`; strings
    /// and hex are interchangeable wherever one is accepted.
    pub fn read_hex(&mut self) -> Vec<u8> {
        self.skip_ws_comments();
        if self.i < self.b.len() && self.b[self.i] == b'"' {
            return self.read_string();
        }
        let mut out = Vec::new();
        loop {
            self.skip_ws_comments();
            if self.i >= self.b.len() || !is_hex(self.b[self.i]) {
                break;
            }
            // One byte = up to two contiguous hex digits.
            let mut val = hexval(self.b[self.i]);
            self.i += 1;
            if self.i < self.b.len() && is_hex(self.b[self.i]) {
                val = val * 16 + hexval(self.b[self.i]);
                self.i += 1;
            }
            out.push(val);
        }
        // After the run, anything other than end-of-line is garbage — flag it
        // so a malformed tail (`f0 7g`) is rejected, not silently dropped.
        if self.i < self.b.len() && self.b[self.i] != b'\n' {
            self.hex_err = true;
        }
        out
    }
}

fn hexval(c: u8) -> u8 {
    match c {
        b'0'..=b'9' => c - b'0',
        b'a'..=b'f' => c - b'a' + 10,
        b'A'..=b'F' => c - b'A' + 10,
        _ => 0,
    }
}

/// Decode a `$`/`'` bank number: base-8 with digits `1`–`8` → `0`–`7` and
/// letters `a`–`h`/`A`–`H` → `0`–`7` (so `$123` == octal `012`).
fn bankno(s: &[u8]) -> i64 {
    let mut res = 0i64;
    for &c in s {
        let d = if c.is_ascii_lowercase() {
            (c - b'a') as i64
        } else if c.is_ascii_uppercase() {
            (c - b'A') as i64
        } else {
            (c - b'1') as i64
        };
        res = res.saturating_mul(8).saturating_add(d);
    }
    res
}
