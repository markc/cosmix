//! P5 (TODO-mix 2026-09-24): `project_info()` — read-only workspace
//! introspection over a synthetic fixture.

use std::process::Command;

#[test]
fn project_info_reports_members_excludes_and_toolchain() {
    let root = std::env::temp_dir().join(format!("mix-project-info-{}", std::process::id()));
    let src = root.join("src");
    for c in ["crates/alpha", "crates/beta", "crates/foreman"] {
        std::fs::create_dir_all(src.join(c)).expect("crate dir");
        let name = c.rsplit('/').next().unwrap();
        std::fs::write(
            src.join(c).join("Cargo.toml"),
            format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\n"),
        )
        .expect("write crate manifest");
    }
    std::fs::write(
        src.join("Cargo.toml"),
        "[workspace]\nresolver = \"2\"\nmembers = [\"crates/*\"]\nexclude = [\"crates/foreman\"]\n",
    )
    .expect("write workspace manifest");
    std::fs::write(
        src.join("rust-toolchain.toml"),
        "[toolchain]\nchannel = \"1.85.0\"\n",
    )
    .expect("write toolchain");

    let out = Command::new(env!("CARGO_BIN_EXE_mix"))
        .args(["-c", "print(json_encode(project_info()))"])
        .env("COSMIX", &root)
        .env("MIX_STATS", "off")
        .output()
        .expect("run mix");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));

    let json: serde_json::Value = serde_json::from_str(stdout.trim()).expect("valid JSON");
    let members = json["main"]["members"].as_array().expect("members array");
    let names: Vec<&str> = members.iter().map(|v| v.as_str().unwrap()).collect();
    assert_eq!(names, vec!["alpha", "beta"], "got: {stdout}");
    assert_eq!(json["main"]["toolchain"].as_str().unwrap(), "1.85.0");
    assert_eq!(json["main"]["exclude"][0].as_str().unwrap(), "crates/foreman");
    assert_eq!(json["desktop"].as_bool().unwrap(), false);

    std::fs::remove_dir_all(&root).ok();
}
