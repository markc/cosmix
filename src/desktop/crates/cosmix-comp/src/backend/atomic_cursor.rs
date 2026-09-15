//! Optional cursor plane. Image/hide commits synchronise buffer retirement.
//! Motion is retained for the primary atomic submission, or flushed by the
//! presentation pump when the primary scene is proven idle.

use super::*;
use crate::capture::CaptureCursorSnapshot;
use smithay::reexports::drm::{DriverCapability, buffer::Buffer, control::PlaneType};

const PROPERTIES: [&str; 10] = [
    "FB_ID", "CRTC_ID", "CRTC_X", "CRTC_Y", "CRTC_W", "CRTC_H", "SRC_X", "SRC_Y", "SRC_W", "SRC_H",
];

#[derive(Clone, Default, bevy::prelude::Resource)]
pub(crate) struct HardwareCursorBridge(Arc<Mutex<Option<Box<dyn CursorPlane>>>>);

trait CursorPlane: Send {
    /// False means this image needs software projection; the hardware image
    /// has already been detached. Errors retire the plane for this generation.
    fn project(&mut self, image: Option<&CaptureCursorSnapshot>) -> Result<bool, String>;
    fn pending_request(&self, generation: u64, crtc: u32) -> Option<AtomicRequest>;
    fn position_submitted(&mut self);
    fn flush_position(&mut self, generation: u64) -> Result<(), String>;
}

impl HardwareCursorBridge {
    pub(crate) fn install(
        &self,
        fd: OwnedFd,
        selection: AtomicOutputSelection,
        events: Arc<ProductionAtomicEventRouter>,
        cancellation: Arc<AtomicCancellation>,
        generation: u64,
    ) {
        self.clear();
        match HardwareCursor::new(fd, selection, events, cancellation, generation) {
            Ok(cursor) => {
                tracing::info!(
                    plane = cursor.plane,
                    width = cursor.size.0,
                    height = cursor.size.1,
                    "DRM hardware cursor plane available"
                );
                if let Ok(mut slot) = self.0.lock() {
                    *slot = Some(Box::new(cursor));
                }
            }
            Err(error) => tracing::warn!(%error, "DRM cursor unavailable; using software cursor"),
        }
    }

    pub(crate) fn clear(&self) {
        if let Ok(mut slot) = self.0.lock() {
            slot.take();
        }
    }

    /// `None` hides the cursor. An unsupported visible image must instead call
    /// this with None and use software projection for that image.
    pub(crate) fn project(&self, image: Option<&CaptureCursorSnapshot>) -> bool {
        let Ok(mut slot) = self.0.lock() else {
            return false;
        };
        let Some(cursor) = slot.as_mut() else {
            return false;
        };
        match cursor.project(image) {
            Ok(active) => active,
            Err(error) => {
                tracing::warn!(%error, "DRM cursor update failed; restoring software cursor");
                // RMFB also detaches a still-bound cursor on a failed disable.
                // Only cursor FBs are owned here, never the primary framebuffer.
                slot.take();
                false
            }
        }
    }

    /// Keep image replacement/destruction excluded until the ioctl returns.
    /// Each EBUSY retry rebuilds from the latest position and current FB.
    pub(super) fn commit_primary(
        &self,
        request: &AtomicRequest,
        generation: u64,
        crtc: u32,
        commit: impl FnOnce(&AtomicRequest) -> Result<(), AtomicCommitError>,
    ) -> Result<(), AtomicCommitError> {
        let mut slot = self
            .0
            .lock()
            .map_err(|_| AtomicCommitError::synthetic("cursor merge", "cursor lock poisoned"))?;
        let Some(cursor) = slot.as_mut() else {
            return commit(request);
        };
        let Some(pending) = cursor.pending_request(generation, crtc) else {
            return commit(request);
        };
        let mut merged = request.clone();
        merged.properties.extend(pending.properties);
        commit(&merged)?;
        cursor.position_submitted();
        Ok(())
    }

    /// Called only by a healthy-idle pump update, never by an input event.
    pub(crate) fn flush_idle(&self, generation: u64) {
        let Ok(mut slot) = self.0.lock() else {
            return;
        };
        if let Some(cursor) = slot.as_mut()
            && let Err(error) = cursor.flush_position(generation)
        {
            tracing::warn!(%error, "DRM cursor flush failed; restoring software cursor");
            slot.take();
        }
    }

