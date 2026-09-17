use cosmix_iced_host::core::keyboard::{self, Key, Modifiers, key::Physical};
use cosmix_iced_host::core::mouse::{self, Button, Interaction};
use cosmix_iced_host::core::widget::{Id, Operation};
use cosmix_iced_host::core::{Color, Event, Font, Point, Rectangle, SmolStr, input_method};
use cosmix_iced_host::widget::{button, column, container, pick_list, row, text, text_input};
use cosmix_iced_host::{
    DamageRect, Element, Frame, ImeRequest, MemoryClipboard, PixelFormat, Program, Redraw,
    Settings, Surface,
};
use std::sync::{Arc, Mutex};

static FONT: &[u8] = include_bytes!("../../cosmix-comp/assets/fonts/DejaVuSans.ttf");
const OPTIONS: &[&str] = &["alpha", "beta"];
const BACKGROUND: Color = Color::from_rgb8(10, 20, 200);

#[derive(Default)]
struct App {
    clicks: u32,
    value: String,
    choice: Option<&'static str>,
}

#[derive(Debug, Clone)]
enum Msg {
    Pressed,
    Input(String),
    Chose(&'static str),
}

impl Program for App {
    type Message = Msg;

    fn update(&mut self, message: Msg) {
        match message {
            Msg::Pressed => self.clicks += 1,
            Msg::Input(value) => self.value = value,
            Msg::Chose(choice) => self.choice = Some(choice),
        }
    }

    fn view(&self) -> Element<'_, Msg> {
        column![
            row![
                container(
                    button(text("Press"))
                        .on_press(Msg::Pressed)
                        .width(100.0)
                        .height(30.0)
                )
                .id("button"),
                container(pick_list(OPTIONS, self.choice, Msg::Chose).width(70.0)).id("pick"),
            ]
            .spacing(10.0),
            container(
                text_input("type here", &self.value)
                    .on_input(Msg::Input)
                    .width(150.0)
            )
            .id("input"),
        ]
        .spacing(20.0)
        .padding(10.0)
        .into()
    }
}

struct Target {
    surface: Surface<App>,
    buffer: Vec<u8>,
    width: u32,
    height: u32,
    format: PixelFormat,
}

impl Target {
    fn new(scale: f32, format: PixelFormat) -> Self {
        cosmix_iced_host::load_font(FONT);
        let width = (200.0 * scale).round() as u32;
        let height = (120.0 * scale).round() as u32;
        let surface = Surface::new(
            App::default(),
            Settings {
                physical_size: (width, height).into(),
                scale_factor: scale,
                default_font: Font::with_name("DejaVu Sans"),
                background: Some(BACKGROUND),
                ..Settings::default()
            },
        );
        Self {
            surface,
            buffer: vec![0; width as usize * height as usize * 4],
            width,
            height,
            format,
        }
    }

    fn draw(&mut self) -> Frame {
        self.surface
            .draw(
                &mut self.buffer,
                self.width,
                self.height,
                self.width * 4,
                self.format,
            )
            .expect("draw")
    }

    fn pixel(&self, x: u32, y: u32) -> [u8; 4] {
        let i = (y * self.width + x) as usize * 4;
        self.buffer[i..i + 4].try_into().unwrap()
    }

    fn scale(&self) -> f32 {
        self.surface.scale_factor()
    }

    /// Logical bounds of the container with `id`.
    fn bounds(&mut self, id: &'static str) -> Rectangle {
        let found = Arc::new(Mutex::new(None));
        self.surface.operate(Box::new(Probe {
            id: Id::from(id),
            found: found.clone(),
        }));
        found.lock().unwrap().expect("container bounds")
    }

    fn physical(&self, logical: Rectangle, margin: f32) -> DamageRect {
        DamageRect::from_logical(
            logical.expand(margin),
            self.scale(),
            self.width,
            self.height,
        )
        .unwrap()
    }

    fn click(&mut self, at: Point) -> cosmix_iced_host::Update {
        self.surface.cursor_moved(at);
        self.surface
            .queue_event(Event::Mouse(mouse::Event::ButtonPressed(Button::Left)));
        self.surface
            .queue_event(Event::Mouse(mouse::Event::ButtonReleased(Button::Left)));
        self.surface.process()
    }
}

struct Probe {
    id: Id,
    found: Arc<Mutex<Option<Rectangle>>>,
}

impl Operation for Probe {
    fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation)) {
        operate(self);
    }

    fn container(&mut self, id: Option<&Id>, bounds: Rectangle) {
        if id == Some(&self.id) {
            *self.found.lock().unwrap() = Some(bounds);
        }
    }
}

