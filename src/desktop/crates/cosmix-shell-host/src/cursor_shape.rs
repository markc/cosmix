//! wp_cursor_shape_v1 for the layer host, driven by `CursorShapeRequest`.
use super::RunnerState;
use cosmix_shell::runtime::{CursorShape, CursorShapeRequest};
use wayland_client::protocol::wl_pointer::WlPointer;
use wayland_client::{Connection, Dispatch, QueueHandle, globals::GlobalList};
use wayland_protocols::wp::cursor_shape::v1::client::{
    wp_cursor_shape_device_v1::{Shape, WpCursorShapeDeviceV1},
    wp_cursor_shape_manager_v1::WpCursorShapeManagerV1,
};

pub(super) struct CursorShapeBridge {
    manager: Option<WpCursorShapeManagerV1>,
    device: Option<WpCursorShapeDeviceV1>,
    /// Serial of the pointer's latest enter on one of our surfaces.
    enter_serial: Option<u32>,
    applied: Option<(u32, CursorShape)>,
}

impl CursorShapeBridge {
    pub(super) fn new(globals: &GlobalList, qh: &QueueHandle<RunnerState>) -> Self {
        Self {
            manager: globals.bind(qh, 1..=1, ()).ok(),
            device: None,
            enter_serial: None,
            applied: None,
        }
    }

    pub(super) fn attach(&mut self, pointer: &WlPointer, qh: &QueueHandle<RunnerState>) {
        self.detach();
        self.device = self
            .manager
            .as_ref()
            .map(|manager| manager.get_pointer(pointer, qh, ()));
    }

    pub(super) fn detach(&mut self) {
        if let Some(device) = self.device.take() {
            device.destroy();
        }
        self.enter_serial = None;
        self.applied = None;
    }

    pub(super) fn entered(&mut self, serial: Option<u32>) {
        self.enter_serial = serial;
        // The compositor shows its own cursor after an enter until we set one.
        self.applied = None;
    }

    /// The `set_shape` call needed now, if any.
    fn plan(&mut self, requested: CursorShape) -> Option<(u32, Shape)> {
        let serial = self.enter_serial?;
        if self.applied == Some((serial, requested)) {
            return None;
        }
        self.applied = Some((serial, requested));
        Some((serial, wayland_shape(requested)))
    }
}

pub(super) fn wayland_shape(shape: CursorShape) -> Shape {
    match shape {
        CursorShape::Default => Shape::Default,
        CursorShape::Pointer => Shape::Pointer,
        CursorShape::Text => Shape::Text,
        CursorShape::Grab => Shape::Grab,
        CursorShape::Grabbing => Shape::Grabbing,
        CursorShape::NotAllowed => Shape::NotAllowed,
        CursorShape::EwResize => Shape::EwResize,
        CursorShape::NsResize => Shape::NsResize,
        CursorShape::Crosshair => Shape::Crosshair,
        CursorShape::Wait => Shape::Wait,
    }
}

impl RunnerState {
    pub(super) fn sync_cursor_shape(&mut self) {
        let requested = self
            .app
            .world()
            .get_resource::<CursorShapeRequest>()
            .map_or(CursorShape::Default, |request| request.shape);
        if self.cursor_shape.device.is_none() {
            return;
        }
        if let Some((serial, shape)) = self.cursor_shape.plan(requested)
            && let Some(device) = self.cursor_shape.device.as_ref()
        {
            device.set_shape(serial, shape);
        }
    }
}

impl Dispatch<WpCursorShapeManagerV1, ()> for RunnerState {
    fn event(
        _: &mut Self,
        _: &WpCursorShapeManagerV1,
        _: <WpCursorShapeManagerV1 as wayland_client::Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WpCursorShapeDeviceV1, ()> for RunnerState {
    fn event(
        _: &mut Self,
        _: &WpCursorShapeDeviceV1,
        _: <WpCursorShapeDeviceV1 as wayland_client::Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bridge() -> CursorShapeBridge {
        CursorShapeBridge {
            manager: None,
            device: None,
            enter_serial: None,
            applied: None,
        }
    }

    #[test]
    fn shapes_map_to_protocol_names() {
        for (shape, expected) in [
            (CursorShape::Default, Shape::Default),
            (CursorShape::Pointer, Shape::Pointer),
            (CursorShape::Text, Shape::Text),
            (CursorShape::Grab, Shape::Grab),
            (CursorShape::Grabbing, Shape::Grabbing),
            (CursorShape::NotAllowed, Shape::NotAllowed),
            (CursorShape::EwResize, Shape::EwResize),
            (CursorShape::NsResize, Shape::NsResize),
            (CursorShape::Crosshair, Shape::Crosshair),
            (CursorShape::Wait, Shape::Wait),
        ] {
            assert_eq!(wayland_shape(shape), expected);
        }
    }

    #[test]
    fn shape_is_set_once_per_enter_and_on_change() {
        let mut bridge = bridge();
        // No enter yet: nothing can be set.
        assert_eq!(bridge.plan(CursorShape::Text), None);
        bridge.entered(Some(7));
        assert_eq!(bridge.plan(CursorShape::Text), Some((7, Shape::Text)));
        assert_eq!(bridge.plan(CursorShape::Text), None);
        assert_eq!(bridge.plan(CursorShape::Default), Some((7, Shape::Default)));
        // A new enter resets the compositor's cursor, so the shape is resent.
        bridge.entered(Some(9));
        assert_eq!(bridge.plan(CursorShape::Default), Some((9, Shape::Default)));
        bridge.entered(None);
        assert_eq!(bridge.plan(CursorShape::Pointer), None);
    }
}