    #[cfg(test)]
    pub(crate) fn for_test(
        project: impl FnMut(Option<&CaptureCursorSnapshot>) -> Result<bool, String> + Send + 'static,
    ) -> Self {
        struct Fake<F>(F);
        impl<F> CursorPlane for Fake<F>
        where
            F: FnMut(Option<&CaptureCursorSnapshot>) -> Result<bool, String> + Send,
        {
            fn project(&mut self, image: Option<&CaptureCursorSnapshot>) -> Result<bool, String> {
                (self.0)(image)
            }
            fn pending_request(&self, _: u64, _: u32) -> Option<AtomicRequest> {
                None
            }
            fn position_submitted(&mut self) {}
            fn flush_position(&mut self, _: u64) -> Result<(), String> {
                Ok(())
            }
        }
        Self(Arc::new(Mutex::new(Some(Box::new(Fake(project))))))
    }

    #[cfg(test)]
    pub(crate) fn pending_for_test(
        generation: u64,
        idle: impl FnMut() -> bool + Send + 'static,
    ) -> Self {
        struct Fake<F> {
            generation: u64,
            pending: bool,
            idle: F,
        }
        impl<F: FnMut() -> bool + Send> CursorPlane for Fake<F> {
            fn project(&mut self, _: Option<&CaptureCursorSnapshot>) -> Result<bool, String> {
                Ok(true)
            }
            fn pending_request(&self, generation: u64, _: u32) -> Option<AtomicRequest> {
                (self.pending && self.generation == generation).then(|| {
                    let mut request = AtomicRequest::default();
                    request.set(90, 1, 100);
                    request.set(90, 3, (-7_i64) as u64);
                    request.set(90, 4, 13);
                    request
                })
            }
            fn position_submitted(&mut self) {
                self.pending = false;
            }
            fn flush_position(&mut self, generation: u64) -> Result<(), String> {
                if self.pending && generation == self.generation && (self.idle)() {
                    self.position_submitted();
                }
                Ok(())
            }
        }
        Self(Arc::new(Mutex::new(Some(Box::new(Fake {
            generation,
            pending: true,
            idle,
        })))))
    }
}

struct CursorBuffer {
    dumb: control::dumbbuffer::DumbBuffer,
    fb: control::framebuffer::Handle,
}

struct HardwareCursor {
    io: ProductionAtomicIo,
    plane: u32,
    crtc: u32,
    crtc_active: u32,
    admitted: bool,
    properties: [u32; 10],
    size: (u32, u32),
    buffers: Vec<CursorBuffer>,
    front: usize,
    image: Option<CaptureCursorSnapshot>,
    pending_position: Option<(i32, i32)>,
    cancellation: Arc<AtomicCancellation>,
    generation: u64,
}

impl CursorPlane for HardwareCursor {
    fn pending_request(&self, generation: u64, crtc: u32) -> Option<AtomicRequest> {
        if generation != self.generation
            || crtc != self.crtc
            || self.cancellation.cancelled(generation)
        {
            return None;
        }
        let (x, y) = self.pending_position?;
        let mut image = self.image.clone()?;
        image.x = x;
        image.y = y;
        let fb = self.buffers.get(self.front)?.fb;
        Some(self.request(Some(&image), fb.into(), true))
    }

    fn position_submitted(&mut self) {
        if let Some((x, y)) = self.pending_position.take()
            && let Some(image) = self.image.as_mut()
        {
            image.x = x;
            image.y = y;
        }
    }

    fn flush_position(&mut self, generation: u64) -> Result<(), String> {
        let Some(request) = self.pending_request(generation, self.crtc) else {
            return Ok(());
        };
        let result = self.commit(&request, false, true);
        self.finish_position_commit(result)
    }

