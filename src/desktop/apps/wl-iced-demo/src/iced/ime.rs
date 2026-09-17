//! Glue: text-input-v3 results to iced input-method events, and iced's
//! input-method request to a text-input-v3 state.

use cosmix_iced_host::ImeRequest;
use cosmix_iced_host::core::Event as IcedEvent;
use cosmix_iced_host::core::input_method::{Event as ImEvent, Purpose};
use cosmix_wl_app::{ContentPurpose, ImeEvent, ImeState, Rect, SurfaceId};

/// `None` for events iced has no counterpart for (surrounding-text deletion:
/// iced text inputs do not report surrounding text, so none is requested).
pub fn to_iced(event: &ImeEvent) -> Option<IcedEvent> {
    Some(IcedEvent::InputMethod(match event {
        ImeEvent::Focus { active: true } => ImEvent::Opened,
        ImeEvent::Focus { active: false } => ImEvent::Closed,
        ImeEvent::Commit(text) => ImEvent::Commit(text.clone()),
        ImeEvent::Preedit { text, cursor } => {
            ImEvent::Preedit(text.clone(), cursor.map(|(a, b)| a..b))
        }
        ImeEvent::DeleteSurrounding { .. } => return None,
    }))
}

/// The text-input state for iced's request, with the caret offset by the
/// chrome's origin in the window (logical).
pub fn to_wl(request: &ImeRequest, surface: SurfaceId, origin: (i32, i32)) -> Option<ImeState> {
    let ImeRequest::Enabled {
        logical_cursor,
        purpose,
        ..
    } = request
    else {
        return None;
    };
    let c = logical_cursor;
    let mut state = ImeState::new(
        surface,
        Rect::new(
            origin.0 + c.x.floor() as i32,
            origin.1 + c.y.floor() as i32,
            c.width.ceil().max(1.0) as i32,
            c.height.ceil().max(1.0) as i32,
        ),
    );
    state.purpose = match purpose {
        Purpose::Normal => ContentPurpose::Normal,
        Purpose::Secure => ContentPurpose::Password,
        Purpose::Terminal => ContentPurpose::Terminal,
    };
    Some(state)
}

#[cfg(test)]
mod tests {
    use super::*;
    use cosmix_iced_host::DamageRect;
    use cosmix_iced_host::core::{Point, Rectangle, Size};

    #[test]
    fn events_map_in_order() {
        assert_eq!(
            to_iced(&ImeEvent::Preedit {
                text: "ka".into(),
                cursor: Some((0, 2))
            }),
            Some(IcedEvent::InputMethod(ImEvent::Preedit(
                "ka".into(),
                Some(0..2)
            )))
        );
        assert_eq!(
            to_iced(&ImeEvent::Focus { active: false }),
            Some(IcedEvent::InputMethod(ImEvent::Closed))
        );
        assert_eq!(
            to_iced(&ImeEvent::DeleteSurrounding {
                before: 1,
                after: 0
            }),
            None
        );
    }

    #[test]
    fn request_maps_rect_and_purpose() {
        let surface = SurfaceId::from_raw(7);
        assert_eq!(to_wl(&ImeRequest::Disabled, surface, (0, 0)), None);
        let req = ImeRequest::Enabled {
            logical_cursor: Rectangle::new(Point::new(10.4, 3.0), Size::new(0.5, 16.2)),
            physical_cursor: DamageRect {
                x: 0,
                y: 0,
                width: 1,
                height: 1,
            },
            purpose: Purpose::Secure,
            preedit: None,
        };
        let state = to_wl(&req, surface, (0, 5)).unwrap();
        assert_eq!(state.cursor, Rect::new(10, 8, 1, 17));
        assert_eq!(state.purpose, ContentPurpose::Password);
    }
}
