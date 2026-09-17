//! Damage granularity and correctness on a chrome-like surface: a caret
//! blink and a one-character edit damage only what changed, and after many
//! small random changes the incrementally drawn buffer equals a full redraw.

use cosmix_iced_host::core::keyboard::{self, Key, Modifiers, key::Named, key::Physical};
use cosmix_iced_host::core::mouse::{self, Button};
use cosmix_iced_host::core::widget::{Id, Operation};
use cosmix_iced_host::core::{Color, Event, Font, Length, Point, Rectangle, SmolStr};
use cosmix_iced_host::widget::{button, column, container, row, space, text, text_input};
use cosmix_iced_host::{
    DamageRect, Element, Frame, ImeRequest, PixelFormat, Program, Settings, Surface,
};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

static FONT: &[u8] = include_bytes!("../../cosmix-comp/assets/fonts/DejaVuSans.ttf");
const WIDTH: f32 = 795.0;
const HEIGHT: f32 = 120.0;

#[derive(Default)]
struct Chrome {
    active: usize,
    search: String,
}

#[derive(Debug, Clone)]
enum Msg {
    Tab(usize),
    Search(String),
}

impl Program for Chrome {
    type Message = Msg;

    fn update(&mut self, message: Msg) {
        match message {
            Msg::Tab(i) => self.active = i,
            Msg::Search(s) => self.search = s,
        }
    }

    fn view(&self) -> Element<'_, Msg> {
        let mut bar = row![].spacing(8.0).padding(4.0);
        for title in ["File", "Edit", "View"] {
            bar = bar.push(text(title).size(14.0));
        }
        let bar = bar.push(space().width(Length::Fill)).push(
            container(
                text_input("Search", &self.search)
                    .on_input(Msg::Search)
                    .width(220.0)
                    .size(14.0),
            )
            .id("search"),
        );
        let mut tabs = row![].spacing(2.0).padding(4.0);
        for i in 0..3 {
            let label = if i == self.active {
                format!("[Tab {}]", i + 1)
            } else {
                format!("Tab {}", i + 1)
            };
            tabs = tabs.push(
                container(
                    button(text(label).size(14.0))
                        .on_press(Msg::Tab(i))
                        .width(120.0),
                )
                .id(["tab0", "tab1", "tab2"][i]),
            );
        }
        column![
            bar,
            tabs,
            text(format!("{} characters searched", self.search.len())).size(14.0)
        ]
        .into()
    }
}

struct Target {
    surface: Surface<Chrome>,
    buffer: Vec<u8>,
    width: u32,
    height: u32,
    now: Instant,
}

impl Target {
    fn new(scale: f32) -> Self {
        cosmix_iced_host::load_font(FONT);
        let width = (WIDTH * scale).round() as u32;
        let height = (HEIGHT * scale).round() as u32;
        let surface = Surface::new(
            Chrome::default(),
            Settings {
                physical_size: (width, height).into(),
                scale_factor: scale,
                default_font: Font::with_name("DejaVu Sans"),
                background: Some(Color::from_rgb8(30, 32, 38)),
                ..Settings::default()
            },
        );
        Self {
            surface,
            buffer: vec![0; width as usize * height as usize * 4],
            width,
            height,
            now: Instant::now(),
        }
    }

    fn draw(&mut self) -> Frame {
        self.surface
            .draw_at(
                &mut self.buffer,
                self.width,
                self.height,
                self.width * 4,
                PixelFormat::Argb8888,
                self.now,
            )
            .expect("draw")
    }

