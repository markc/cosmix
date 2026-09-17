//! Clipboard and primary selection, text only. Pipes are non-blocking and
//! driven by the event loop in both directions.
//!
//! Text types: `text/plain;charset=utf-8`, `UTF8_STRING` and `text/plain`
//! are UTF-8. `STRING` is ISO 8859-1 (characters outside it are written as
//! `?`). `TEXT` has no fixed encoding; it is written as UTF-8 and read as
//! UTF-8, falling back to ISO 8859-1, as is `text/plain`.

use crate::event::{Event, ReadStatus, Selection};
use crate::runtime::{OwnedSource, Runtime, State};
use calloop::generic::Generic;
use calloop::timer::{TimeoutAction, Timer};
use calloop::{Interest, Mode};
use calloop::{PostAction, RegistrationToken};
use smithay_client_toolkit::data_device_manager::data_device::DataDeviceHandler;
use smithay_client_toolkit::data_device_manager::data_offer::{DataOfferHandler, DragOffer};
use smithay_client_toolkit::data_device_manager::data_source::DataSourceHandler;
use smithay_client_toolkit::data_device_manager::{ReadPipe, WritePipe};
use smithay_client_toolkit::primary_selection::device::PrimarySelectionDeviceHandler;
use smithay_client_toolkit::primary_selection::selection::PrimarySelectionSourceHandler;
use std::fs::File;
use std::io::{ErrorKind, Read, Write};
use std::os::fd::{AsFd, OwnedFd};
use std::time::Duration;
use wayland_client::protocol::wl_data_device::WlDataDevice;
use wayland_client::protocol::wl_data_device_manager::DndAction;
use wayland_client::protocol::wl_data_source::WlDataSource;
use wayland_client::protocol::wl_surface::WlSurface;
use wayland_client::{Connection, QueueHandle};
use wayland_protocols::wp::primary_selection::zv1::client::zwp_primary_selection_device_v1::ZwpPrimarySelectionDeviceV1;
use wayland_protocols::wp::primary_selection::zv1::client::zwp_primary_selection_source_v1::ZwpPrimarySelectionSourceV1;

const TEXT_MIMES: [&str; 5] = [
    "text/plain;charset=utf-8",
    "UTF8_STRING",
    "text/plain",
    "TEXT",
    "STRING",
];
/// Refuse to buffer more than this from another client.
const MAX_READ: usize = 64 << 20;
/// A read that has not finished by then is abandoned.
pub const READ_TIMEOUT: Duration = Duration::from_secs(2);

/// The preferred text mime type among those offered.
pub fn pick_text_mime(offered: &[String]) -> Option<String> {
    TEXT_MIMES
        .iter()
        .find(|m| offered.iter().any(|o| o == *m))
        .map(|m| (*m).to_string())
}

/// Decode what arrived for `mime`.
pub fn decode_text(mime: &str, bytes: Vec<u8>) -> Option<String> {
    let latin1 = |b: &[u8]| b.iter().map(|c| char::from(*c)).collect::<String>();
    match mime {
        "STRING" => Some(latin1(&bytes)),
        "text/plain" | "TEXT" => {
            Some(String::from_utf8(bytes).unwrap_or_else(|e| latin1(e.as_bytes())))
        }
        _ => String::from_utf8(bytes).ok(),
    }
}

/// Encode `text` for a receiver that asked for `mime`.
pub fn encode_text(mime: &str, text: &str) -> Vec<u8> {
    if mime == "STRING" {
        text.chars()
            .map(|c| u8::try_from(u32::from(c)).unwrap_or(b'?'))
            .collect()
    } else {
        text.as_bytes().to_vec()
    }
}

pub(crate) struct PendingRead {
    pipe: RegistrationToken,
    timer: Option<RegistrationToken>,
}

fn set_nonblocking(fd: impl AsFd) -> std::io::Result<()> {
    let flags = rustix::fs::fcntl_getfl(&fd)?;
    rustix::fs::fcntl_setfl(&fd, flags | rustix::fs::OFlags::NONBLOCK)?;
    Ok(())
}

impl Runtime {
    pub(crate) fn set_selection(&mut self, selection: Selection, text: String) -> bool {
        let serial = self.last_serial;
        let source = match selection {
            Selection::Clipboard => {
                let (Some(manager), Some(device)) = (&self.data_manager, &self.seat.data_device)
                else {
                    return false;
                };
                let source = manager.create_copy_paste_source(&self.qh, TEXT_MIMES);
                source.set_selection(device, serial);
                OwnedSource::Clipboard(source)
            }
            Selection::Primary => {
                let (Some(manager), Some(device)) =
                    (&self.primary_manager, &self.seat.primary_device)
                else {
                    return false;
                };
                let source = manager.create_selection_source(&self.qh, TEXT_MIMES);
                source.set_selection(device, serial);
                OwnedSource::Primary(source)
            }
        };
        // Replacing drops (destroys) the previous source.
        self.sources.insert(selection, (source, text));
        true
    }

    fn selection_text(&mut self, selection: Selection, token: u64, status: ReadStatus) {
        self.queue.push_back(Event::SelectionText {
            selection,
            token,
            text: None,
            status,
        });
    }

