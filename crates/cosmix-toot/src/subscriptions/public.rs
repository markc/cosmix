use cosmic::iced::Subscription;
use cosmic::iced::futures::SinkExt;
use mastodon_async::Mastodon;
use std::hash::{Hash, Hasher};

use super::BoxStream;
use crate::pages;

#[derive(Clone)]
struct PublicKey(Mastodon, &'static str);

impl Hash for PublicKey {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.0.data.base.to_string().hash(state);
        self.1.hash(state);
    }
}

pub fn timeline(mastodon: Mastodon) -> Subscription<pages::public::Message> {
    Subscription::run_with(PublicKey(mastodon, "public"), build_public)
}

fn build_public(key: &PublicKey) -> BoxStream<pages::public::Message> {
    let mastodon = key.0.clone();
    Box::pin(cosmic::iced::stream::channel(1, move |mut output: cosmic::iced::futures::channel::mpsc::Sender<_>| async move {
        match mastodon.get_public_timeline(false, false).await {
            Ok(statuses) => {
                for status in statuses {
                    if let Err(err) = output
                        .send(pages::public::Message::AppendStatus(status.clone()))
                        .await
                    {
                        tracing::warn!("failed to send post: {}", err);
                    }
                }
            }
            Err(err) => {
                tracing::warn!("failed to get public timeline: {}", err);
            }
        }

        std::future::pending().await
    }))
}

pub fn local_timeline(mastodon: Mastodon) -> Subscription<pages::public::Message> {
    Subscription::run_with(PublicKey(mastodon, "local"), build_local)
}

fn build_local(key: &PublicKey) -> BoxStream<pages::public::Message> {
    let mastodon = key.0.clone();
    Box::pin(cosmic::iced::stream::channel(1, move |mut output: cosmic::iced::futures::channel::mpsc::Sender<_>| async move {
        match mastodon.get_public_timeline(true, false).await {
            Ok(statuses) => {
                for status in statuses {
                    if let Err(err) = output
                        .send(pages::public::Message::AppendStatus(status.clone()))
                        .await
                    {
                        tracing::warn!("failed to send post: {}", err);
                    }
                }
            }
            Err(err) => {
                tracing::warn!("failed to get local timeline: {}", err);
            }
        }

        std::future::pending().await
    }))
}

pub fn remote_timeline(mastodon: Mastodon) -> Subscription<pages::public::Message> {
    Subscription::run_with(PublicKey(mastodon, "remote"), build_remote)
}

fn build_remote(key: &PublicKey) -> BoxStream<pages::public::Message> {
    let mastodon = key.0.clone();
    Box::pin(cosmic::iced::stream::channel(1, move |mut output: cosmic::iced::futures::channel::mpsc::Sender<_>| async move {
        match mastodon.get_public_timeline(false, true).await {
            Ok(statuses) => {
                for status in statuses {
                    if let Err(err) = output
                        .send(pages::public::Message::AppendStatus(status.clone()))
                        .await
                    {
                        tracing::warn!("failed to send post: {}", err);
                    }
                }
            }
            Err(err) => {
                tracing::warn!("failed to get remote timeline: {}", err);
            }
        }

        std::future::pending().await
    }))
}