    fn project(&mut self, image: Option<&CaptureCursorSnapshot>) -> Result<bool, String> {
        if self.cancellation.cancelled(self.generation) {
            return Ok(false);
        }
        if self.image.is_none() && image.is_some() {
            // Startup/resume can install the scene before the first primary
            // modeset. Wait for ACTIVE rather than permanently rejecting a
            // perfectly usable cursor plane on an inactive CRTC.
            let crtc = control::from_u32::<control::crtc::Handle>(self.crtc)
                .ok_or("invalid cursor CRTC")?;
            let properties = self
                .io
                .card
                .get_properties(crtc)
                .map_err(|e| e.to_string())?;
            if !properties
                .iter()
                .any(|(id, value)| u32::from(*id) == self.crtc_active && *value != 0)
            {
                return Ok(false);
            }
        }
        if image.is_some_and(|image| image.width > self.size.0 || image.height > self.size.1) {
            self.update(None)?;
            return Ok(false);
        }
        self.update(image)?;
        Ok(self.admitted)
    }
}

impl HardwareCursor {
    fn finish_position_commit(
        &mut self,
        result: Result<(), AtomicCommitError>,
    ) -> Result<(), String> {
        match result {
            Ok(()) => self.position_submitted(),
            Err(error) if error.is_busy() => (),
            Err(error) => return Err(error.to_string()),
        }
        Ok(())
    }

    fn new(
        fd: OwnedFd,
        selection: AtomicOutputSelection,
        events: Arc<ProductionAtomicEventRouter>,
        cancellation: Arc<AtomicCancellation>,
        generation: u64,
    ) -> Result<Self, String> {
        let io = ProductionAtomicIo::new(fd, events);
        let card = &io.card;
        let resources = card.resource_handles().map_err(|e| e.to_string())?;
        let crtc = control::from_u32::<control::crtc::Handle>(selection.crtc_id)
            .ok_or("invalid cursor CRTC")?;
        let crtc_active = property_id(card, crtc, "ACTIVE")?;
        let width = cursor_dimension(card.get_driver_capability(DriverCapability::CursorWidth))?;
        let height = cursor_dimension(card.get_driver_capability(DriverCapability::CursorHeight))?;
        for plane in card.plane_handles().map_err(|e| e.to_string())? {
            let Ok(info) = card.get_plane(plane) else {
                continue;
            };
            if !resources
                .filter_crtcs(info.possible_crtcs())
                .iter()
                .any(|crtc| u32::from(*crtc) == selection.crtc_id)
                || !info
                    .formats()
                    .contains(&(drm_fourcc::DrmFourcc::Argb8888 as u32))
            {
                continue;
            }
            let Ok(values) = card.get_properties(plane) else {
                continue;
            };
            let mut cursor_type = false;
            let mut occupied = false;
            for (id, value) in values.iter() {
                let Ok(property) = card.get_property(*id) else {
                    continue;
                };
                match property.name().to_bytes() {
                    b"type" => cursor_type = *value == PlaneType::Cursor as u64,
                    b"CRTC_ID" => occupied = *value != 0 && *value != u64::from(selection.crtc_id),
                    _ => (),
                }
            }
            if !cursor_type || occupied {
                continue;
            }
            let ids = PROPERTIES
                .iter()
                .map(|name| property_id(card, plane, name))
                .collect::<Result<Vec<_>, _>>();
            let Ok(ids) = ids else {
                continue;
            };
            let properties: [u32; 10] = ids.try_into().map_err(|_| "cursor property count")?;
            return Ok(Self {
                io,
                plane: plane.into(),
                crtc: selection.crtc_id,
                crtc_active,
                admitted: false,
                properties,
                size: (width, height),
                buffers: Vec::new(),
                front: 0,
                image: None,
                pending_position: None,
                cancellation,
                generation,
            });
        }
        Err("no compatible ARGB8888 cursor plane with atomic properties".into())
    }

    fn request(&self, image: Option<&CaptureCursorSnapshot>, fb: u32, full: bool) -> AtomicRequest {
        let mut request = AtomicRequest::default();
        let Some(image) = image else {
            request.set(self.plane, self.properties[0], 0);
            request.set(self.plane, self.properties[1], 0);
            return request;
        };
        request.set(self.plane, self.properties[2], image.x as i64 as u64);
        request.set(self.plane, self.properties[3], image.y as i64 as u64);
        if full {
            for (index, value) in [
                (0, u64::from(fb)),
                (1, u64::from(self.crtc)),
                (4, u64::from(self.size.0)),
                (5, u64::from(self.size.1)),
                (6, 0),
                (7, 0),
                (8, u64::from(self.size.0) << 16),
                (9, u64::from(self.size.1) << 16),
            ] {
                request.set(self.plane, self.properties[index], value);
            }
        }
        request
    }