    pub(crate) fn selection_generation(&self, selection: Selection) -> u64 {
        self.selection_gens.get(&selection).copied().unwrap_or(0)
    }

    pub(crate) fn request_selection(&mut self, selection: Selection) -> u64 {
        self.next_read += 1;
        let token = self.next_read;
        if let Some((_, text)) = self.sources.get(&selection) {
            let text = Some(text.clone());
            self.queue.push_back(Event::SelectionText {
                selection,
                token,
                text,
                status: ReadStatus::Complete,
            });
            return token;
        }
        let receive = match selection {
            Selection::Clipboard => self.seat.data_device.as_ref().and_then(|d| {
                let offer = d.data().selection_offer()?;
                let mime = offer.with_mime_types(pick_text_mime)?;
                Some(
                    offer
                        .receive(mime.clone())
                        .map(|pipe| (pipe, mime))
                        .map_err(|e| e.to_string()),
                )
            }),
            Selection::Primary => self.seat.primary_device.as_ref().and_then(|d| {
                let offer = d.data().selection_offer()?;
                let mime = offer.with_mime_types(pick_text_mime)?;
                Some(
                    offer
                        .receive(mime.clone())
                        .map(|pipe| (pipe, mime))
                        .map_err(|e| e.to_string()),
                )
            }),
        };
        match receive {
            None => self.selection_text(selection, token, ReadStatus::Empty),
            Some(Err(e)) => {
                log::warn!("selection receive: {e}");
                self.selection_text(selection, token, ReadStatus::Failed);
            }
            Some(Ok((pipe, mime))) => {
                if let Err(e) = self.read_pipe(selection, token, pipe, mime) {
                    log::warn!("selection read: {e}");
                    self.selection_text(selection, token, ReadStatus::Failed);
                }
            }
        }
        token
    }

    fn read_pipe(
        &mut self,
        selection: Selection,
        token: u64,
        pipe: ReadPipe,
        mime: String,
    ) -> Result<(), String> {
        set_nonblocking(&pipe).map_err(|e| e.to_string())?;
        let generation = self.selection_generation(selection);
        let mut data = Vec::new();
        let pipe = self
            .handle
            .insert_source(pipe, move |(), file, state: &mut State| {
                let mut chunk = [0u8; 16 * 1024];
                let status = loop {
                    match (&**file).read(&mut chunk) {
                        Ok(0) => break ReadStatus::Complete,
                        Ok(n) if data.len() + n <= MAX_READ => data.extend_from_slice(&chunk[..n]),
                        Ok(_) => {
                            log::warn!("selection larger than {MAX_READ} bytes, dropped");
                            break ReadStatus::TooLarge;
                        }
                        Err(e) if e.kind() == ErrorKind::WouldBlock => return PostAction::Continue,
                        Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                        Err(e) => {
                            log::warn!("selection read: {e}");
                            break ReadStatus::Failed;
                        }
                    }
                };
                let bytes = std::mem::take(&mut data);
                let superseded = state.rt.selection_generation(selection) != generation;
                let (status, text) = match status {
                    ReadStatus::Complete if superseded => (ReadStatus::Superseded, None),
                    ReadStatus::Complete if bytes.is_empty() => (ReadStatus::Empty, None),
                    ReadStatus::Complete => match decode_text(&mime, bytes) {
                        Some(text) => (ReadStatus::Complete, Some(text)),
                        None => (ReadStatus::Failed, None),
                    },
                    other => (other, None),
                };
                if let Some(read) = state.rt.reads.remove(&token)
                    && let Some(timer) = read.timer
                {
                    state.rt.handle.remove(timer);
                }
                state.emit(Event::SelectionText {
                    selection,
                    token,
                    text,
                    status,
                });
                PostAction::Remove
            })
            .map_err(|e| e.to_string())?;
        let timer = self
            .handle
            .insert_source(
                Timer::from_duration(READ_TIMEOUT),
                move |_, _, state: &mut State| {
                    if let Some(read) = state.rt.reads.remove(&token) {
                        state.rt.handle.remove(read.pipe);
                        log::warn!("selection read timed out");
                        state
                            .rt
                            .selection_text(selection, token, ReadStatus::TimedOut);
                        state.drain();
                    }
                    TimeoutAction::Drop
                },
            )
            .map_err(|e| log::warn!("selection read timer: {e}"))
            .ok();
        self.reads.insert(token, PendingRead { pipe, timer });
        Ok(())
    }

    fn write_pipe(&mut self, text: Vec<u8>, pipe: WritePipe) {
        let fd: OwnedFd = pipe.into();
        if let Err(e) = set_nonblocking(&fd) {
            log::warn!("selection write: {e}");
            return;
        }
        let mut offset = 0;
        let source = Generic::new(File::from(fd), Interest::WRITE, Mode::Level);
        let inserted = self
            .handle
            .insert_source(source, move |_, file, _: &mut State| {
                while offset < text.len() {
                    match (&**file).write(&text[offset..]) {
                        Ok(0) => return Ok(PostAction::Remove),
                        Ok(n) => offset += n,
                        Err(e) if e.kind() == ErrorKind::WouldBlock => {
                            return Ok(PostAction::Continue);
                        }
                        Err(e) if e.kind() == ErrorKind::Interrupted => {}
                        // The reader went away (EPIPE); nothing to do.
                        Err(_) => return Ok(PostAction::Remove),
                    }
                }
                Ok(PostAction::Remove)
            });
        if let Err(e) = inserted {
            log::warn!("selection write: {e}");
        }
    }

