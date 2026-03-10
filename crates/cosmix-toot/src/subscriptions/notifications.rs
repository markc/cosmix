use cosmic::iced::Subscription;
use cosmic::iced::futures::SinkExt;
use futures_util::StreamExt;
use mastodon_async::Mastodon;

use super::{BoxStream, HashableMastodon};
use crate::pages;

pub fn timeline(mastodon: Mastodon) -> Subscription<pages::notifications::Message> {
    Subscription::run_with(HashableMastodon(mastodon), build_notifications)
}

fn build_notifications(hm: &HashableMastodon) -> BoxStream<pages::notifications::Message> {
    let mastodon = hm.0.clone();
    Box::pin(cosmic::iced::stream::channel(1, |mut output: cosmic::iced::futures::channel::mpsc::Sender<_>| async move {
        let mut stream = Box::pin(
            mastodon
                .notifications()
                .await
                .unwrap()
                .items_iter()
                .take(100),
        );

        while let Some(notification) = stream.next().await {
            if let Err(err) = output
                .send(pages::notifications::Message::AppendNotification(
                    notification.clone(),
                ))
                .await
            {
                tracing::warn!("failed to send post: {}", err);
            }
        }

        std::future::pending().await
    }))
}
