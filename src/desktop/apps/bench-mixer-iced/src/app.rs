//! Run state, messages and the view root.
//!
//! The state is exactly what a frame draws: nothing is recomputed in `view`
//! that a tick did not change, so a frame woken by input alone redraws the
//! same numbers. Everything that moves is written by [`Message::Tick`], which
//! the [`crate::ticker`] widget publishes once per feed tick and never when
//! the run is idle.

use cosmix_bench_feed::layout::Layout;
use cosmix_bench_feed::{BenchSong, MeterFrame, MixerFeed, Mode, RollViewport, roll_script};
use cosmix_iced_widgets::{RollNotes, RollView, Tokens};
use iced::widget::Stack;
use iced::{Element, Fill, Size, Subscription};

use crate::ticker::Ticker;
use crate::{Config, View, mixer, roll};

#[derive(Debug, Clone)]
pub enum Message {
    /// A new feed tick is showing.
    Tick(u64),
    /// A fader moved, in the widget scale's dB.
    Fader(usize, f32),
    Pan(usize, f32),
    Mute(usize, bool),
    Solo(usize, bool),
    /// A pointer gesture ended. The bake-off records no automation, so this
    /// only exists because the controls publish it.
    Release,
    /// The roll was scrolled or zoomed by hand (not in `--mode roll`, whose
    /// script owns the viewport).
    Roll(RollView, Size),
    /// The window's logical size, so the roll's beats-per-pixel follows the
    /// surface the compositor actually gave us rather than what was asked for.
    Resized(Size),
}

pub struct Bench {
    pub config: Config,
    pub tokens: Tokens,
    pub feed: MixerFeed,
    /// The tick the current state was built from.
    pub tick: u64,
    pub meters: Vec<MeterFrame>,
    /// Fader values in dB, one per slot, master last.
    pub faders: Vec<f32>,
    /// Pan, -1..=1, one per slot.
    pub pans: Vec<f32>,
    /// Strip names, resolved once. The feed hashes and formats a name on
    /// every `strip()` call, and a view that did that per frame would be
    /// measuring the feed instead of the toolkit.
    pub names: Vec<String>,
    pub mutes: Vec<bool>,
    pub solos: Vec<bool>,
    pub song: Option<BenchSong>,
    pub notes: RollNotes,
    /// One colour per track, the spread CTK's `channel_color` draws.
    pub track_colours: Vec<iced::Color>,
    pub roll: RollView,
    /// Reference canvas for `roll`; idle resizes must not repeatedly round
    /// its pixel scale or notes just beyond the right edge can leak in.
    roll_size: Size,
    /// The shared board geometry for the current size; the Bevy arm and the
    /// driver read the same rectangles from the same function.
    pub layout: Layout,
    /// Logical window size the layout and roll view were resolved for.
    size: Size,
}

impl Bench {
    pub fn new(config: Config, song: Option<BenchSong>, tokens: Tokens) -> Self {
        let feed = MixerFeed::new(config.seed, config.strips, config.mode);
        let slots = feed.slot_count();
        let size = Size::new(config.width as f32, config.height as f32);
        let notes = song
            .as_ref()
            .map_or_else(|| RollNotes::new(Vec::new()), roll::notes);
        let roll = song.as_ref().map_or_else(RollView::default, |song| {
            roll::roll_view(RollViewport::initial(song), song, size.width, size.height)
        });
        Self {
            tokens,
            feed,
            tick: 0,
            meters: vec![MeterFrame::default(); slots],
            faders: (0..slots).map(|slot| feed.fader_db(slot, 0)).collect(),
            pans: (0..slots).map(|slot| feed.strip(slot).pan).collect(),
            names: (0..slots).map(|slot| feed.strip(slot).name).collect(),
            mutes: (0..slots).map(|slot| feed.strip(slot).mute).collect(),
            solos: (0..slots).map(|slot| feed.strip(slot).solo).collect(),
            track_colours: crate::channel::track_palette(
                song.as_ref().map_or(1, |song| song.track_count),
                tokens.ring,
            ),
            song,
            notes,
            roll,
            roll_size: size,
            layout: Layout::new((size.width, size.height), config.strips),
            size,
            config,
        }
    }

    /// Whether this run's clock has to run at all.
    fn animated(&self) -> bool {
        self.config.mode.is_animated()
    }

    /// Rebuild the roll view for the current size; the script owns the
    /// viewport in roll mode, otherwise the last hand-driven view stands.
    fn remap_roll(&mut self) {
        let Some(song) = &self.song else { return };
        if self.config.mode != Mode::Roll {
            return;
        }
        self.roll = roll::roll_view(
            roll_script(song, self.tick),
            song,
            self.size.width,
            self.size.height,
        );
        self.roll_size = self.size;
    }

    /// Resolve the same slice against the canvas's actual layout size, even
    /// before the window resize subscription has delivered its message.
    pub fn roll_at_size(&self, size: Size) -> RollView {
        roll::resize_view(self.roll, self.roll_size, size)
    }