    fn source_cancelled(&mut self, selection: Selection, matches: impl Fn(&OwnedSource) -> bool) {
        if self
            .sources
            .get(&selection)
            .is_some_and(|(source, _)| matches(source))
        {
            self.sources.remove(&selection);
            self.queue.push_back(Event::SelectionLost { selection });
        }
    }
}

impl State {
    fn selection_changed(&mut self, selection: Selection) {
        // Reads still running were for the previous offer.
        *self.rt.selection_gens.entry(selection).or_default() += 1;
        // The compositor echoes our own selection back; that is not news.
        if self.rt.sources.contains_key(&selection) {
            return;
        }
        self.emit(Event::SelectionChanged { selection });
    }
}

impl DataDeviceHandler for State {
    fn enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &WlDataDevice,
        _: f64,
        _: f64,
        _: &WlSurface,
    ) {
    }
    fn leave(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataDevice) {}
    fn motion(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataDevice, _: f64, _: f64) {}
    fn selection(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataDevice) {
        self.selection_changed(Selection::Clipboard);
    }
    fn drop_performed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataDevice) {}
}

impl DataOfferHandler for State {
    fn source_actions(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &mut DragOffer,
        _: DndAction,
    ) {
    }
    fn selected_action(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &mut DragOffer,
        _: DndAction,
    ) {
    }
}

impl DataSourceHandler for State {
    fn accept_mime(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &WlDataSource,
        _: Option<String>,
    ) {
    }

    fn send_request(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        source: &WlDataSource,
        mime: String,
        pipe: WritePipe,
    ) {
        let bytes = match self.rt.sources.get(&Selection::Clipboard) {
            Some((OwnedSource::Clipboard(s), text)) if s.inner() == source => {
                encode_text(&mime, text)
            }
            _ => return,
        };
        self.rt.write_pipe(bytes, pipe);
    }

    fn cancelled(&mut self, _: &Connection, _: &QueueHandle<Self>, source: &WlDataSource) {
        self.rt.source_cancelled(
            Selection::Clipboard,
            |s| matches!(s, OwnedSource::Clipboard(s) if s.inner() == source),
        );
        self.drain();
    }

    fn dnd_dropped(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataSource) {}
    fn dnd_finished(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataSource) {}
    fn action(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataSource, _: DndAction) {}
}

impl PrimarySelectionDeviceHandler for State {
    fn selection(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &ZwpPrimarySelectionDeviceV1,
    ) {
        self.selection_changed(Selection::Primary);
    }
}

impl PrimarySelectionSourceHandler for State {
    fn send_request(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        source: &ZwpPrimarySelectionSourceV1,
        mime: String,
        pipe: WritePipe,
    ) {
        let bytes = match self.rt.sources.get(&Selection::Primary) {
            Some((OwnedSource::Primary(s), text)) if s.inner() == source => {
                encode_text(&mime, text)
            }
            _ => return,
        };
        self.rt.write_pipe(bytes, pipe);
    }

    fn cancelled(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        source: &ZwpPrimarySelectionSourceV1,
    ) {
        self.rt.source_cancelled(
            Selection::Primary,
            |s| matches!(s, OwnedSource::Primary(s) if s.inner() == source),
        );
        self.drain();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_utf8_first() {
        let offered = vec!["STRING".to_string(), "text/plain;charset=utf-8".to_string()];
        assert_eq!(
            pick_text_mime(&offered).as_deref(),
            Some("text/plain;charset=utf-8")
        );
        assert_eq!(pick_text_mime(&["image/png".to_string()]), None);
    }

    #[test]
    fn text_types_encode_and_decode() {
        let text = "caf\u{e9} \u{2192}";
        for mime in [
            "text/plain;charset=utf-8",
            "UTF8_STRING",
            "TEXT",
            "text/plain",
        ] {
            let bytes = encode_text(mime, text);
            assert_eq!(bytes, text.as_bytes());
            assert_eq!(decode_text(mime, bytes).as_deref(), Some(text));
        }
        let latin1 = encode_text("STRING", text);
        assert_eq!(latin1, b"caf\xe9 ?");
        assert_eq!(
            decode_text("STRING", latin1).as_deref(),
            Some("caf\u{e9} ?")
        );
        // Invalid UTF-8: rejected for the UTF-8 types, read as Latin-1 for
        // the untyped ones.
        assert_eq!(decode_text("UTF8_STRING", vec![0xe9]), None);
        assert_eq!(decode_text("TEXT", vec![0xe9]).as_deref(), Some("\u{e9}"));
    }
}
