//! 09-24 entry: a required-module fn that assigns a local `$rows` and
//! then calls the module's own `rows()` raised FUNCTION_UNDEFINED — the
//! frame-injected sibling function was shadowed by the same-named local
//! variable. A non-Function variable must not shadow a module function.

use cosmix_mix::evaluator::Evaluator;
use cosmix_mix::lexer::Lexer;
use cosmix_mix::parser::Parser;

async fn run(src: &str) -> Result<String, String> {
    let mut lexer = Lexer::new(src);
    let tokens = lexer.tokenize().map_err(|e| e.to_string())?;
    let mut parser = Parser::new(tokens, src);
    let stmts = parser.parse_program().map_err(|e| e.to_string())?;
    let stdout = cosmix_mix::evaluator::SharedBuf::new();
    let stderr = cosmix_mix::evaluator::SharedBuf::new();
    let mut eval = Evaluator::with_output(Box::new(stdout.clone()), Box::new(stderr.clone()));
    eval.execute(&stmts).await.map_err(|e| e.to_string())?;
    Ok(stdout.to_string_lossy())
}

#[tokio::test]
async fn module_fn_call_survives_a_same_named_local_variable() {
    use std::io::Write;
    let dir = std::env::temp_dir().join(format!("mix-rows-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("tmp dir");
    let lib = dir.join("lib.mix");
    let mut f = std::fs::File::create(&lib).expect("write lib");
    writeln!(f, "fn rows($x)").unwrap();
    writeln!(f, "  return 1").unwrap();
    writeln!(f, "end").unwrap();
    writeln!(f, "fn g()").unwrap();
    writeln!(f, "  $rows = []").unwrap();
    writeln!(f, "  return rows(3)").unwrap();
    writeln!(f, "end").unwrap();
    drop(f);
    let src = format!("$l = require(\"{}\")\nprint($l.g())\n", lib.display());
    let out = run(&src).await.expect("module fn call must survive the local");
    assert!(out.contains('1'), "got: {out}");
    std::fs::remove_dir_all(&dir).ok();
}