fn type_char(surface: &mut Surface<App>, c: &str) {
    surface.queue_event(Event::Keyboard(keyboard::Event::KeyPressed {
        key: Key::Character(SmolStr::new(c)),
        modified_key: Key::Character(SmolStr::new(c)),
        physical_key: Physical::Unidentified(keyboard::key::NativeCode::Unidentified),
        location: keyboard::Location::Standard,
        modifiers: Modifiers::empty(),
        text: Some(SmolStr::new(c)),
        repeat: false,
    }));
}

fn assert_damage_within(frame: &Frame, outer: DamageRect) {
    assert!(!frame.full, "unexpected full damage");
    assert!(!frame.damage.is_empty(), "expected damage");
    for rect in &frame.damage {
        assert!(rect.is_within(&outer), "damage {rect:?} escapes {outer:?}");
    }
}

fn background_rgba() -> [u8; 4] {
    [10, 20, 200, 255]
}

#[test]
fn first_draw_paints_everything_and_an_unchanged_frame_paints_nothing() {
    let mut target = Target::new(1.0, PixelFormat::Rgba8);
    let frame = target.draw();
    assert!(frame.full);
    assert_eq!(
        frame.damage,
        vec![DamageRect {
            x: 0,
            y: 0,
            width: 200,
            height: 120
        }]
    );

    let button = target.bounds("button");
    assert_eq!(
        button,
        Rectangle::new(Point::new(10.0, 10.0), (100.0, 30.0).into())
    );
    let inside = target.pixel(15, 15);
    assert_ne!(inside, background_rgba(), "button not drawn");
    assert_eq!(target.pixel(195, 5), background_rgba());
    assert_eq!(target.pixel(195, 115), background_rgba());

    let again = target.draw();
    assert!(!again.full);
    assert!(
        again.damage.is_empty(),
        "unchanged frame damaged {:?}",
        again.damage
    );
    assert_eq!(again.requests.redraw, Redraw::Wait);

    let idle = target.surface.process();
    assert!(!idle.needs_redraw);
    assert_eq!(idle.messages, 0);
    assert!(idle.statuses.is_empty());
}

#[test]
fn hover_damages_only_the_button_and_a_click_reaches_update() {
    let mut target = Target::new(1.0, PixelFormat::Rgba8);
    target.draw();
    let button = target.bounds("button");
    let before = target.pixel(15, 15);

    target.surface.cursor_moved(button.center());
    let update = target.surface.process();
    assert!(update.needs_redraw);
    assert_eq!(update.interaction, Interaction::Pointer);
    assert!(update.interaction_changed);

    let frame = target.draw();
    assert_damage_within(&frame, target.physical(button, 2.0));
    assert_eq!(frame.requests.interaction, Interaction::Pointer);
    assert_ne!(target.pixel(15, 15), before, "hover colour not drawn");

    target
        .surface
        .queue_event(Event::Mouse(mouse::Event::ButtonPressed(Button::Left)));
    target
        .surface
        .queue_event(Event::Mouse(mouse::Event::ButtonReleased(Button::Left)));
    let update = target.surface.process();
    assert_eq!(update.messages, 1);
    assert!(update.needs_redraw);
    assert_eq!(target.surface.program().clicks, 1);
}

#[test]
fn typing_damages_only_the_input_and_ime_follows_focus() {
    let mut target = Target::new(1.0, PixelFormat::Argb8888);
    target
        .surface
        .set_clipboard(Box::new(MemoryClipboard::default()));
    target.draw();
    let input = target.bounds("input");

    target.click(input.center());
    let frame = target.draw();
    assert!(frame.ime_changed);
    let ImeRequest::Enabled {
        logical_cursor,
        physical_cursor,
        preedit,
        ..
    } = &frame.requests.ime
    else {
        panic!("IME not enabled on focus: {:?}", frame.requests.ime);
    };
    assert!(input.contains(logical_cursor.center()));
    assert!(physical_cursor.is_within(&target.physical(input, 0.0)));
    assert_eq!(preedit, &None);
    assert!(
        matches!(frame.requests.redraw, Redraw::At(_)),
        "caret blink not scheduled"
    );

    type_char(&mut target.surface, "a");
    let update = target.surface.process();
    assert_eq!(update.messages, 1);
    assert_eq!(target.surface.program().value, "a");
    let frame = target.draw();
    assert_damage_within(&frame, target.physical(input, 2.0));

    target
        .surface
        .queue_event(Event::InputMethod(input_method::Event::Opened));
    target
        .surface
        .queue_event(Event::InputMethod(input_method::Event::Preedit(
            "xy".into(),
            Some(2..2),
        )));
    target.surface.process();
    let frame = target.draw();
    let ImeRequest::Enabled { preedit, .. } = &frame.requests.ime else {
        panic!("IME disabled during preedit");
    };
    assert_eq!(preedit.as_ref().map(|p| p.content.as_str()), Some("xy"));
    assert!(!frame.damage.is_empty(), "preedit overlay not drawn");

    target
        .surface
        .queue_event(Event::InputMethod(input_method::Event::Commit("xy".into())));
    let update = target.surface.process();
    assert_eq!(update.messages, 1);
    assert_eq!(target.surface.program().value, "axy");

    target.click(Point::new(190.0, 110.0));
    let frame = target.draw();
    assert_eq!(frame.requests.ime, ImeRequest::Disabled);
    assert!(frame.ime_changed);
}

