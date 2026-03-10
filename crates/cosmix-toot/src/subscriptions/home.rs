use cosmic::iced::Subscription;
use cosmic::iced::futures::SinkExt;
use futures_util::StreamExt;
use mastodon_async::Mastodon;
use std::hash::{Hash, Hasher};

use super::BoxStream;
use crate::pages;

#[derive(Clone)]
struct HomeKey(Mastodon, usize);

impl Hash for HomeKey {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.0.data.base.to_string().hash(state);
        self.1.hash(state);
        "home-timeline".hash(state);
    }
}

pub fn user_timeline(mastodon: Mastodon, skip: usize) -> Subscription<pages::home::Message> {
    Subscription::run_with(HomeKey(mastodon, skip), build_timeline)
}

fn build_timeline(key: &HomeKey) -> BoxStream<pages::home::Message> {
    let mastodon = key.0.clone();
    let skip = key.1;
    Box::pin(cosmic::iced::stream::channel(1, move |mut output: cosmic::iced::futures::channel::mpsc::Sender<_>| async move {
        let mut stream = Box::pin(
            mastodon
                .get_home_timeline()
                .await
                .unwrap()
                .items_iter()
                .skip(skip)
                .take(20),
        );

        while let Some(status) = stream.next().await {
            if let Err(err) = output
                .send(pages::home::Message::AppendStatus(status.clone()))
                .await
            {
                tracing::warn!("failed to send post: {}", err);
            }
        }

        std::future::pending().await
    }))
}
