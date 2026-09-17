//! Shadows are painted without the clip mask, so the damage has to cover
//! them however it was grown: by coalescing, by a bounding box, or by an
//! older buffer's history. These cases drive stacked shadowed cards with a
//! caret blinking in the first one and compare every frame with a full
//! redraw, byte for byte, in both pixel formats.

use cosmix_iced_host::core::mouse::{self, Button};
use cosmix_iced_host::core::{Color, Event, Font, Length, Point, Shadow, Vector};
use cosmix_iced_host::widget::{column, container, text, text_input};
use cosmix_iced_host::{DamageRect, Element, Frame, PixelFormat, Program, Settings, Surface, core};
use std::time::{Duration, Instant};

static FONT: &[u8] = include_bytes!("../../cosmix-comp/assets/fonts/DejaVuSans.ttf");
const WIDTH: u32 = 320;
const CARD: f32 = 34.0;
/// Cards are further apart than this gap once shadows are counted, so the
/// damage only reaches a neighbour after it has been grown.
const GAP: f32 = 20.0;

struct Cards {
    count: usize,
    value: String,
}

#[derive(Debug, Clone)]
enum Msg {
    Input(String),
}

impl Program for Cards {
    type Message = Msg;

    fn update(&mut self, message: Msg) {
        let Msg::Input(value) = message;
        self.value = value;
    }

    fn view(&self) -> Element<'_, Msg> {
        let mut stack = column![].spacing(GAP).padding(10.0);
        for i in 0..self.count {
            let content: Element<'_, Msg> = if i == 0 {
                text_input("type", &self.value)
                    .on_input(Msg::Input)
                    .size(14.0)
                    .into()
            } else {
                text(format!("card {i}")).size(14.0).into()
            };
            stack = stack.push(
                container(content)
                    .width(Length::Fill)
                    .height(CARD)
                    .padding(4.0)
                    .style(|_| container::Style {
                        background: Some(Color::from_rgb8(60, 62, 70).into()),
                        border: core::Border::default().rounded(6.0),
                        shadow: Shadow {
                            color: Color::from_rgba8(0, 0, 0, 0.8),
                            offset: Vector::new(2.0, 3.0),
                            blur_radius: 10.0,
                        },
                        ..container::Style::default()
                    }),
            );
        }
        stack.into()
    }
}

struct Target {
    surface: Surface<Cards>,
    buffer: Vec<u8>,
    width: u32,
    height: u32,
    format: PixelFormat,
    now: Instant,
}

impl Target {
    fn new(count: usize, scale: f32, format: PixelFormat) -> Self {
        cosmix_iced_host::load_font(FONT);
        let logical_height = 20.0 + count as f32 * (CARD + GAP);
        let width = (WIDTH as f32 * scale).round() as u32;
        let height = (logical_height * scale).round() as u32;
        let surface = Surface::new(
            Cards {
                count,
                value: String::new(),
            },
            Settings {
                physical_size: (width, height).into(),
                scale_factor: scale,
                default_font: Font::with_name("DejaVu Sans"),
                background: Some(Color::from_rgb8(20, 22, 26)),
                ..Settings::default()
            },
        );
        Self {
            surface,
            buffer: vec![0; width as usize * height as usize * 4],
            width,
            height,
            format,
            now: Instant::now(),
        }
    }

    fn draw(&mut self) -> Frame {
        self.draw_into_aged(1)
    }

    fn draw_into_aged(&mut self, age: u32) -> Frame {
        let mut buffer = std::mem::take(&mut self.buffer);
        let frame = self
            .surface
            .draw_aged_at(
                &mut buffer,
                self.width,
                self.height,
                self.width * 4,
                self.format,
                self.now,
                age,
            )
            .expect("draw");
        self.buffer = buffer;
        frame
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
                self.format,
                self.now,
            )
            .expect("full draw");
        assert!(frame.full);
        fresh
    }

    fn focus_first_card(&mut self) {
        let at = Point::new(60.0, 20.0);
        self.surface.cursor_moved(at);
        self.surface
            .queue_event(Event::Mouse(mouse::Event::ButtonPressed(Button::Left)));
        self.surface
            .queue_event(Event::Mouse(mouse::Event::ButtonReleased(Button::Left)));
        self.surface.process();
        self.draw();
        assert!(
            self.surface.requests().ime.is_enabled(),
            "the first card's field did not take focus"
        );
    }
}

