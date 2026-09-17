//! `cosmix-bench-layout` — print the bake-off mixer layout as JSON.
//!
//! The Mix driver reads this instead of re-deriving the wrap rule and the
//! widget geometry, so it aims injected input at the rectangles the arms
//! actually drew. Same `Layout::new` both arms call.

use cosmix_bench_feed::layout::{FADER_TRAVEL_INSET, Layout, StripLayout};
use cosmix_bench_feed::{DEFAULT_SEED, DEFAULT_STRIPS, layout};

const USAGE: &str = "\
usage: cosmix-bench-layout [options]
  --size WxH     window size in logical pixels (default 1024x576)
  --strips N     channel strips, plus the master (default 64)
  --seed S       feed seed, echoed for provenance (default 0x5eed1234)
  --pretty       indent the JSON";

/// The printed document: the layout, plus the run's provenance and the
/// master strip called out by name.
#[derive(serde::Serialize)]
struct Report {
    seed: u64,
    fader_travel_inset: f32,
    #[serde(flatten)]
    layout: Layout,
    master: StripLayout,
}

fn parse_size(text: &str) -> Result<(f32, f32), String> {
    let bad = || format!("--size must be WxH, e.g. 1024x576, got {text:?}");
    let (width, height) = text.split_once(['x', 'X']).ok_or_else(bad)?;
    let parse = |value: &str| {
        value
            .parse::<u32>()
            .ok()
            .filter(|n| (200..=8000).contains(n))
            .ok_or_else(bad)
            .map(|n| n as f32)
    };
    Ok((parse(width)?, parse(height)?))
}

fn parse_seed(text: &str) -> Result<u64, String> {
    match text.strip_prefix("0x").or_else(|| text.strip_prefix("0X")) {
        Some(hex) => u64::from_str_radix(&hex.replace('_', ""), 16),
        None => text.replace('_', "").parse(),
    }
    .map_err(|_| format!("--seed: not a number: {text:?}"))
}

fn run(args: &[String]) -> Result<String, String> {
    let mut size = (layout::WINDOW_WIDTH as f32, layout::WINDOW_HEIGHT as f32);
    let mut strips = DEFAULT_STRIPS;
    let mut seed = DEFAULT_SEED;
    let mut pretty = false;
    let mut iter = args.iter();
    while let Some(flag) = iter.next() {
        match flag.as_str() {
            "--pretty" => {
                pretty = true;
                continue;
            }
            "--help" | "-h" => return Err(USAGE.to_owned()),
            _ => {}
        }
        let value = iter
            .next()
            .ok_or_else(|| format!("{flag} needs a value\n{USAGE}"))?;
        match flag.as_str() {
            "--size" => size = parse_size(value)?,
            "--strips" => {
                strips = value
                    .parse()
                    .ok()
                    .filter(|n| (1..=512).contains(n))
                    .ok_or_else(|| format!("--strips must be 1..=512, got {value:?}"))?;
            }
            "--seed" => seed = parse_seed(value)?,
            other => return Err(format!("unknown option {other:?}\n{USAGE}")),
        }
    }
    let layout = Layout::new(size, strips);
    let report = Report {
        seed,
        fader_travel_inset: FADER_TRAVEL_INSET,
        master: *layout.master(),
        layout,
    };
    let json = if pretty {
        serde_json::to_string_pretty(&report)
    } else {
        serde_json::to_string(&report)
    };
    json.map_err(|error| format!("serialising the layout: {error}"))
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(json) => println!("{json}"),
        Err(message) => {
            eprintln!("{message}");
            std::process::exit(2);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn prints_the_same_layout_the_arms_lay_out() {
        let json = run(&args(&["--size", "1024x576", "--strips", "64"])).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["strips_per_row"], 22);
        assert_eq!(value["rows"], 3);
        assert_eq!(value["slots"].as_array().unwrap().len(), 65);
        assert_eq!(value["master"], value["slots"][64]);
        assert_eq!(value["master"]["is_master"], true);
        assert_eq!(value["fader_travel_inset"], FADER_TRAVEL_INSET);
        assert_eq!(value["seed"], DEFAULT_SEED);
        // Every documented rect key is present on a channel strip.
        let strip = &value["slots"][0];
        for key in ["rect", "name", "knob", "meter", "fader", "mute", "solo"] {
            assert!(strip[key].is_object(), "slot 0 has no {key}");
            for axis in ["x", "y", "w", "h"] {
                assert!(strip[key][axis].is_number(), "{key}.{axis}");
            }
        }
        // The master drops the knob and solo.
        assert!(value["master"]["knob"].is_null() && value["master"]["solo"].is_null());
    }

    #[test]
    fn json_round_trips_the_layout() {
        let expected = Layout::new((1600.0, 900.0), 48);
        let json = run(&args(&["--size", "1600x900", "--strips", "48"])).unwrap();
        #[derive(serde::Deserialize)]
        struct Parsed {
            seed: u64,
            #[serde(flatten)]
            layout: Layout,
            master: StripLayout,
        }
        let parsed: Parsed = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.layout, expected);
        assert_eq!(parsed.master, *expected.master());
        assert_eq!(parsed.seed, DEFAULT_SEED);
    }

    #[test]
    fn options_and_errors() {
        let json = run(&args(&["--seed", "0x11", "--strips", "8", "--pretty"])).unwrap();
        assert!(json.contains('\n'), "--pretty indents");
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["seed"], 17);
        assert_eq!(value["strips"], 8);
        for bad in [
            &["--size", "1024"][..],
            &["--size", "10x10"],
            &["--strips", "0"],
            &["--seed", "0xzz"],
            &["--size"],
            &["--nope", "1"],
            &["--help"],
        ] {
            assert!(run(&args(bad)).is_err(), "{bad:?} must be refused");
        }
    }
}
