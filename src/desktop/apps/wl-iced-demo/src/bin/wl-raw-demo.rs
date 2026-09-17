//! `wl-raw-demo`: cosmix-wl-app with a hand-drawn grid and popup menu.
//!
//! `WL_DEMO_TRACE=1` logs one line per notable event. `WL_DEMO_EXIT_AFTER=N`
//! exits after N seconds (a sleeping helper thread, not a loop timer).

use cosmix_wl_app::App;
use cosmix_wl_iced_demo::raw::{RawDemo, WAKE_EXIT};
use cosmix_wl_iced_demo::startup::Clock;
use std::time::Duration;

struct Runner {
    demo: RawDemo,
    exit_after: Option<Duration>,
}

impl cosmix_wl_app::App for Runner {
    fn init(&mut self, cx: &mut cosmix_wl_app::Ctx<'_>) {
        if let Some(after) = self.exit_after {
            let waker = cx.waker();
            std::thread::spawn(move || {
                std::thread::sleep(after);
                waker.wake(WAKE_EXIT);
            });
        }
        self.demo.init(cx);
    }

    fn event(&mut self, cx: &mut cosmix_wl_app::Ctx<'_>, event: cosmix_wl_app::Event) {
        self.demo.event(cx, event);
    }

    fn draw(&mut self, cx: &mut cosmix_wl_app::Ctx<'_>, frame: &mut cosmix_wl_app::Frame<'_>) {
        self.demo.draw(cx, frame);
    }
}

fn main() {
    let clock = Clock::start();
    let exit_after = std::env::var("WL_DEMO_EXIT_AFTER")
        .ok()
        .and_then(|v| v.parse::<f64>().ok())
        .map(Duration::from_secs_f64);
    let runner = Runner {
        demo: RawDemo::new(clock),
        exit_after,
    };
    match cosmix_wl_app::run(runner) {
        Ok(stats) => {
            let startup = stats
                .first_commit
                .map(|t| format!("{:.1}", clock.startup_ms(t)))
                .unwrap_or_else(|| "none".into());
            eprintln!(
                "wl-raw-demo: exit frames_committed={} buffer_allocations={} startup_ms={startup}",
                stats.frames_committed, stats.buffer_allocations
            );
        }
        Err(e) => {
            eprintln!("wl-raw-demo: {e}");
            std::process::exit(1);
        }
    }
}
