pub mod analyzer;
pub mod ast;
pub mod builtin_info;
pub mod builtins;
pub mod builtins_hof;
pub mod continuation;
pub mod error;
pub mod evaluator;
pub mod interrupt;
pub mod lexer;
mod numeric;
pub mod parser;
pub mod scope;
pub mod stats;
pub mod token;
pub mod value;

#[cfg(feature = "json")]
pub mod json;

#[cfg(feature = "json")]
pub mod jq;

/// Serde `Deserializer` over the `Value` tree — typed config structs
/// hydrate from a `.conf.mix` parse via `#[derive(Deserialize)]`.
#[cfg(feature = "serde")]
pub mod serde_de;

/// Serde `Serializer` building a `Value` tree — typed config structs
/// emit `.conf.mix` text via `#[derive(Serialize)]` + `to_mix_data_string`.
#[cfg(feature = "serde")]
pub mod serde_ser;

#[cfg(feature = "serde")]
pub use serde_de::{DeError, from_conf_mix_str, from_value};

#[cfg(feature = "serde")]
pub use serde_ser::{SerError, to_conf_mix_string, to_value};

use evaluator::Evaluator;
use lexer::Lexer;
use parser::Parser;
use value::Value;

pub use builtins::{CapabilityClass, CategoryAllowList, capability_category, set_script_argv};
pub use error::{MixError, MixResult};
pub use evaluator::{
    ArityMode, BusCallFuture, BusCallHandler, BusHandler, CapabilityPolicy,
    DEFAULT_RECURSION_LIMIT, DbFuture, DbHandler, EvalLimits, ExtFn, JmapCall, JmapFuture,
    JmapHandler, SharedBuf, sync_ext,
};
/// Re-exported so embedders can build a `Value::Map` (which wraps
/// `IndexMap<String, Value>`) without a version-coupled `indexmap`
/// dependency of their own — use `cosmix_mix::IndexMap`.
pub use indexmap::IndexMap;

/// Run Mix source code and return the result.
pub async fn run(source: &str) -> MixResult<Value> {
    let mut lexer = Lexer::new(source);
    let tokens = lexer.tokenize()?;
    let mut parser = Parser::new(tokens, source);
    let stmts = parser.parse_program()?;
    let mut eval = Evaluator::new();
    eval.execute(&stmts).await
}

/// Run Mix source code, capturing stdout and stderr.
pub async fn run_capturing(source: &str) -> MixResult<(Value, String, String)> {
    let mut lexer = Lexer::new(source);
    let tokens = lexer.tokenize()?;
    let mut parser = Parser::new(tokens, source);
    let stmts = parser.parse_program()?;

    let stdout = SharedBuf::new();
    let stderr = SharedBuf::new();
    let mut eval = Evaluator::with_output(Box::new(stdout.clone()), Box::new(stderr.clone()));
    let result = eval.execute(&stmts).await?;

    Ok((result, stdout.to_string_lossy(), stderr.to_string_lossy()))
}

/// Parse a Mix source string in strict-data mode.
///
/// Accepts only the literal-data subset (scalars, lists, maps,
/// nested combinations); rejects every executable construct
/// (variable references, calls, send/sh, interpolation, control
/// flow, arithmetic) with `MixError::StrictDataViolation`. The
/// result is a tree-shaped `Value` that round-trips through
/// `Value::write_mix_data` (note: not `write_mix` — that path is
/// for `print`-style human output and emits unquoted strings,
/// which won't survive re-parse for arbitrary content).
///
/// This is the entry point for `.spec.mix` / `.conf.mix` /
/// `.journal.mix` / `.verdict.mix` / `.call.mix` / `.state.mix`
/// substrate-internal data files. See
/// `_doc/2026-05-04-substrate-mix-data-formats.md`.
pub fn parse_data(source: &str) -> MixResult<Value> {
    let mut lexer = Lexer::new(source);
    let tokens = lexer.tokenize()?;
    let mut parser = Parser::new(tokens, source);
    parser.parse_data()
}

/// Read a `.*.mix` data file from disk and parse it in strict-data
/// mode. Convenience wrapper over `parse_data`.
pub fn parse_data_file(path: &std::path::Path) -> MixResult<Value> {
    let source = std::fs::read_to_string(path).map_err(|e| error::MixError::RuntimeError {
        msg: format!("failed to read {}: {}", path.display(), e),
        span: None,
    })?;
    parse_data(&source)
}
