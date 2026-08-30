//! HTTP private-CA options (0.29.0, decision record D8): `ca_file` /
//! `ca_pem` add certificates to the default webpki roots. These tests
//! cover the option-validation and PEM-loading paths without touching
//! the network (the one dispatch test targets 127.0.0.1:1, which
//! refuses instantly). Live-handshake verification against a private-CA
//! endpoint happens in the provisioning acceptance run.

#![cfg(feature = "http")]

use cosmix_mix::error::MixError;
use cosmix_mix::evaluator::{Evaluator, SharedBuf};
use cosmix_mix::lexer::Lexer;
use cosmix_mix::parser::Parser;

/// Throwaway self-signed test fixture (CN=test-fixture.example.com,
/// generated for this suite; carries no trust anywhere).
const TEST_CA_PEM: &str = "-----BEGIN CERTIFICATE-----
MIIBmzCCAUGgAwIBAgIUedC/tz8CgKu2bowmQu0Ebz3P8nEwCgYIKoZIzj0EAwIw
IzEhMB8GA1UEAwwYdGVzdC1maXh0dXJlLmV4YW1wbGUuY29tMB4XDTI2MDcxMDE0
MTI0NVoXDTM2MDcwNzE0MTI0NVowIzEhMB8GA1UEAwwYdGVzdC1maXh0dXJlLmV4
YW1wbGUuY29tMFkwEwYHKoZIzj0CAQYIKoZIzj0DAQcDQgAEqfCfMIhN9ObSx1zg
MlomzL74PmfStj8yQ317+TKShdPaAtUeHR/DG1jMsVRa1Ya4aeT4RjfodBaEYZex
Lvan5qNTMFEwHQYDVR0OBBYEFPVAcucPHuvgHtZzxSnCIdqyVpbqMB8GA1UdIwQY
MBaAFPVAcucPHuvgHtZzxSnCIdqyVpbqMA8GA1UdEwEB/wQFMAMBAf8wCgYIKoZI
zj0EAwIDSAAwRQIhANXE4lltti1CljlkupACXwwvko4oFVg17KtiT3iKozaGAiAN
Q4BsjjYvwAjsySE6HQaIaQy7RQf8oZKUdpPW8H80yA==
-----END CERTIFICATE-----
";

async fn run(source: &str) -> Result<String, MixError> {
    let mut lexer = Lexer::new(source);
    let tokens = lexer.tokenize()?;
    let mut parser = Parser::new(tokens, source);
    let stmts = parser.parse_program()?;
    let stdout = SharedBuf::new();
    let stderr = SharedBuf::new();
    let mut eval = Evaluator::with_output(Box::new(stdout.clone()), Box::new(stderr.clone()));
    eval.execute(&stmts).await?;
    Ok(stdout.to_string_lossy())
}

async fn run_ok(source: &str) -> String {
    match run(source).await {
        Ok(out) => out,
        Err(e) => panic!("script should succeed, got: {e}"),
    }
}

#[tokio::test]
async fn bad_ca_inputs_raise_http_tls() {
    for (opts, needle) in [
        ("{ca_file: \"/no/such/ca.pem\"}", "ca_file /no/such/ca.pem"),
        ("{ca_pem: \"this is not pem\"}", "no certificates found"),
        ("{ca_pem: \"\"}", "empty"),
    ] {
        let src = format!(
            "try\n  http_get(\"https://192.0.2.1/x\", {opts})\ncatch $m, $e\n  print($e.code .. \": \" .. $m)\nend\n"
        );
        let out = run_ok(&src).await;
        assert!(
            out.starts_with("HTTP_TLS:") && out.contains(needle),
            "{opts} -> {out}"
        );
    }
}

#[tokio::test]
async fn exclusivity_rules_raise_option_invalid() {
    for opts in [
        "{ca_file: \"/a.pem\", ca_pem: \"x\"}",
        "{ssl_verify: false, ca_pem: \"x\"}",
        "{ssl_verify: false, ca_file: \"/a.pem\"}",
    ] {
        let src = format!(
            "try\n  http_get(\"https://192.0.2.1/x\", {opts})\ncatch $m, $e\n  print($e.code)\nend\n"
        );
        assert_eq!(run_ok(&src).await, "OPTION_INVALID\n", "{opts}");
    }
}

