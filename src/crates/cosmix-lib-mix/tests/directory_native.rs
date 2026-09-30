//! Exercise retained-root recovery through the actual Mix evaluator.
#![cfg(target_os = "linux")]

use cosmix_mix::{
    MixResult,
    evaluator::{CapabilityPolicy, Evaluator},
    lexer::Lexer,
    parser::Parser,
    value::Value,
};
use std::{path::PathBuf, rc::Rc};

struct Scratch(PathBuf);
impl Scratch {
    fn new(tag: &str) -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let path = std::env::temp_dir().join(format!(
            "cosmix-dir-api-{tag}-{}-{}", std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
    fn quoted(&self) -> String { serde_json::to_string(self.0.to_str().unwrap()).unwrap() }
}
impl Drop for Scratch {
    fn drop(&mut self) { let _ = std::fs::remove_dir_all(&self.0); }
}

async fn exec(eval: &mut Evaluator, source: &str) -> MixResult<Value> {
    let tokens = Lexer::new(source).tokenize()?;
    let statements = Parser::new(tokens, source).parse_program()?;
    eval.execute(&statements).await
}

#[tokio::test(flavor = "current_thread")]
async fn recovery_moves_and_no_replace_use_the_real_builtin_path() {
    let root = Scratch::new("move");
    std::fs::write(root.0.join("scene"), b"original").unwrap();
    std::fs::create_dir(root.0.join("recovery")).unwrap();
    std::fs::write(root.0.join("existing"), b"sentinel").unwrap();
    let mut eval = Evaluator::new();
    exec(&mut eval, &format!("$h = dir_open({})", root.quoted())).await.unwrap();
    exec(&mut eval, "dir_rename($h, \"scene\", \"recovery/backup\")").await.unwrap();
    assert_eq!(std::fs::read(root.0.join("recovery/backup")).unwrap(), b"original");
    assert!(!root.0.join("scene").exists());
    let error = exec(&mut eval, "dir_rename($h, \"recovery/backup\", \"existing\")").await.unwrap_err();
    assert_eq!(error.info().unwrap().code, "DIR_RENAME_FAILED");
    assert_eq!(std::fs::read(root.0.join("existing")).unwrap(), b"sentinel");
    exec(&mut eval, "dir_close($h)").await.unwrap();
    let error = exec(&mut eval, "dir_rename($h, \"recovery/backup\", \"other\")").await.unwrap_err();
    assert_eq!(error.info().unwrap().code, "DIR_INVALID_HANDLE");
}

struct RefuseWrites;
impl CapabilityPolicy for RefuseWrites {
    fn check_builtin(&self, name: &str) -> Result<(), String> {
        if name == "dir_rename" { Err("writes refused".into()) } else { Ok(()) }
    }
}

#[tokio::test(flavor = "current_thread")]
async fn capability_and_operand_refusals_happen_before_changes() {
    let root = Scratch::new("refuse");
    std::fs::write(root.0.join("scene"), b"original").unwrap();
    let mut eval = Evaluator::new();
    exec(&mut eval, &format!("$h = dir_open({})", root.quoted())).await.unwrap();
    let error = exec(&mut eval, "dir_rename($h, \"scene\", \"../escape\")").await.unwrap_err();
    assert_eq!(error.info().unwrap().code, "DIR_INVALID_PATH");
    eval.set_capability_policy(Rc::new(RefuseWrites));
    let error = exec(&mut eval, "dir_rename($h, \"scene\", \"other\")").await.unwrap_err();
    assert!(error.to_string().contains("writes refused"));
    assert_eq!(std::fs::read(root.0.join("scene")).unwrap(), b"original");
    assert!(!root.0.join("other").exists());
}

#[tokio::test(flavor = "current_thread")]
async fn evaluator_retirement_closes_roots_even_with_exported_module_functions() {
    let root = Scratch::new("retirement");
    let count = || std::fs::read_dir("/proc/self/fd").unwrap()
        .filter_map(Result::ok)
        .filter(|entry| std::fs::read_link(entry.path()).is_ok_and(|path| path == root.0))
        .count();
    let module = root.0.join("roots.mix");
    std::fs::write(&module, format!(
        "$root = dir_open({})\nfn handle() = $root\nreturn {{handle:handle}}\n", root.quoted()
    )).unwrap();
    let mut eval = Evaluator::new();
    let escaped = exec(&mut eval, &format!("require({})", serde_json::to_string(module.to_str().unwrap()).unwrap())).await.unwrap();
    assert_eq!(count(), 1, "one retained native directory descriptor");
    drop(eval);
    assert_eq!(count(), 0, "retirement must close the native descriptor");
    drop(escaped);
}
