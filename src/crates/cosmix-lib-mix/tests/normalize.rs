//! `normalize(s[, form])` — Unicode normalisation (0.92.0).
//!
//! Before it, canonically-equivalent text compared unequal: a decomposed
//! `e` + COMBINING ACUTE (what macOS filenames and many web forms carry)
//! and the precomposed `é` are two codepoints vs one, so `==`, hashes and
//! dedup keys disagreed with no builtin to reconcile them. Codepoints are
//! built with `chr()` so the test source itself is not subject to any
//! editor's normalisation.

use cosmix_mix::evaluator::{Evaluator, SharedBuf};
use cosmix_mix::lexer::Lexer;
use cosmix_mix::parser::Parser;

async fn run(source: &str) -> Result<String, String> {
    let mut lexer = Lexer::new(source);
    let tokens = lexer.tokenize().map_err(|e| e.to_string())?;
    let mut parser = Parser::new(tokens, source);
    let stmts = parser.parse_program().map_err(|e| e.to_string())?;
    let stdout = SharedBuf::new();
    let stderr = SharedBuf::new();
    let mut eval = Evaluator::with_output(Box::new(stdout.clone()), Box::new(stderr.clone()));
    eval.execute(&stmts).await.map_err(|e| e.to_string())?;
    Ok(stdout.to_string_lossy())
}

#[tokio::test]
async fn canonical_equivalents_compare_equal_after_normalize() {
    let out = run("$d = \"e\" .. chr(769)\n$c = chr(233)\n\
                   print($d == $c)\n\
                   print(normalize($d) == $c)\n\
                   print(normalize($d, \"NFC\") == $c)\n\
                   print(normalize($c, \"NFD\") == $d)\n\
                   print(length(normalize($c, \"nfd\")))\n")
        .await
        .unwrap();
    assert_eq!(out, "false\ntrue\ntrue\ntrue\n2\n");
}

#[tokio::test]
async fn nfkc_folds_compatibility_forms_and_nfc_does_not() {
    // U+FB01 LATIN SMALL LIGATURE FI; U+FF46/U+FF49 FULLWIDTH f / i.
    let out = run("$lig = chr(64257)\n$wide = chr(65350) .. chr(65353)\n\
                   print(normalize($lig, \"NFKC\"))\n\
                   print(normalize($wide, \"NFKC\"))\n\
                   print(normalize($wide, \"NFKD\"))\n\
                   print(normalize($lig) == $lig)\n")
        .await
        .unwrap();
    assert_eq!(out, "fi\nfi\nfi\ntrue\n");
}

#[tokio::test]
async fn emoji_and_zwj_sequences_pass_through_every_form() {
    // A ZWJ family, a skin-tone modifier, and a flag.
    let out = run("$fam = chr(128104) .. chr(8205) .. chr(128105) .. chr(8205) .. chr(128103)\n\
                   $thumb = chr(128077) .. chr(127997)\n\
                   $flag = chr(127462) .. chr(127482)\n\
                   $all = $fam .. $thumb .. $flag\n\
                   for each $f in [\"NFC\", \"NFD\", \"NFKC\", \"NFKD\"]\n\
                     print(normalize($all, $f) == $all)\n\
                   end\n")
        .await
        .unwrap();
    assert_eq!(out, "true\ntrue\ntrue\ntrue\n");
}

#[tokio::test]
async fn unknown_or_non_string_form_raises() {
    let e = run("normalize(\"x\", \"NFX\")\n").await.unwrap_err();
    assert!(e.contains("VALUE_ERROR") || e.contains("unknown form"), "{e}");
    let e = run("normalize(\"x\", 1)\n").await.unwrap_err();
    assert!(e.contains("TYPE_MISMATCH") || e.contains("must be a string"), "{e}");
    let e = run("normalize()\n").await.unwrap_err();
    assert!(e.contains("normalize"), "{e}");
}
