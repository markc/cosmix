//! 09-24 entry: a required-module fn that assigns a local `$rows` and
//! then calls the module's own `rows()` raised FUNCTION_UNDEFINED — the
//! frame-injected sibling function was shadowed by the same-named local
//! variable. A non-Function variable must not shadow a module function.

use std::io::Write;
use std::process::Command;

fn mix(args: &[&str]) -> (i32, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_mix"))
        .args(args)
        .env("MIX_STATS", "off")
        .env_remove("MIXRC")
        .output()
        .expect("run mix");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
    )
}

#[test]
fn module_fn_call_survives_a_same_named_local_variable() {
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
    let main = dir.join("main.mix");
    let mut f = std::fs::File::create(&main).expect("write main");
    writeln!(f, "$l = require(\"{}\")", lib.display()).unwrap();
    writeln!(f, "print($l.g())").unwrap();
    drop(f);
    let (code, out) = mix(&[main.to_str().unwrap()]);
    assert_eq!(code, 0, "module fn call must survive the local: {out}");
    assert!(out.contains('1'), "got: {out}");
    std::fs::remove_dir_all(&dir).ok();
}