    /// A full repaint of the current state at the same instant.
    fn full_redraw(&mut self) -> Vec<u8> {
        let mut fresh = vec![0; self.buffer.len()];
        self.surface.invalidate();
        let frame = self
            .surface
            .draw_at(
                &mut fresh,
                self.width,
                self.height,
                self.width * 4,
                PixelFormat::Argb8888,
                self.now,
            )
            .expect("full draw");
        assert!(frame.full);
        fresh
    }

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
            self.surface.scale_factor(),
            self.width,
            self.height,
        )
        .unwrap()
    }

    fn click(&mut self, at: Point) {
        self.surface.cursor_moved(at);
        self.surface
            .queue_event(Event::Mouse(mouse::Event::ButtonPressed(Button::Left)));
        self.surface
            .queue_event(Event::Mouse(mouse::Event::ButtonReleased(Button::Left)));
        self.surface.process();
    }

    fn key(&mut self, key: Key, text: Option<&str>) {
        self.surface
            .queue_event(Event::Keyboard(keyboard::Event::KeyPressed {
                key: key.clone(),
                modified_key: key,
                physical_key: Physical::Unidentified(keyboard::key::NativeCode::Unidentified),
                location: keyboard::Location::Standard,
                modifiers: Modifiers::empty(),
                text: text.map(SmolStr::new),
                repeat: false,
            }));
        self.surface.process();
    }

    fn area(&self) -> u64 {
        u64::from(self.width) * u64::from(self.height)
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

fn damaged_area(frame: &Frame) -> u64 {
    frame.damage.iter().map(DamageRect::area).sum()
}

#[derive(Debug, Default)]
struct Check {
    /// Pixels whose full-redraw colour changed since the previous frame.
    changed: usize,
    /// Pixels where the incremental buffer is off by one channel level
    /// (tiny-skia's masked and unmasked pipelines round antialiased edges
    /// differently; a quad cut by a damage rectangle is drawn masked).
    rounding: usize,
}

/// Damage correctness for one frame, against full redraws of this frame and
/// the previous one:
/// - every pixel whose true colour changed lies inside the damage (too
///   little damage is a rendering bug);
/// - the incremental buffer equals the full redraw everywhere, except for
///   one-level rounding differences.
fn check(
    t: &Target,
    frame: &Frame,
    incremental: &[u8],
    full: &[u8],
    previous_full: &[u8],
) -> Result<Check, String> {
    let mut out = Check::default();
    for (i, ((inc, now), before)) in incremental
        .chunks_exact(4)
        .zip(full.chunks_exact(4))
        .zip(previous_full.chunks_exact(4))
        .enumerate()
    {
        let (x, y) = (i as u32 % t.width, i as u32 / t.width);
        if now != before {
            out.changed += 1;
            if !frame.full && !frame.damage.iter().any(|r| r.contains(x, y)) {
                return Err(format!(
                    "pixel ({x}, {y}) changed {before:?} -> {now:?} outside damage {:?}",
                    frame.damage
                ));
            }
        }
        if inc != now {
            let delta = inc
                .iter()
                .zip(now)
                .map(|(a, b)| a.abs_diff(*b))
                .max()
                .unwrap_or(0);
            if delta > 1 {
                return Err(format!(
                    "pixel ({x}, {y}) is {inc:?}, full redraw {now:?} (damage {:?})",
                    frame.damage
                ));
            }
            out.rounding += 1;
        }
    }
    Ok(out)
}

#[test]
fn caret_blink_damages_about_the_caret() {
    for scale in [1.0, 2.5] {
        let mut t = Target::new(scale);
        t.draw();
        let search = t.bounds("search");
        t.click(search.center());
        t.now = Instant::now();
        let frame = t.draw();
        let mut previous = t.full_redraw();
        let ImeRequest::Enabled {
            physical_cursor, ..
        } = frame.requests.ime
        else {
            panic!("field not focused");
        };
        for step in 1..=4u64 {
            t.now += Duration::from_millis(550);
            let frame = t.draw();
            eprintln!(
                "DAMAGE_REPORT scale={scale} blink={step} surface={}x{} damage={:?}",
                t.width, t.height, frame.damage
            );
            assert!(!frame.full, "blink repainted everything");
            assert!(!frame.damage.is_empty(), "blink {step} damaged nothing");
            let area = damaged_area(&frame);
            assert!(
                area * 50 <= t.area(),
                "blink damage {area} px is over 2 % of {} px: {:?}",
                t.area(),
                frame.damage
            );
            let caret_y = physical_cursor.y + physical_cursor.height / 2;
            assert!(
                frame
                    .damage
                    .iter()
                    .any(|r| r.contains(physical_cursor.x, caret_y)),
                "caret at {physical_cursor:?} not in {:?}",
                frame.damage
            );
            let incremental = t.buffer.clone();
            let full = t.full_redraw();
            let c = check(&t, &frame, &incremental, &full, &previous)
                .unwrap_or_else(|e| panic!("blink {step}: {e}"));
            assert!(c.changed > 0, "blink {step} changed no pixels");
            eprintln!(
                "DAMAGE_REPORT scale={scale} blink={step} changed={} rounding={}",
                c.changed, c.rounding
            );
            previous = full;
        }
    }
}

#[test]
fn one_character_edit_damages_the_field_and_its_echo() {
    let mut t = Target::new(2.5);
    t.draw();
    let search = t.bounds("search");
    t.click(search.center());
    t.draw();
    let previous = t.full_redraw();
    t.key(Key::Character("x".into()), Some("x"));
    let frame = t.draw();
    eprintln!("DAMAGE_REPORT edit damage={:?}", frame.damage);
    assert!(!frame.full);
    // The field, plus the "N characters searched" line that echoes it.
    let field = t.physical(search, 2.0);
    let echo_row = t.physical(
        Rectangle::new(
            Point::new(0.0, 60.0),
            cosmix_iced_host::core::Size::new(WIDTH, 60.0),
        ),
        0.0,
    );
    for rect in &frame.damage {
        assert!(
            rect.is_within(&field) || rect.is_within(&echo_row),
            "edit damage {rect:?} outside field {field:?} and echo {echo_row:?}"
        );
    }
    assert!(frame.damage.iter().any(|r| r.is_within(&field)));
    let incremental = t.buffer.clone();
    let full = t.full_redraw();
    let c =
        check(&t, &frame, &incremental, &full, &previous).unwrap_or_else(|e| panic!("edit: {e}"));
    eprintln!(
        "DAMAGE_REPORT edit changed={} rounding={}",
        c.changed, c.rounding
    );
}

/// Deterministic pseudo-random sequence (xorshift).
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

#[test]
fn random_small_edits_match_a_full_redraw() {
    for (scale, seed) in [(1.0, 7), (1.25, 99), (2.5, 2024)] {
        let mut t = Target::new(scale);
        let mut rng = Rng(seed);
        t.draw();
        let search = t.bounds("search");
        let tabs = [t.bounds("tab0"), t.bounds("tab1"), t.bounds("tab2")];
        let mut previous = t.full_redraw();
        let (mut partial_frames, mut exact_frames, mut rounding_pixels) = (0, 0, 0);
        for step in 0..80 {
            let op = rng.below(8);
            match op {
                0 | 1 => {
                    let c = char::from(b'a' + rng.below(26) as u8).to_string();
                    t.key(Key::Character(c.as_str().into()), Some(&c));
                }
                2 => t.key(Key::Named(Named::Backspace), None),
                3 => t.key(
                    Key::Named(if rng.below(2) == 0 {
                        Named::ArrowLeft
                    } else {
                        Named::ArrowRight
                    }),
                    None,
                ),
                4 => t.click(tabs[rng.below(3) as usize].center()),
                5 => t.click(search.center()),
                6 => {
                    let at = Point::new(
                        rng.below(WIDTH as u64) as f32,
                        rng.below(HEIGHT as u64) as f32,
                    );
                    t.surface.cursor_moved(at);
                    t.surface.process();
                }
                _ => {}
            }
            t.now += Duration::from_millis(rng.below(700));
            let frame = t.draw();
            if !frame.full && !frame.damage.is_empty() {
                partial_frames += 1;
            }
            let incremental = t.buffer.clone();
            let full = t.full_redraw();
            let c = check(&t, &frame, &incremental, &full, &previous)
                .unwrap_or_else(|e| panic!("scale {scale} step {step} op {op}: {e}"));
            if c.rounding == 0 {
                exact_frames += 1;
            }
            rounding_pixels += c.rounding;
            // Continue from the exact image so rounding cannot accumulate.
            t.buffer.copy_from_slice(&full);
            previous = full;
        }
        eprintln!(
            "DAMAGE_REPORT random scale={scale} partial_frames={partial_frames}/80 \
             byte_exact_frames={exact_frames}/80 rounding_pixels={rounding_pixels}"
        );
        assert!(
            partial_frames > 20,
            "too few partial frames to mean anything"
        );
    }
}
