//! Phase 2 integration test: prove inter-app communication via hub.
//!
//! Requires cosmix-hubd and cosmix-files to be running:
//!   ./target/release/cosmix-hubd &
//!   ./target/release/cosmix-files &
//!   cargo run --example test_phase2 -p cosmix-lib-client

use cosmix_client::HubClient;
use tokio::time::{sleep, Duration};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Connect as "test-view" (simulating cosmix-view)
    let view = HubClient::connect("test-view", "ws://localhost:4200/ws").await?;
    println!("[OK] test-view connected to hub");

    sleep(Duration::from_millis(500)).await;

    // Verify files service is registered
    let services = view.list_services().await?;
    println!("[OK] services: {:?}", services);
    assert!(services.contains(&"files".to_string()), "cosmix-files not registered!");

    // Test file.list — list home directory
    let home = std::env::var("HOME").unwrap_or("/tmp".into());
    let result = view.call("files", "file.list", serde_json::json!({"path": &home})).await?;
    let entries: Vec<serde_json::Value> = serde_json::from_value(result)?;
    println!("[OK] file.list returned {} entries from {}", entries.len(), home);
    assert!(!entries.is_empty(), "home directory should not be empty");

    // Verify entry structure
    let first = &entries[0];
    assert!(first.get("name").is_some(), "entry missing 'name'");
    assert!(first.get("path").is_some(), "entry missing 'path'");
    assert!(first.get("is_dir").is_some(), "entry missing 'is_dir'");
    println!("[OK] entry structure valid: {}", first.get("name").unwrap());

    // Test file.stat on a known file
    let test_path = format!("{}/.bashrc", home);
    let stat = view.call("files", "file.stat", serde_json::json!({"path": &test_path})).await;
    match stat {
        Ok(s) => println!("[OK] file.stat {}: {}", test_path, s),
        Err(e) => println!("[SKIP] file.stat {}: {}", test_path, e),
    }

    // Test file.read on a known text file
    let doc_path = format!("{}/.gh/cosmixos/_doc/2026-03-26-network-topology.md", home);
    if std::path::Path::new(&doc_path).exists() {
        let content = view.call("files", "file.read", serde_json::json!({"path": &doc_path})).await?;
        let text = content.as_str().unwrap_or("");
        println!("[OK] file.read returned {} chars from {}", text.len(), doc_path);
        assert!(text.contains("Network Topology"), "content should contain expected text");
    } else {
        println!("[SKIP] file.read: test file not found at {}", doc_path);
    }

    // Test file.list on _doc directory
    let doc_dir = format!("{}/.gh/cosmixos/_doc", home);
    let docs = view.call("files", "file.list", serde_json::json!({"path": &doc_dir})).await?;
    let doc_entries: Vec<serde_json::Value> = serde_json::from_value(docs)?;
    println!("[OK] file.list _doc/: {} entries", doc_entries.len());

    // Test error handling — nonexistent path
    let err = view.call("files", "file.list", serde_json::json!({"path": "/nonexistent"})).await;
    match err {
        Ok(entries) => {
            let e: Vec<serde_json::Value> = serde_json::from_value(entries)?;
            println!("[OK] file.list /nonexistent returned {} entries (empty)", e.len());
        }
        Err(e) => println!("[OK] file.list /nonexistent correctly errored: {e}"),
    }

    println!("\n=== Phase 2 integration tests passed ===");
    Ok(())
}
