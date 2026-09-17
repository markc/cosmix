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
use iced::widget::stack;
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
    Roll(RollView),
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
    pub roll: RollView,
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
            song,
            notes,
            roll,
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
            Message::Roll(view) => {
                if self.config.mode != Mode::Roll {
                    self.roll = view;
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
        stack![body, Ticker::new(self.tick, self.animated(), Message::Tick)]
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
        assert_eq!(bench.pans[0], 0.5, "controls are controlled: store and echo");
        bench.update(Message::Tick(9));
        assert_eq!(bench.meters, before);
    }
}