#[tokio::test]
async fn valid_pem_builds_agent_and_dispatches() {
    // A syntactically valid CA loads; the request then fails at the
    // TCP layer (127.0.0.1:1 refuses) and surfaces as the normal
    // {status: 0, error} result — proving the whole option path wires
    // into dispatch without touching external networks.
    let src = format!(
        "$pem = \"{}\"\n$r = http_get(\"https://127.0.0.1:1/x\", {{ca_pem: $pem, timeout: 2}})\nprint($r.status)\nprint(length($r.error) > 0)\n",
        TEST_CA_PEM.replace('\n', "\\n")
    );
    let out = run_ok(&src).await;
    assert_eq!(out, "0\ntrue\n");
}

#[tokio::test]
async fn ca_file_variant_loads_from_disk() {
    let dir = std::env::temp_dir().join(format!("mix-httpca-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("ca.pem");
    std::fs::write(&path, TEST_CA_PEM).unwrap();
    let src = format!(
        "$r = http_get(\"https://127.0.0.1:1/x\", {{ca_file: \"{}\", timeout: 2}})\nprint($r.status)\n",
        path.display()
    );
    let out = run_ok(&src).await;
    std::fs::remove_dir_all(&dir).ok();
    assert_eq!(out, "0\n");
}

#[tokio::test]
async fn ca_file_read_is_byte_capped_on_special_files() {
    // codex release review BLOCKER: /dev/zero reports length 0 but
    // never reaches EOF. The take(cap+1) read must reject it, not hang.
    #[cfg(unix)]
    {
        let out = run_ok(
            "try\n  http_get(\"https://127.0.0.1:1/x\", {ca_file: \"/dev/zero\", timeout: 2})\ncatch $m, $e\n  print($e.code .. \" \" .. (pos(\"exceeds\", $m) > 0))\nend\n",
        )
        .await;
        assert_eq!(out, "HTTP_TLS true\n");
    }
}

#[tokio::test]
async fn conditional_cap_engagement_ignores_non_opts_maps() {
    // codex release review MINOR: {ca_file: ...} in the HEADERS slot is
    // not the opts map, so it must NOT engage fs-read.
    use cosmix_mix::value::Value;
    // header slot (2nd arg is headers because a later opts arg exists,
    // OR the map has a non-opt key): not engaged.
    let header_map = {
        let mut m = cosmix_mix::IndexMap::new();
        m.insert(
            "ca_file".to_string(),
            Value::String("literal-header".into()),
        );
        m.insert("X-Custom".to_string(), Value::String("y".into()));
        Value::map(m)
    };
    let args = vec![Value::String("https://x/".into()), header_map];
    assert!(
        !cosmix_mix::builtins::conditional_cap_engaged("http_get", &args, "ca_file"),
        "a header map with ca_file must not engage fs-read"
    );
    // real opts map (all opt keys): engaged.
    let opts_map = {
        let mut m = cosmix_mix::IndexMap::new();
        m.insert("ca_file".to_string(), Value::String("/path".into()));
        Value::map(m)
    };
    let args = vec![Value::String("https://x/".into()), opts_map];
    assert!(
        cosmix_mix::builtins::conditional_cap_engaged("http_get", &args, "ca_file"),
        "an all-opt-keys map with ca_file IS the opts map and must engage fs-read"
    );
}

#[tokio::test]
async fn http_v2_additive_fields_on_transport_error() {
    // The v2 response fields are present on EVERY path, including a
    // refused connection: a typed error_code (a refused TCP connect is
    // HTTP_CONNECT via ureq's ErrorKind, not a display-text guess),
    // duration_ms, and the legacy status/error stay intact.
    let out = run_ok(
        "$r = http_get(\"https://127.0.0.1:1/\", {timeout: 2})\nprint($r.status .. \" \" .. $r.error_code .. \" \" .. has_key($r, \"duration_ms\"))\n",
    )
    .await;
    assert_eq!(out, "0 HTTP_CONNECT true\n");
}

#[tokio::test]
async fn metadata_declares_conditional_capability() {
    let cc = cosmix_mix::builtins::conditional_capabilities("http_get");
    assert_eq!(cc.len(), 1);
    assert_eq!(cc[0].option, "ca_file");
    assert!(matches!(
        cc[0].capability,
        cosmix_mix::builtins::CapabilityClass::FsRead
    ));
    assert!(cosmix_mix::builtins::conditional_capabilities("length").is_empty());
}