    fn commit(
        &mut self,
        request: &AtomicRequest,
        test_only: bool,
        nonblock: bool,
    ) -> Result<(), AtomicCommitError> {
        // Recheck after allocation/upload or TEST_ONLY: pause may have been
        // published while that work ran. Never queue a new revoked update.
        if self.cancellation.cancelled(self.generation) {
            return Err(AtomicCommitError::synthetic(
                "cursor atomic commit",
                "output generation was cancelled",
            ));
        }
        self.io.commit(
            request,
            AtomicCommitOptions {
                test_only,
                allow_modeset: false,
                nonblock,
                page_flip_event: false,
                correlation: None,
            },
        )
    }

    fn update(&mut self, image: Option<&CaptureCursorSnapshot>) -> Result<(), String> {
        if image.is_none() && self.image.is_none() {
            return Ok(());
        }
        let changed = match (self.image.as_ref(), image) {
            (Some(old), Some(new)) => !same_pixels(old, new),
            _ => true,
        };
        if !changed {
            self.pending_position = self.image.as_ref().zip(image).and_then(|(old, new)| {
                (old.x != new.x || old.y != new.y).then_some((new.x, new.y))
            });
            return Ok(());
        }
        // An image replacement/hide supersedes any motion for the old image.
        self.pending_position = None;
        if let Some(image) = image {
            if image.width > self.size.0 || image.height > self.size.1 {
                return Err("cursor image exceeds driver cursor dimensions".into());
            }
            if changed {
                while self.buffers.len() < 2 {
                    let dumb = self
                        .io
                        .card
                        .create_dumb_buffer(self.size, drm_fourcc::DrmFourcc::Argb8888, 32)
                        .map_err(|e| e.to_string())?;
                    let fb = match self.io.card.add_framebuffer(&dumb, 32, 32) {
                        Ok(fb) => fb,
                        Err(e) => {
                            let _ = self.io.card.destroy_dumb_buffer(dumb);
                            return Err(e.to_string());
                        }
                    };
                    self.buffers.push(CursorBuffer { dumb, fb });
                }
                let back = 1 - self.front;
                let buffer = &mut self.buffers[back];
                let pitch = buffer.dumb.pitch() as usize;
                let mut mapping = self
                    .io
                    .card
                    .map_dumb_buffer(&mut buffer.dumb)
                    .map_err(|e| e.to_string())?;
                pack_argb(&mut mapping, pitch, self.size, image)?;
            }
        }
        let next = if changed && image.is_some() {
            1 - self.front
        } else {
            self.front
        };
        let fb = self
            .buffers
            .get(next)
            .map_or(0, |buffer| u32::from(buffer.fb));
        let request = self.request(image, fb, changed);
        if changed && image.is_some() {
            self.commit(&request, true, false)
                .map_err(|e| e.to_string())?;
        }
        match self.commit(&request, false, !changed) {
            // Preserve the existing image-commit lifetime rule: EBUSY does
            // not advance front/image or retire the plane to software.
            Err(error) if error.is_busy() => return Ok(()),
            Err(error) => return Err(error.to_string()),
            Ok(()) => (),
        }
        self.front = next;
        self.image = image.cloned();
        if image.is_some() && !self.admitted {
            self.admitted = true;
            tracing::info!(plane = self.plane, "DRM hardware cursor active");
        }
        Ok(())
    }
}

impl Drop for HardwareCursor {
    fn drop(&mut self) {
        if self.image.is_some() && !self.cancellation.cancelled(self.generation) {
            let request = self.request(None, 0, true);
            if let Err(error) = self.commit(&request, false, false) {
                tracing::warn!(%error, "cursor disable failed; removing owned cursor framebuffers");
            }
        }
        for buffer in self.buffers.drain(..) {
            if let Err(error) = self.io.card.destroy_framebuffer(buffer.fb) {
                tracing::warn!(%error, "cursor RMFB failed; retaining cursor storage until DRM fd closes");
                continue;
            }
            let _ = self.io.card.destroy_dumb_buffer(buffer.dumb);
        }
    }
}