    pub fn update(&mut self, message: Message) {
        match message {
            Message::Tick(tick) => {
                if self.tick == tick {
                    return;
                }
                self.tick = tick;
                match self.config.view {
                    View::Mixer => {
                        if self.feed.mode().animates_meters() {
                            self.feed.meters_into(tick, &mut self.meters);
                        }
                        if self.config.scripted_drag
                            && let Some(drag) = self.feed.drag_sample(tick)
                        {
                            self.faders[drag.strip] = drag.db;
                        }
                    }
                    View::Roll => self.remap_roll(),
                }
            }
            Message::Fader(slot, db) => self.faders[slot] = db,
            Message::Pan(slot, pan) => self.pans[slot] = pan,
            Message::Release => {}
            Message::Mute(slot, on) => self.mutes[slot] = on,
            Message::Solo(slot, on) => self.solos[slot] = on,
            Message::Roll(view, size) => {
                if self.config.mode != Mode::Roll {
                    self.roll = view;
                    self.roll_size = size;
                }
            }
            Message::Resized(size) => {
                if self.size != size && size.width > 0.0 && size.height > 0.0 {
                    self.size = size;
                    self.layout = Layout::new((size.width, size.height), self.config.strips);
                    self.remap_roll();
                }
            }
        }
    }

    pub fn view(&self) -> Element<'_, Message> {
        let body = match self.config.view {
            View::Mixer => mixer::view(self, &self.layout, self.tokens),
            View::Roll => roll::view(self, self.tokens),
        };
        // The clock is zero-sized and stacked over the surface, so it costs
        // the layout nothing and cannot shift a strip by a pixel.
        // `stack!` / `with_children` / `push` discard void size hints in
        // iced 0.14. `from_vec` retains the clock so it receives redraws.
        Stack::from_vec(vec![
            body,
            Ticker::new(self.tick, self.animated(), Message::Tick).into(),
        ])
        .width(Fill)
        .height(Fill)
        .into()
    }

    pub fn subscription(&self) -> Subscription<Message> {
        iced::window::resize_events().map(|(_, size)| Message::Resized(size))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cosmix_bench_feed::{DEFAULT_SEED, Mode};

    fn config(mode: Mode, view: View, strips: usize) -> Config {
        Config {
            mode,
            view,
            strips,
            seed: DEFAULT_SEED,
            song: None,
            width: 1024,
            height: 576,
            scripted_drag: true,
        }
    }

    fn bench(mode: Mode, strips: usize) -> Bench {
        Bench::new(
            config(mode, View::Mixer, strips),
            None,
            crate::theme::tokens().unwrap(),
        )
    }

    #[test]
    fn idle_roll_preserves_its_slice_through_layout_and_resize() {
        let mut source = cosmix_song::Song::new("resize-test");
        let mut track = cosmix_song::Track::new("notes", 0);
        for step in 0..128 {
            track.add_note(cosmix_song::Note::new(
                40 + (step % 30) as u8,
                100,
                step * 240,
                200,
            ));
        }
        source.add_track(track);
        let song = BenchSong::from_song(&source);
        let initial = RollViewport::initial(&song);
        let mut visible = Vec::new();
        initial.visible_notes_into(&song, &mut visible);
        let mut bench = Bench::new(
            config(Mode::Idle, View::Roll, 64),
            Some(song.clone()),
            crate::theme::tokens().unwrap(),
        );
        for size in [
            Size::new(795.2, 420.0),
            Size::new(1024.0, 576.0),
            Size::new(900.0, 500.0),
            Size::new(795.2, 420.0),
            Size::new(1024.0, 576.0),
        ] {
            let expected = roll::roll_view(initial, &song, size.width, size.height);
            // Layout may precede the resize message; both paths must agree.
            for actual in [bench.roll_at_size(size), {
                bench.update(Message::Resized(size));
                bench.roll_at_size(size)
            }] {
                assert!((actual.row_height - expected.row_height).abs() < 1e-4);
                assert!((actual.scroll_y - expected.scroll_y).abs() < 1e-3);
                assert!((actual.pixels_per_beat - expected.pixels_per_beat).abs() < 1e-4);
                assert_eq!(actual.scroll_beats, expected.scroll_beats);
                assert_eq!(actual.beat_at(size.width), expected.beat_at(size.width),
                    "even sub-pixel drift can admit the notes at the excluded right edge");
                assert_eq!(bench.notes.visible(actual.scroll_beats, actual.beat_at(size.width)).count(),
                    visible.len(), "resize must preserve the half-open note set");
            }
        }
        let size = Size::new(795.0, 420.0);
        let manual =
            bench
                .roll_at_size(size)
                .zoomed(2.0, 200.0)
                .scrolled(30.0, 20.0, size.height, 64.0);
        bench.update(Message::Roll(manual, size));
        bench.update(Message::Resized(size));
        let actual = bench.roll_at_size(size);
        assert!((actual.scroll_beats - manual.scroll_beats).abs() < 1e-4);
        assert!((actual.scroll_y - manual.scroll_y).abs() < 1e-3);
        assert!((actual.row_height - manual.row_height).abs() < 1e-4);
        assert!((actual.pixels_per_beat - manual.pixels_per_beat).abs() < 1e-4);
    }

    #[test]
    fn both_views_mount_the_clock_even_though_it_has_no_area() {
        for (mode, view) in [
            (Mode::Idle, View::Mixer),
            (Mode::Meters, View::Mixer),
            (Mode::Drag, View::Mixer),
            (Mode::Idle, View::Roll),
            (Mode::Roll, View::Roll),
        ] {
            let bench = Bench::new(config(mode, view, 4), None, crate::theme::tokens().unwrap());
            assert_eq!(bench.animated(), mode != Mode::Idle);
            let view = bench.view();
            let tree = iced::advanced::widget::Tree::new(view.as_widget());
            assert_eq!(
                tree.children.len(),
                2,
                "body and clock must both be mounted"
            );
            let ticker: Element<'_, Message> =
                Ticker::new(bench.tick, bench.animated(), Message::Tick).into();
            assert_eq!(tree.children[1].tag, ticker.as_widget().tag());
            assert!(ticker.as_widget().size().is_void());
        }
    }

    #[test]
    fn the_board_starts_on_the_feeds_state() {
        let bench = bench(Mode::Idle, 64);
        assert_eq!(bench.faders.len(), 65);
        assert_eq!(bench.meters.len(), 65);
        for slot in 0..65 {
            let state = bench.feed.strip(slot);
            assert!((bench.faders[slot] - state.fader_db).abs() < 1e-6, "{slot}");
            assert_eq!(bench.mutes[slot], state.mute);
            assert_eq!(bench.solos[slot], state.solo);
        }
        assert!(bench.mutes.iter().any(|on| *on));
        assert!(bench.solos.iter().any(|on| *on));
        assert!(!bench.animated());
    }

    #[test]
    fn idle_ticks_never_move_a_meter() {
        let mut bench = bench(Mode::Idle, 8);
        for tick in [1, 2, 50] {
            bench.update(Message::Tick(tick));
            assert!(
                bench.meters.iter().all(|f| *f == MeterFrame::default()),
                "tick {tick}"
            );
        }
    }

    #[test]
    fn animated_ticks_follow_the_feed() {
        let mut bench = bench(Mode::Meters, 8);
        assert!(bench.animated());
        for tick in [1, 7, 31] {
            bench.update(Message::Tick(tick));
            for slot in 0..bench.feed.slot_count() {
                assert_eq!(
                    bench.meters[slot],
                    bench.feed.meter(slot, tick),
                    "slot {slot} tick {tick}"
                );
            }
        }
    }

    #[test]
    fn the_scripted_drag_moves_one_fader() {
        let mut bench = bench(Mode::Drag, 8);
        let strip = bench.feed.drag_strip().unwrap();
        let untouched = bench.feed.strip(0).fader_db;
        for tick in [5, 20, 45, 60] {
            bench.update(Message::Tick(tick));
            let expected = bench.feed.drag_sample(tick).unwrap().db;
            assert_eq!(bench.faders[strip], expected, "tick {tick}");
            assert!((bench.faders[0] - untouched).abs() < 1e-6);
        }
    }

    #[test]
    fn a_pointer_drag_moves_the_fader_it_is_on() {
        let mut config = config(Mode::Drag, View::Mixer, 8);
        config.scripted_drag = false;
        let mut bench = Bench::new(config, None, crate::theme::tokens().unwrap());
        let strip = bench.feed.drag_strip().unwrap();
        let before = bench.faders[strip];
        // A tick must not overwrite a pointer-driven fader in this mode.
        bench.update(Message::Tick(20));
        assert_eq!(bench.faders[strip], before);
        bench.update(Message::Fader(strip, -14.5));
        assert_eq!(bench.faders[strip], -14.5);
        bench.update(Message::Tick(21));
        assert_eq!(bench.faders[strip], -14.5, "the tick must not undo it");
        // Meters still animate around it.
        assert_eq!(bench.meters[0], bench.feed.meter(0, 21));
    }

    #[test]
    fn toggles_are_controlled_and_a_repeated_tick_is_a_no_op() {
        let mut bench = bench(Mode::Meters, 4);
        bench.update(Message::Tick(9));
        let before = bench.meters.clone();
        bench.update(Message::Mute(1, true));
        bench.update(Message::Solo(2, true));
        bench.update(Message::Pan(0, 0.5));
        bench.update(Message::Release);
        assert!(bench.mutes[1] && bench.solos[2]);
        assert_eq!(
            bench.pans[0], 0.5,
            "controls are controlled: store and echo"
        );
        bench.update(Message::Tick(9));
        assert_eq!(bench.meters, before);
    }
}