fn differences(a: &[u8], b: &[u8], width: u32) -> Option<String> {
    let count = a
        .chunks_exact(4)
        .zip(b.chunks_exact(4))
        .filter(|(x, y)| x != y)
        .count();
    if count == 0 {
        return None;
    }
    let first = a.iter().zip(b).position(|(x, y)| x != y).unwrap() / 4;
    let o = first * 4;
    Some(format!(
        "{count} pixels differ; first at ({}, {}): {:?} vs {:?}",
        first as u32 % width,
        first as u32 / width,
        &a[o..o + 4],
        &b[o..o + 4]
    ))
}

fn area(frame: &Frame) -> u64 {
    frame.damage.iter().map(DamageRect::area).sum()
}

#[test]
fn neighbouring_shadows_are_never_painted_outside_the_damage() {
    for format in [PixelFormat::Argb8888, PixelFormat::Rgba8] {
        let mut t = Target::new(2, 2.5, format);
        t.draw();
        t.focus_first_card();
        t.now = Instant::now();
        t.draw();
        for blink in 1..=4u64 {
            t.now += Duration::from_millis(550);
            let frame = t.draw();
            assert!(!frame.full, "{format:?} blink {blink} repainted everything");
            let incremental = t.buffer.clone();
            let full = t.full_redraw();
            if let Some(diff) = differences(&incremental, &full, t.width) {
                panic!(
                    "{format:?} blink {blink}: {diff}; damage was {:?}",
                    frame.damage
                );
            }
            eprintln!(
                "SHADOW_REPORT two-cards {format:?} blink={blink} rects={} area={}",
                frame.damage.len(),
                area(&frame)
            );
            t.buffer.copy_from_slice(&full);
        }
    }
}

#[test]
fn a_chain_of_shadowed_cards_expands_all_the_way() {
    let mut t = Target::new(12, 1.0, PixelFormat::Argb8888);
    t.draw();
    t.focus_first_card();
    t.now = Instant::now();
    t.draw();
    for blink in 1..=3u64 {
        t.now += Duration::from_millis(550);
        let frame = t.draw();
        let incremental = t.buffer.clone();
        let full = t.full_redraw();
        if let Some(diff) = differences(&incremental, &full, t.width) {
            panic!("chain blink {blink}: {diff}; damage was {:?}", frame.damage);
        }
        eprintln!(
            "SHADOW_REPORT chain blink={blink} full={} rects={} area={} of {}",
            frame.full,
            frame.damage.len(),
            area(&frame),
            u64::from(t.width) * u64::from(t.height)
        );
        t.buffer.copy_from_slice(&full);
    }
}

#[test]
fn older_buffers_with_shadows_catch_up_exactly() {
    for format in [PixelFormat::Argb8888, PixelFormat::Rgba8] {
        // One surface cycles three buffers; a second one, always fully
        // repainted, is the reference. Both see the same events and times,
        // so their state matches frame for frame. (A full redraw on the
        // first surface itself would clear the damage history the ages
        // depend on.)
        let mut t = Target::new(3, 1.0, format);
        let mut reference = Target::new(3, 1.0, format);
        for target in [&mut t, &mut reference] {
            target.draw();
            target.focus_first_card();
        }
        let start = Instant::now();
        let mut buffers = [t.buffer.clone(), t.buffer.clone(), t.buffer.clone()];
        let mut drawn_at: [Option<u64>; 3] = [Some(0), None, None];
        for index in 1..=9u64 {
            let now = start + Duration::from_millis(550 * index);
            t.now = now;
            reference.now = now;
            let slot = index as usize % 3;
            let age = drawn_at[slot].map_or(0, |at| (index - at) as u32);
            t.buffer = std::mem::take(&mut buffers[slot]);
            let frame = t.draw_into_aged(age);
            reference.surface.invalidate();
            reference.draw();
            if let Some(diff) = differences(&t.buffer, &reference.buffer, t.width) {
                panic!(
                    "{format:?} frame {index} age {age}: {diff}; damage was {:?}",
                    frame.damage
                );
            }
            eprintln!(
                "SHADOW_REPORT aged {format:?} frame={index} age={age} full={} rects={} area={}",
                frame.full,
                frame.damage.len(),
                area(&frame)
            );
            buffers[slot] = std::mem::take(&mut t.buffer);
            drawn_at[slot] = Some(index);
            t.buffer = vec![0; buffers[slot].len()];
        }
        assert!(
            drawn_at.iter().all(Option::is_some),
            "every buffer should have been drawn into"
        );
    }
}
