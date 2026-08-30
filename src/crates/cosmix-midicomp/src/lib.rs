//! cosmix-midicomp — SMF ⇄ plain-text converter core.
//!
//! The library behind the `cosmix-midicomp` CLI: [`decode_smf_to_text`]
//! (SMF → text) and [`encode_text_to_smf`] (text → SMF), both pure in-memory
//! functions. The text grammar, number/note/string rules, and column layout
//! are documented in `README.md`; the `examples/songs/` corpus pins the
//! canonical round-trip byte-exact in both directions.
//!
//! The decode path is a faithful port of the C reader's tolerant behaviour;
//! the encode path tokenises the documented grammar and serialises through
//! the `midly` SMF writer (whose minimal running-status output makes the
//! round-trip byte-exact).

mod decode;
mod encode;
mod lex;

/// Rendering/parsing options shared by both converter directions.
///
/// `Default` gives the plain canonical form: ticks, numeric notes, no
/// folding — the form the round-trip corpus is pinned to.
#[derive(Debug, Clone, Default)]
pub struct Options {
    /// Aligned columns, note names on.
    pub verbose: bool,
    /// Note on/off values as symbolic note|octave.
    pub note: bool,
    /// Absolute time instead of ticks.
    pub time: bool,
    /// Incremental (delta) time values instead of absolute.
    pub inc: bool,
    /// Fold sysex / long strings at N columns.
    pub fold: Option<usize>,
}

/// SMF bytes → midicomp text, plus a flag set when the input was malformed
/// (the text still holds all decodable output). See [`decode::decode`].
pub fn decode_smf_to_text(smf: &[u8], opts: &Options) -> anyhow::Result<(String, bool)> {
    decode::decode(smf, opts)
}

/// midicomp text → SMF bytes. See [`encode::encode`].
pub fn encode_text_to_smf(text: &str, opts: &Options) -> anyhow::Result<Vec<u8>> {
    encode::encode(text, opts)
}
