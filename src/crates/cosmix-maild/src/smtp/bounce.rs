//! Bounce (NDR) generation for permanently failed deliveries.

use anyhow::Result;
use chrono::Utc;

/// Generate a Non-Delivery Report (bounce) message.
pub fn generate_ndr(
    hostname: &str,
    original_from: &str,
    failed_recipients: &[String],
    error: &str,
) -> Result<Vec<u8>> {
    let now = Utc::now().to_rfc2822();
    let message_id = format!("<bounce-{}@{hostname}>", uuid::Uuid::new_v4());
    let recipients_list = failed_recipients.join(", ");

    let body = format!(
        "This is the mail system at {hostname}.\r\n\
         \r\n\
         I'm sorry to inform you that your message could not be delivered to\r\n\
         one or more recipients:\r\n\
         \r\n\
         {recipients_list}\r\n\
         \r\n\
         Error: {error}\r\n\
         \r\n\
         No further delivery attempts will be made.\r\n"
    );

    let msg = format!(
        "From: Mail Delivery System <postmaster@{hostname}>\r\n\
         To: <{original_from}>\r\n\
         Subject: Undelivered Mail Returned to Sender\r\n\
         Date: {now}\r\n\
         Message-ID: {message_id}\r\n\
         MIME-Version: 1.0\r\n\
         Content-Type: text/plain; charset=utf-8\r\n\
         Auto-Submitted: auto-replied\r\n\
         \r\n\
         {body}"
    );

    Ok(msg.into_bytes())
}