#[test]
fn rgba_is_argb_with_red_and_blue_swapped() {
    let mut rgba = Target::new(1.0, PixelFormat::Rgba8);
    let mut argb = Target::new(1.0, PixelFormat::Argb8888);
    rgba.draw();
    argb.draw();
    assert_eq!(rgba.pixel(195, 5), [10, 20, 200, 255]);
    assert_eq!(argb.pixel(195, 5), [200, 20, 10, 255]);
    for (a, b) in rgba.buffer.chunks_exact(4).zip(argb.buffer.chunks_exact(4)) {
        assert_eq!(a, [b[2], b[1], b[0], b[3]]);
    }
}

#[test]
fn partial_redraw_matches_a_full_redraw() {
    for format in [PixelFormat::Rgba8, PixelFormat::Argb8888] {
        let mut partial = Target::new(1.5, format);
        partial.draw();
        let button = partial.bounds("button");
        partial.surface.cursor_moved(button.center());
        partial.surface.process();
        let frame = partial.draw();
        assert!(!frame.full && !frame.damage.is_empty());

        let mut full = Target::new(1.5, format);
        full.surface.set_cursor(Some(button.center()));
        assert!(full.draw().full);
        assert!(
            partial.buffer == full.buffer,
            "{format:?}: partial redraw differs from full redraw"
        );
    }
}

#[test]
fn scale_2_5_renders_proportionally() {
    let mut one = Target::new(1.0, PixelFormat::Rgba8);
    let mut big = Target::new(2.5, PixelFormat::Rgba8);
    assert_eq!((big.width, big.height), (500, 300));
    one.draw();
    big.draw();
    assert_eq!(one.bounds("button"), big.bounds("button"));

    let (one_box, one_count) = ink(&one);
    let (big_box, big_count) = ink(&big);
    for (a, b) in one_box.iter().zip(big_box) {
        assert!(
            (*a as f32 * 2.5 - b as f32).abs() <= 3.0,
            "{one_box:?} x 2.5 vs {big_box:?}"
        );
    }
    let ratio = big_count as f32 / one_count as f32;
    assert!((ratio - 6.25).abs() < 0.6, "ink ratio {ratio}");

    // Button pixel at the same logical point.
    assert_eq!(one.pixel(15, 15), big.pixel(38, 38));
}

#[test]
fn a_new_buffer_size_forces_full_damage() {
    let mut target = Target::new(1.0, PixelFormat::Rgba8);
    target.draw();
    target.width = 240;
    target.buffer = vec![0; 240 * 120 * 4];
    let frame = target.draw();
    assert!(frame.full);
    assert_eq!(target.surface.logical_size().width, 240.0);

    target.surface.invalidate();
    assert!(target.draw().full);
    assert!(target.draw().damage.is_empty());
}

#[test]
fn bad_buffers_are_refused() {
    let mut target = Target::new(1.0, PixelFormat::Rgba8);
    let mut small = vec![0; 16];
    assert!(
        target
            .surface
            .draw(&mut small, 200, 120, 800, PixelFormat::Rgba8)
            .is_err()
    );
    assert!(
        target
            .surface
            .draw(&mut small, 2, 2, 12, PixelFormat::Rgba8)
            .is_err()
    );
    assert!(
        target
            .surface
            .draw(&mut small, 0, 2, 0, PixelFormat::Rgba8)
            .is_err()
    );
}

/// Bounding box (x0, y0, x1, y1) and count of pixels that are not background.
fn ink(target: &Target) -> ([u32; 4], usize) {
    let mut bbox = [u32::MAX, u32::MAX, 0, 0];
    let mut count = 0;
    for y in 0..target.height {
        for x in 0..target.width {
            if target.pixel(x, y) != background_rgba() {
                count += 1;
                bbox[0] = bbox[0].min(x);
                bbox[1] = bbox[1].min(y);
                bbox[2] = bbox[2].max(x + 1);
                bbox[3] = bbox[3].max(y + 1);
            }
        }
    }
    (bbox, count)
}

