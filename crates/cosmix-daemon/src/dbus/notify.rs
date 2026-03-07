use anyhow::Result;
use zbus::Connection;

/// Send a desktop notification via org.freedesktop.Notifications
pub async fn send_notification(summary: &str, body: &str) -> Result<u32> {
    let conn = Connection::session().await?;
    let reply = conn
        .call_method(
            Some("org.freedesktop.Notifications"),
            "/org/freedesktop/Notifications",
            Some("org.freedesktop.Notifications"),
            "Notify",
            &(
                "cosmix",           // app_name
                0u32,               // replaces_id
                "dialog-information", // app_icon
                summary,            // summary
                body,               // body
                &[] as &[&str],     // actions
                &std::collections::HashMap::<&str, zbus::zvariant::Value>::new(), // hints
                5000i32,            // expire_timeout (ms)
            ),
        )
        .await?;
    let id: u32 = reply.body().deserialize()?;
    Ok(id)
}

/// CLI command: send a notification
pub fn notify_cmd(summary: &str, body: &str) -> Result<()> {
    let rt = tokio::runtime::Runtime::new()?;
    let id = rt.block_on(send_notification(summary, body))?;
    println!("Notification sent (id={id})");
    Ok(())
}
