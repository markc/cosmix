//! 09-25 entry: SHA512-CRYPT ($6$) hash + verify builtins — every NS
//! passdb (Dovecot {SHA512-CRYPT}$6$…) uses this scheme, so
//! password_hash()/password_verify() grew the sha512-crypt scheme, the
//! $6$/$5$ verify prefixes, and the Dovecot prefix.

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

/// Pinned vector: `printf 'vector-pw' | mkpasswd -m sha512crypt
/// -S testsalt -R 1000 --stdin` (shadow-utils mkpasswd, the same tool the
/// TODO entry was shelling out to).
const VECTOR: &str =
    "$6$rounds=1000$testsalt$pJiR2JZDZxtYoBdU749uokWBxsGFrMNLhFayE.be8uJClPWvdHSeiwAM9vJ0i.4hdjjgaNlHowi9/kLtLgs1k1";

#[tokio::test]
async fn sha512_crypt_hash_round_trips() {
    let out = run(
        "$h = password_hash(\"pw\", {scheme: \"sha512-crypt\", rounds: 1000})\n\
         print($h)\n\
         print(password_verify(\"pw\", $h))\n",
    )
    .await
    .expect("sha512-crypt hash");
    assert!(out.contains("$6$"), "hash must be $6$: {out}");
    assert!(out.contains("true"), "verify must match: {out}");
}

#[tokio::test]
async fn sha512_crypt_verifies_a_mkpasswd_vector() {
    // The external-vector check: a hash made by mkpasswd (the tool this
    // entry was shelling out to) must verify — this pins interop with
    // glibc/Dovecot SHA512-CRYPT.
    let src = format!(
        "print(password_verify(\"vector-pw\", \"{VECTOR}\"))\n\
         print(password_verify(\"vector-pw\", \"{{SHA512-CRYPT}}{VECTOR}\"))\n\
         print(password_verify(\"wrong\", \"{VECTOR}\"))\n"
    );
    let out = run(&src).await.expect("vector verify");
    assert!(out.contains("true"), "mkpasswd vector must verify: {out}");
    // "wrong" must be false, not an error.
    assert!(out.contains("false"), "wrong password must answer false: {out}");
}

#[tokio::test]
async fn malformed_sha_crypt_hash_raises() {
    let err = run("print(password_verify(\"pw\", \"$6$not-a-hash\"))\n")
        .await
        .expect_err("malformed $6$ hash must raise");
    assert!(err.contains("sha-crypt"), "got: {err}");
    let err = run("print(password_verify(\"pw\", \"{SHA512-CRYPT}broken\"))\n")
        .await
        .expect_err("prefix without body must raise");
    assert!(err.contains("prefix"), "got: {err}");
}
