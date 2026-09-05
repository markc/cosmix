//! Helper utilities for using x11rb as an event source in calloop.
//!
//! The primary use for this module is XWayland integration but is also widely useful for an X11
//! backend in a compositor.

use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread::{spawn, JoinHandle},
};

use tracing::{error, warn};
use x11rb::{
    connection::Connection as _,
    protocol::{
        xproto::{Atom, ClientMessageEvent, ConnectionExt as _, EventMask, Window, CLIENT_MESSAGE_EVENT},
        Event,
    },
    rust_connection::RustConnection,
};

use calloop::{
    channel::{sync_channel, Channel, ChannelError, Event as ChannelEvent, SyncSender},
    EventSource, Poll, PostAction, Readiness, Token, TokenFactory,
};

/// Integration of an x11rb X11 connection with calloop.
///
/// This is a thin wrapper around `Channel`. It works by spawning an extra thread reads events from
/// the X11 connection and then sends them across the channel.
///
/// See [1] for why this extra thread is necessary. The single-thread solution proposed on that
/// page does not work with calloop, since it requires checking something on every main loop
/// iteration. Calloop only allows "when an FD becomes readable".
///
/// [1]: https://docs.rs/x11rb/0.8.1/x11rb/event_loop_integration/index.html#threads-and-races
#[derive(Debug)]
pub struct X11Source {
    connection: Arc<RustConnection>,
    channel: Option<Channel<Event>>,
    event_thread: Option<JoinHandle<()>>,
    close_window: Window,
    close_type: Atom,
}

impl X11Source {
    /// Create a new X11 source.
    ///
    /// The returned instance will use `SendRequest` to cause a `ClientMessageEvent` to be sent to
    /// the given window with the given type. The expectation is that this is a window that was
    /// created by us. Thus, the event reading thread will wake up and check an internal exit flag,
    /// then exit.
    pub fn new(connection: Arc<RustConnection>, close_window: Window, close_type: Atom) -> Self {
        Self::new_with_shutdown(
            connection,
            close_window,
            close_type,
            Arc::new(AtomicBool::new(false)),
        )
    }

    // Only the XWayland lifecycle supplies this generation-scoped intent.
    // Generic X11 backends keep treating unexpected disconnects as errors.
    pub(crate) fn new_with_shutdown(
        connection: Arc<RustConnection>,
        close_window: Window,
        close_type: Atom,
        shutdown_requested: Arc<AtomicBool>,
    ) -> Self {
        let (sender, channel) = sync_channel(5);
        let conn = Arc::clone(&connection);
        let event_thread = Some(spawn(move || {
            run_event_thread(conn, sender, shutdown_requested);
        }));

        Self {
            connection,
            channel: Some(channel),
            event_thread,
            close_window,
            close_type,
        }
    }
}

impl Drop for X11Source {
    fn drop(&mut self) {
        // Signal the worker thread to exit by dropping the read end of the channel.
        self.channel.take();

        // Send an event to wake up the worker so that it actually exits
        let event = ClientMessageEvent {
            response_type: CLIENT_MESSAGE_EVENT,
            format: 8,
            sequence: 0,
            window: self.close_window,
            type_: self.close_type,
            data: [0; 20].into(),
        };

        let _ = self
            .connection
            .send_event(false, self.close_window, EventMask::NO_EVENT, event);
        let _ = self.connection.flush();

        // Wait for the worker thread to exit
        self.event_thread.take().map(|handle| handle.join());
    }
}

impl EventSource for X11Source {
    type Event = ChannelEvent<Event>;
    type Metadata = ();
    type Ret = ();
    type Error = ChannelError;

    #[profiling::function]
    fn process_events<C>(
        &mut self,
        readiness: Readiness,
        token: Token,
        mut callback: C,
    ) -> Result<PostAction, ChannelError>
    where
        C: FnMut(Self::Event, &mut Self::Metadata) -> Self::Ret,
    {
        if let Some(channel) = &mut self.channel {
            channel.process_events(readiness, token, move |event, meta| {
                if matches!(event, ChannelEvent::Closed) {
                    warn!("Event thread exited");
                }
                callback(event, meta)
            })
        } else {
            Ok(PostAction::Remove)
        }
    }

    fn register(&mut self, poll: &mut Poll, factory: &mut TokenFactory) -> calloop::Result<()> {
        if let Some(channel) = &mut self.channel {
            channel.register(poll, factory)?;
        }

        Ok(())
    }

    fn reregister(&mut self, poll: &mut Poll, factory: &mut TokenFactory) -> calloop::Result<()> {
        if let Some(channel) = &mut self.channel {
            channel.reregister(poll, factory)?;
        }

        Ok(())
    }

    fn unregister(&mut self, poll: &mut Poll) -> calloop::Result<()> {
        if let Some(channel) = &mut self.channel {
            channel.unregister(poll)?;
        }

        Ok(())
    }
}

/// This thread reads X11 events from the connection and sends them on the channel.
///
/// This is run in an extra thread since sending an X11 request or waiting for the reply to an X11
/// request can both read X11 events from the underlying socket which are then saved in the
/// RustConnection. Thus, readability of the underlying socket is not enough to guarantee we do not
/// miss wakeups.
///
/// This thread will call wait_for_event(). RustConnection then ensures internally to wake us up
/// when an event arrives. So far, this seems to be the only safe way to integrate x11rb with
/// calloop.
fn run_event_thread(
    connection: Arc<RustConnection>,
    sender: SyncSender<Event>,
    shutdown_requested: Arc<AtomicBool>,
) {
    loop {
        let event = match connection.wait_for_event() {
            Ok(event) => event,
            Err(err) => {
                // Connection errors are most likely permanent. Thus, exit the thread.
                if expected_shutdown_error(&err, shutdown_requested.load(Ordering::Acquire)) {
                    tracing::debug!("X11 event thread closed during requested shutdown");
                } else {
                    error!("Event thread exiting due to connection error {}", err);
                }
                break;
            }
        };
        match sender.send(event) {
            Ok(()) => {}
            Err(_) => {
                // The only possible error is that the other end of the channel was dropped.
                // This happens in X11Source's Drop impl.
                break;
            }
        }
    }
}

fn expected_shutdown_error(error: &x11rb::errors::ConnectionError, orderly: bool) -> bool {
    orderly
        && matches!(error, x11rb::errors::ConnectionError::IoError(err)
        if matches!(err.kind(), std::io::ErrorKind::UnexpectedEof | std::io::ErrorKind::ConnectionReset))
}

#[cfg(test)]
mod shutdown_tests {
    use super::expected_shutdown_error;
    use std::io::{Error, ErrorKind};
    use x11rb::errors::ConnectionError;

    #[test]
    fn disconnect_errors_require_explicit_shutdown_intent() {
        for kind in [ErrorKind::UnexpectedEof, ErrorKind::ConnectionReset] {
            let err = ConnectionError::IoError(Error::from(kind));
            assert!(expected_shutdown_error(&err, true));
            assert!(!expected_shutdown_error(&err, false));
        }
        let err = ConnectionError::IoError(Error::from(ErrorKind::PermissionDenied));
        assert!(!expected_shutdown_error(&err, true));
        assert!(!expected_shutdown_error(&ConnectionError::UnknownError, true));
    }
}
