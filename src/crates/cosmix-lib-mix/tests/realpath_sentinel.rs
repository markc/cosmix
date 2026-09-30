//! A6 (TODO-mix 2026-09-24): `realpath` now returns nil ONLY for absence —
//! a permission/IO/symlink-loop/non-UTF-8 failure raises instead of
//! collapsing to nil (which misread a denied or broken path as "missing").

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
async fn missing_path_is_nil_but_a_symlink_loop_raises() {
    let dir = std::env::temp_dir().join(format!("mix-realpath-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("tmp dir");
    let missing = dir.join("nope");
    // Absence -> nil.
    let out = run(&format!("print(type(realpath(\"{}\")))", missing.display()))
        .await
        .expect("missing path");
    assert_eq!(out.trim(), "nil", "got: {out}");
    // Existing -> the canonical string.
    let file = dir.join("f");
    std::fs::write(&file, "x").expect("write file");
    let out = run(&format!("print(realpath(\"{}\"))", file.display()))
        .await
        .expect("existing path");
    assert_eq!(out.trim(), std::fs::canonicalize(&file).unwrap().to_string_lossy());
    // A self-referential symlink loop -> raise (FilesystemLoop, not nil).
    #[cfg(unix)]
    {
        let loop_link = dir.join("loop");
        std::os::unix::fs::symlink(&loop_link, &loop_link).expect("symlink loop");
        let err = run(&format!("print(realpath(\"{}\"))", loop_link.display()))
            .await
            .expect_err("symlink loop must raise");
        assert!(err.contains("cannot resolve"), "got: {err}");
    }
    std::fs::remove_dir_all(&dir).ok();
}
