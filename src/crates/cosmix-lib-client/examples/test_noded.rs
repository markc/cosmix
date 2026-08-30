use cosmix_client::NodedClient;
use tokio::time::{Duration, sleep};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Connect two clients
    let client_a = NodedClient::connect("service-a", "ws://localhost:4200/ws").await?;
    println!("[OK] service-a connected");

    let client_b = NodedClient::connect("service-b", "ws://localhost:4200/ws").await?;
    println!("[OK] service-b connected");

    sleep(Duration::from_millis(200)).await;

    // List services
    let services = client_a.list_services().await?;
    println!("[OK] services on broker: {:?}", services);

    // Send from A to B
    client_a
        .send(
            "service-b",
            "greet",
            serde_json::json!({"msg": "hello from A"}),
        )
        .await?;
    println!("[OK] A sent 'greet' to B");

    sleep(Duration::from_millis(200)).await;

    // Check B received it
    let mut rx = client_b.incoming_async().await.expect("incoming channel");
    match rx.try_recv() {
        Ok(cmd) => {
            println!(
                "[OK] B received: command='{}' from='{}' args={}",
                cmd.command, cmd.from, cmd.args
            );
            assert_eq!(cmd.command, "greet");
            assert_eq!(cmd.from, "service-a");
        }
        Err(_) => {
            println!("[FAIL] B did not receive the message");
            std::process::exit(1);
        }
    }

    // Test call (request/response) via noded.ping
    let pong = client_a
        .call("noded", "noded.ping", serde_json::Value::Null)
        .await?;
    println!("[OK] noded.ping response: {}", pong);

    println!("\n=== All noded integration tests passed ===");
    Ok(())
}
