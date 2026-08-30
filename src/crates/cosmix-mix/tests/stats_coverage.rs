use std::path::PathBuf;
use std::process::Command;

fn fixture(label: &str) -> PathBuf {
    std::env::temp_dir().join(format!("mix-stats-coverage-{}-{label}", std::process::id()))
}

#[test]
fn coverage_ignores_strings_and_comments_and_fails_closed() {
    let dir = fixture("fixture");
    let state = fixture("state");
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&state);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("good.mix"),
        "-- len([1])\n$x = \"map($f, $xs)\"\nprint(length([1]))\n",
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_mix"))
        .env("XDG_STATE_HOME", &state)
        .args(["stats", "coverage"])
        .arg(&dir)
        .output()
        .unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("length"));
    assert!(stdout.contains("Never-authored builtins:"));
    assert!(!state.join("mix/current.json").exists());

    std::fs::write(dir.join("bad.mix"), "if true then\n").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_mix"))
        .env("XDG_STATE_HOME", &state)
        .args(["stats", "coverage"])
        .arg(&dir)
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(!String::from_utf8_lossy(&output.stdout).contains("Never-authored"));
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&state);
}

#[cfg(unix)]
#[test]
fn coverage_follows_the_user_named_root_symlink_only() {
    use std::os::unix::fs::symlink;

    let real = fixture("symlink-real");
    let link = fixture("symlink-root");
    let state = fixture("symlink-state");
    let _ = std::fs::remove_dir_all(&real);
    let _ = std::fs::remove_file(&link);
    let _ = std::fs::remove_dir_all(&state);
    std::fs::create_dir_all(&real).unwrap();
    std::fs::write(real.join("used.mix"), "print(length([1]))\n").unwrap();
    symlink(&real, &link).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_mix"))
        .env("XDG_STATE_HOME", &state)
        .args(["stats", "coverage"])
        .arg(&link)
        .output()
        .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("1 .mix file(s)"));
    assert!(stdout.contains("length"));
    let _ = std::fs::remove_file(&link);
    let _ = std::fs::remove_dir_all(&real);
    let _ = std::fs::remove_dir_all(&state);
}