#[test]
fn process_surfaces_the_caret_deadline_only_while_a_field_is_focused() {
    let mut target = Target::new(1.0, PixelFormat::Rgba8);
    let input = target.bounds("input");
    let empty = Point::new(190.0, 110.0);
    target.draw();

    // Unfocused: pointer motion over empty space changes nothing.
    target.surface.cursor_moved(empty);
    let update = target.surface.process();
    assert!(!update.needs_redraw);
    assert_eq!(update.redraw, Redraw::Wait);
    assert_eq!(target.surface.process().redraw, Redraw::Wait);

    target.click(input.center());
    target.draw();
    // Motion inside the focused field leaves its status unchanged.
    target
        .surface
        .cursor_moved(input.center() + cosmix_iced_host::core::Vector::new(5.0, 0.0));
    let before = std::time::Instant::now();
    let update = target.surface.process();
    assert!(!update.needs_redraw, "{update:?}");
    let Redraw::At(deadline) = update.redraw else {
        panic!("no caret deadline from process: {update:?}");
    };
    assert!(deadline > before);
    assert!(deadline <= before + std::time::Duration::from_millis(510));
    // Idle: no work, same deadline.
    let idle = target.surface.process();
    assert!(!idle.needs_redraw);
    assert_eq!(idle.redraw, Redraw::At(deadline));

    // A modifier change in the focused field keeps the deadline.
    target
        .surface
        .queue_event(Event::Keyboard(keyboard::Event::ModifiersChanged(
            Modifiers::SHIFT,
        )));
    assert!(matches!(target.surface.process().redraw, Redraw::At(_)));

    // Blur: the pending draw recomputes, and afterwards nothing is scheduled.
    let update = target.click(empty);
    assert!(update.needs_redraw);
    assert_eq!(update.redraw, Redraw::NextFrame);
    assert_eq!(target.draw().requests.redraw, Redraw::Wait);
    target.surface.cursor_moved(Point::new(185.0, 110.0));
    assert_eq!(target.surface.process().redraw, Redraw::Wait);
}

#[test]
fn window_focus_events_gate_the_input_method() {
    let mut target = Target::new(1.0, PixelFormat::Rgba8);
    let input = target.bounds("input");
    target.draw();
    target.click(input.center());
    assert!(target.draw().requests.ime.is_enabled());

    target
        .surface
        .queue_event(cosmix_iced_host::input::focus_event(false));
    target.surface.process();
    let frame = target.draw();
    assert_eq!(frame.requests.ime, ImeRequest::Disabled);
    assert_eq!(frame.requests.redraw, Redraw::Wait);

    target
        .surface
        .queue_event(cosmix_iced_host::input::focus_event(true));
    assert!(target.surface.process().needs_redraw);
    assert!(target.draw().requests.ime.is_enabled());
}

#[test]
fn overlay_menus_draw_outside_their_widget_and_take_input() {
    let mut target = Target::new(1.0, PixelFormat::Rgba8);
    let pick = target.bounds("pick");
    target.draw();
    let probe = Point::new(pick.center_x(), pick.y + pick.height + 8.0);
    let before = target.pixel(probe.x as u32, probe.y as u32);

    let update = target.click(pick.center());
    assert!(update.needs_redraw);
    let frame = target.draw();
    let bottom = ((pick.y + pick.height) * target.scale()) as u32;
    assert!(
        frame
            .damage
            .iter()
            .any(|rect| rect.y + rect.height > bottom + 8),
        "menu not drawn below the pick list: {:?}",
        frame.damage
    );
    assert_ne!(
        target.pixel(probe.x as u32, probe.y as u32),
        before,
        "menu pixels unchanged"
    );

    // The first option sits just under the pick list.
    let update = target.click(probe);
    assert_eq!(update.messages, 1, "{update:?}");
    assert_eq!(target.surface.program().choice, Some("alpha"));
    target.draw();
    assert_eq!(
        target.pixel(probe.x as u32, probe.y as u32),
        before,
        "menu not closed"
    );
}

#[test]
fn independent_surfaces_share_one_process() {
    let mut targets: Vec<Target> = (0..4)
        .map(|i| Target::new(1.0 + i as f32 * 0.5, PixelFormat::Argb8888))
        .collect();
    for target in &mut targets {
        assert!(target.draw().full);
    }
    let input = targets[0].bounds("input");
    targets[0].click(input.center());
    type_char(&mut targets[0].surface, "z");
    targets[0].surface.process();
    assert_eq!(targets[0].surface.program().value, "z");
    for target in &mut targets[1..] {
        assert!(target.surface.program().value.is_empty());
        assert!(!target.surface.process().needs_redraw);
        assert!(target.draw().damage.is_empty());
    }
}