fn cursor_dimension(result: io::Result<u64>) -> Result<u32, String> {
    // Older drivers do not implement these caps; the DRM cursor ABI default
    // is 64. A test-only commit still checks the actual plane constraints.
    let value = match result {
        Ok(value) => value,
        Err(error)
            if matches!(
                error.raw_os_error(),
                Some(libc::EINVAL | libc::ENOTTY | libc::EOPNOTSUPP)
            ) =>
        {
            64
        }
        Err(error) => return Err(format!("cursor capability query failed: {error}")),
    };
    if (1..=512).contains(&value) {
        Ok(value as u32)
    } else {
        Err(format!("unsupported driver cursor dimension {value}"))
    }
}

fn same_pixels(a: &CaptureCursorSnapshot, b: &CaptureCursorSnapshot) -> bool {
    a.width == b.width
        && a.height == b.height
        && a.premultiplied == b.premultiplied
        && a.rgba == b.rgba
}

fn pack_argb(
    destination: &mut [u8],
    pitch: usize,
    size: (u32, u32),
    image: &CaptureCursorSnapshot,
) -> Result<(), String> {
    if image.width == 0
        || image.height == 0
        || image.width > size.0
        || image.height > size.1
        || pitch < size.0 as usize * 4
        || destination.len() < pitch * size.1 as usize
        || image.rgba.len() != image.width as usize * image.height as usize * 4
    {
        return Err("invalid cursor buffer geometry".into());
    }
    destination.fill(0);
    for y in 0..image.height as usize {
        for x in 0..image.width as usize {
            let offset = (y * image.width as usize + x) * 4;
            let pixel = &image.rgba[offset..offset + 4];
            let alpha = u32::from(pixel[3]);
            let channel = |value: u8| {
                if image.premultiplied {
                    value
                } else {
                    ((u32::from(value) * alpha + 127) / 255) as u8
                }
            };
            let argb = (alpha << 24)
                | (u32::from(channel(pixel[0])) << 16)
                | (u32::from(channel(pixel[1])) << 8)
                | u32::from(channel(pixel[2]));
            destination[y * pitch + x * 4..y * pitch + x * 4 + 4]
                .copy_from_slice(&argb.to_ne_bytes());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image() -> CaptureCursorSnapshot {
        CaptureCursorSnapshot {
            x: -7,
            y: 13,
            width: 1,
            height: 1,
            rgba: Arc::new(vec![200, 100, 50, 128]),
            premultiplied: false,
        }
    }

    #[test]
    fn cursor_upload_premultiplies_argb_and_clears_pitch_padding() {
        let mut bytes = vec![255; 24];
        pack_argb(&mut bytes, 12, (2, 2), &image()).unwrap();
        assert_eq!(&bytes[..4], &0x8064_3219_u32.to_ne_bytes());
        assert!(bytes[4..].iter().all(|byte| *byte == 0));
        let mut premultiplied = image();
        premultiplied.premultiplied = true;
        pack_argb(&mut bytes, 12, (2, 2), &premultiplied).unwrap();
        assert_eq!(&bytes[..4], &0x80c8_6432_u32.to_ne_bytes());
    }

    #[test]
    fn malformed_cursor_storage_is_rejected_before_writing() {
        let mut bytes = vec![255; 16];
        assert!(pack_argb(&mut bytes, 3, (2, 2), &image()).is_err());
        assert!(pack_argb(&mut bytes, 12, (2, 2), &image()).is_err());
        let mut invalid = image();
        invalid.width = 3;
        assert!(pack_argb(&mut bytes, 8, (2, 2), &invalid).is_err());
        invalid.width = 1;
        invalid.rgba = Arc::new(vec![0; 3]);
        assert!(pack_argb(&mut bytes, 8, (2, 2), &invalid).is_err());
        assert_eq!(bytes, vec![255; 16]);
    }

    #[test]
    fn cursor_caps_are_bounded_and_old_drivers_use_64() {
        assert_eq!(cursor_dimension(Ok(128)).unwrap(), 128);
        assert_eq!(
            cursor_dimension(Err(io::Error::from_raw_os_error(libc::EINVAL))).unwrap(),
            64
        );
        assert!(cursor_dimension(Ok(0)).is_err());
        assert!(cursor_dimension(Ok(u64::MAX)).is_err());
    }

    #[test]
    fn cursor_motion_and_hide_requests_touch_only_the_cursor_plane() {
        // eventfd is only inert ownership storage: this test never opens DRM
        // or submits an ioctl, including on Drop (no FBs have been allocated).
        let raw = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
        assert!(raw >= 0);
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };
        let events = ProductionAtomicEventRouter::new(fd.try_clone().unwrap()).unwrap();
        let mut cursor = HardwareCursor {
            io: ProductionAtomicIo::new(fd, events),
            plane: 90,
            crtc: 20,
            crtc_active: 11,
            admitted: false,
            properties: [1, 2, 3, 4, 5, 6, 7, 8, 9, 10],
            size: (64, 64),
            buffers: Vec::new(),
            front: 0,
            image: None,
            pending_position: None,
            cancellation: AtomicCancellation::new().unwrap(),
            generation: 1,
        };
        let motion = cursor.request(Some(&image()), 100, false);
        assert_eq!(
            motion.properties,
            vec![
                AtomicProperty {
                    object: 90,
                    property: 3,
                    value: (-7_i64) as u64
                },
                AtomicProperty {
                    object: 90,
                    property: 4,
                    value: 13
                },
            ]
        );
        let show = cursor.request(Some(&image()), 100, true);
        assert_eq!(show.properties.len(), 10);
        assert!(show.properties.iter().all(|p| p.object == 90));
        assert!(
            show.properties
                .iter()
                .any(|p| p.property == 9 && p.value == 64 << 16)
        );
        let hide = cursor.request(None, 0, true);
        assert_eq!(
            hide.properties,
            vec![
                AtomicProperty {
                    object: 90,
                    property: 1,
                    value: 0
                },
                AtomicProperty {
                    object: 90,
                    property: 2,
                    value: 0
                },
            ]
        );
        // An admitted position-only update must never reach the eventfd's
        // invalid DRM ioctl. Coalesce, return to the submitted point, then
        // prove submitted coordinates advance only at successful submission.
        cursor.image = Some(image());
        cursor.admitted = true;
        let mut moved = image();
        moved.x = 40;
        assert!(cursor.project(Some(&moved)).unwrap());
        moved.x = 70;
        assert!(cursor.project(Some(&moved)).unwrap());
        assert_eq!(cursor.pending_position, Some((70, 13)));
        assert_eq!(cursor.image.as_ref().unwrap().x, -7);
        assert_eq!(cursor.front, 0);
        assert!(cursor.buffers.is_empty());
        cursor
            .finish_position_commit(Err(AtomicCommitError {
                operation: "cursor test",
                errno: Some(libc::EBUSY),
                detail: "busy".into(),
            }))
            .unwrap();
        assert_eq!(cursor.pending_position, Some((70, 13)));
        assert_eq!(cursor.image.as_ref().unwrap().x, -7);
        cursor.project(Some(&image())).unwrap();
        assert_eq!(cursor.pending_position, None);
        cursor.project(Some(&moved)).unwrap();
        cursor.finish_position_commit(Ok(())).unwrap();
        assert_eq!(cursor.image.as_ref().unwrap().x, 70);
        assert_eq!(cursor.pending_position, None);
        moved.y = 42;
        cursor.project(Some(&moved)).unwrap();
        cursor.cancellation.cancel(CancelScope::Generation(1));
        assert!(!cursor.project(Some(&image())).unwrap());
        assert!(cursor.pending_request(1, 20).is_none());
        assert!(cursor.pending_request(2, 20).is_none());
        cursor.flush_position(1).unwrap();
        // eventfd is not DRM: reaching the ioctl would fail with ENOTTY.
        let error = cursor.commit(&motion, false, true).unwrap_err();
        assert!(error.to_string().contains("generation was cancelled"));
    }
}
