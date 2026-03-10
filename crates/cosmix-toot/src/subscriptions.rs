use crate::pages;
use cosmic::iced::Subscription;
use cosmic::iced::futures::SinkExt;
use cosmic::iced::futures::Stream;
use futures_util::TryStreamExt;
use mastodon_async::entities::event::Event;
use mastodon_async::Mastodon;
use std::hash::{Hash, Hasher};
use std::pin::Pin;

use crate::app;

pub mod home;
pub mod notifications;
pub mod public;

/// Hashable wrapper around Mastodon client (hashes on base URL + token presence)
#[derive(Clone)]
pub struct HashableMastodon(pub Mastodon);

impl Hash for HashableMastodon {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.0.data.base.to_string().hash(state);
        self.0.data.token.is_empty().hash(state);
    }
}

pub type BoxStream<T> = Pin<Box<dyn Stream<Item = T> + Send>>;

pub fn stream_user_events(mastodon: Mastodon) -> Subscription<app::Message> {
    Subscription::run_with(HashableMastodon(mastodon), build_user_stream)
}

fn build_user_stream(hm: &HashableMastodon) -> BoxStream<app::Message> {
    let mastodon = hm.0.clone();
    Box::pin(cosmic::iced::stream::channel(1, |output: cosmic::iced::futures::channel::mpsc::Sender<_>| async move {
        let stream = mastodon.stream_user().await.unwrap();
        stream
            .try_for_each(|(event, _client)| {
                let mut output = output.clone();
                async move {
                    match event {
                        Event::Update(ref status) => {
                            if let Err(err) = output
                                .send(app::Message::Home(pages::home::Message::PrependStatus(
                                    status.clone(),
                                )))
                                .await
                            {
                                tracing::warn!("failed to send post: {}", err);
                            }
                        }
                        Event::Notification(ref notification) => {
                            if let Err(err) = output
                                .send(app::Message::Notifications(
                                    pages::notifications::Message::PrependNotification(
                                        notification.clone(),
                                    ),
                                ))
                                .await
                            {
                                tracing::warn!("failed to send post: {}", err);
                            }
                        }
                        Event::Delete(ref id) => {
                            if let Err(err) = output
                                .send(app::Message::Home(pages::home::Message::DeleteStatus(
                                    id.clone(),
                                )))
                                .await
                            {
                                tracing::warn!("failed to send post: {}", err);
                            }
                        }
                        Event::FiltersChanged => (),
                    };
                    Ok(())
                }
            })
            .await
            .unwrap();

        std::future::pending().await
    }))
}
