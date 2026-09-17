//! Clipboard and primary selection, text only. Pipes are non-blocking and
//! driven by the event loop in both directions.

use crate::event::{Event, Selection};
use crate::runtime::{OwnedSource, Runtime, State};
use calloop::PostAction;
use calloop::generic::Generic;
use calloop::{Interest, Mode};
use smithay_client_toolkit::data_device_manager::data_device::DataDeviceHandler;
use smithay_client_toolkit::data_device_manager::data_offer::{DataOfferHandler, DragOffer};
use smithay_client_toolkit::data_device_manager::data_source::DataSourceHandler;
use smithay_client_toolkit::data_device_manager::{ReadPipe, WritePipe};
use smithay_client_toolkit::primary_selection::device::PrimarySelectionDeviceHandler;
use smithay_client_toolkit::primary_selection::selection::PrimarySelectionSourceHandler;
use std::fs::File;
use std::io::{ErrorKind, Read, Write};
use std::os::fd::{AsFd, OwnedFd};
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

/// The preferred text mime type among those offered.
pub fn pick_text_mime(offered: &[String]) -> Option<String> {
    TEXT_MIMES
        .iter()
        .find(|m| offered.iter().any(|o| o == *m))
        .map(|m| (*m).to_string())
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

    pub(crate) fn request_selection(&mut self, selection: Selection) {
        if let Some((_, text)) = self.sources.get(&selection) {
            let text = Some(text.clone());
            self.queue
                .push_back(Event::SelectionText { selection, text });
            return;
        }
        let pipe = match selection {
            Selection::Clipboard => self.seat.data_device.as_ref().and_then(|d| {
                let offer = d.data().selection_offer()?;
                let mime = offer.with_mime_types(pick_text_mime)?;
                offer
                    .receive(mime)
                    .map_err(|e| log::warn!("clipboard receive: {e}"))
                    .ok()
            }),
            Selection::Primary => self.seat.primary_device.as_ref().and_then(|d| {
                let offer = d.data().selection_offer()?;
                let mime = offer.with_mime_types(pick_text_mime)?;
                offer
                    .receive(mime)
                    .map_err(|e| log::warn!("primary receive: {e}"))
                    .ok()
            }),
        };
        let Some(pipe) = pipe else {
            self.queue.push_back(Event::SelectionText {
                selection,
                text: None,
            });
            return;
        };
        if let Err(e) = self.read_pipe(selection, pipe) {
            log::warn!("selection read: {e}");
            self.queue.push_back(Event::SelectionText {
                selection,
                text: None,
            });
        }
    }

    fn read_pipe(&mut self, selection: Selection, pipe: ReadPipe) -> Result<(), String> {
        set_nonblocking(&pipe).map_err(|e| e.to_string())?;
        let mut data = Vec::new();
        self.handle
            .insert_source(pipe, move |(), file, state: &mut State| {
                let mut chunk = [0u8; 16 * 1024];
                loop {
                    match (&**file).read(&mut chunk) {
                        Ok(0) => break,
                        Ok(n) if data.len() + n <= MAX_READ => data.extend_from_slice(&chunk[..n]),
                        Ok(_) => {
                            log::warn!("selection larger than {MAX_READ} bytes, dropped");
                            data.clear();
                            break;
                        }
                        Err(e) if e.kind() == ErrorKind::WouldBlock => return PostAction::Continue,
                        Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                        Err(e) => {
                            log::warn!("selection read: {e}");
                            data.clear();
                            break;
                        }
                    }
                }
                let text = String::from_utf8(std::mem::take(&mut data))
                    .ok()
                    .filter(|t| !t.is_empty());
                state.emit(Event::SelectionText { selection, text });
                PostAction::Remove
            })
            .map(|_| ())
            .map_err(|e| e.to_string())
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
    fn selection(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataDevice) {}
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
        _mime: String,
        pipe: WritePipe,
    ) {
        let text = match self.rt.sources.get(&Selection::Clipboard) {
            Some((OwnedSource::Clipboard(s), text)) if s.inner() == source => text.clone(),
            _ => return,
        };
        self.rt.write_pipe(text.into_bytes(), pipe);
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
    }
}

impl PrimarySelectionSourceHandler for State {
    fn send_request(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        source: &ZwpPrimarySelectionSourceV1,
        _mime: String,
        pipe: WritePipe,
    ) {
        let text = match self.rt.sources.get(&Selection::Primary) {
            Some((OwnedSource::Primary(s), text)) if s.inner() == source => text.clone(),
            _ => return,
        };
        self.rt.write_pipe(text.into_bytes(), pipe);
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
}
