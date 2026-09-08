//! One complete pointer sample, fenced by Bus generation and comp instance.

use crate::boids::geometry::Geometry;
use bevy::prelude::Resource;
use cosmix_flock::{Point, Rect};
use serde_json::Value;
use std::time::{Duration, Instant};

pub const MAX_AGE: Duration = Duration::from_millis(2500);

#[derive(Resource, Default)]
pub struct ScenePointer {
    binding: Option<(u64, String)>,
    sequence: u64,
    timestamp: u64,
    sample: Option<(String, Point, Instant)>,
    outputs: std::collections::BTreeMap<String, Rect>,
}
impl ScenePointer {
    pub fn bind(&mut self, generation: Option<u64>, geometry: Option<&Geometry>) {
        let binding = generation
            .zip(geometry)
            .filter(|(_, g)| !g.locked)
            .map(|(generation, g)| (generation, g.instance.clone()));
        if self.binding != binding {
            self.binding = binding;
            self.sequence = geometry.map_or(0, |g| g.sequence);
            self.timestamp = 0;
            self.sample = None;
        }
        if let Some(geometry) = geometry {
            if self.outputs != geometry.outputs {
                self.outputs.clone_from(&geometry.outputs);
                self.sequence = self.sequence.max(geometry.sequence);
                self.clear();
            }
        } else {
            self.outputs.clear();
        }
    }
    pub fn clear(&mut self) {
        self.sample = None;
    }
    pub fn expire(&mut self, now: Instant) {
        if self.deadline().is_some_and(|d| now >= d) {
            self.clear();
        }
    }
    pub fn deadline(&self) -> Option<Instant> {
        self.sample.as_ref().map(|(_, _, at)| *at + MAX_AGE)
    }
    pub fn for_output(&self, name: &str) -> Option<Point> {
        self.sample
            .as_ref()
            .filter(|(output, _, _)| output == name)
            .map(|(_, point, _)| *point)
    }
    pub fn valid(&self) -> bool {
        self.sample.is_some()
    }
    pub fn accept(
        &mut self,
        generation: u64,
        geometry: &Geometry,
        body: &str,
        now: Instant,
    ) -> bool {
        if self.binding.as_ref() != Some(&(generation, geometry.instance.clone()))
            || geometry.locked
            || body.len() > 4096
        {
            return false;
        }
        let Ok(root) = serde_json::from_str::<Value>(body) else {
            self.clear();
            return false;
        };
        let Some(sequence) = root["event_seq"].as_u64() else {
            self.clear();
            return false;
        };
        let Some(timestamp) = root["timestamp_ms"].as_u64() else {
            self.clear();
            return false;
        };
        if root["version"] != 1
            || root["instance"].as_str() != Some(geometry.instance.as_str())
            || sequence <= self.sequence
            || timestamp < self.timestamp
        {
            return false;
        }
        let sample = match root["valid"].as_bool() {
            Some(false) if root["output"].is_null() && root["position"].is_null() => None,
            Some(true) => {
                let Some(output) = root["output"].as_str() else {
                    self.clear();
                    return false;
                };
                let Some(rect) = geometry.outputs.get(output) else {
                    self.clear();
                    return false;
                };
                let (Some(x), Some(y)) = (
                    root["position"]["x"].as_f64(),
                    root["position"]["y"].as_f64(),
                ) else {
                    self.clear();
                    return false;
                };
                if !x.is_finite()
                    || !y.is_finite()
                    || x < 0.0
                    || y < 0.0
                    || x >= f64::from(rect.max.x - rect.min.x)
                    || y >= f64::from(rect.max.y - rect.min.y)
                {
                    self.clear();
                    return false;
                }
                Some((output.to_owned(), Point::new(x as f32, y as f32), now))
            }
            _ => {
                self.clear();
                return false;
            }
        };
        self.sequence = sequence;
        self.timestamp = timestamp;
        self.sample = sample;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cosmix_flock::Rect;
    use serde_json::json;
    fn geometry() -> Geometry {
        Geometry {
            instance: "instance-one".into(),
            sequence: 10,
            lost: 0,
            locked: false,
            outputs: std::collections::BTreeMap::from([(
                "OUT".into(),
                Rect::new(-800.0, 0.0, 800.0, 600.0).unwrap(),
            )]),
            obstacles: vec![],
            covered_outputs: Default::default(),
        }
    }
    fn body(seq: u64) -> Value {
        json!({"version":1,"instance":"instance-one","event_seq":seq,"timestamp_ms":100,"valid":true,"output":"OUT","position":{"x":20.5,"y":30.25}})
    }
    #[test]
    fn local_coordinates_are_not_scaled_again_and_expire_without_frames() {
        let now = Instant::now();
        let geometry = geometry();
        let mut p = ScenePointer::default();
        p.bind(Some(1), Some(&geometry));
        assert!(p.accept(1, &geometry, &body(11).to_string(), now));
        assert_eq!(p.for_output("OUT"), Some(Point::new(20.5, 30.25)));
        assert_eq!(p.for_output("OTHER"), None);
        p.expire(now + MAX_AGE);
        assert!(!p.valid());
    }
    #[test]
    fn epochs_watermark_and_lock_reject_stale_samples() {
        let now = Instant::now();
        let mut geometry = geometry();
        let mut p = ScenePointer::default();
        p.bind(Some(1), Some(&geometry));
        assert!(!p.accept(1, &geometry, &body(10).to_string(), now));
        assert!(p.accept(1, &geometry, &body(11).to_string(), now));
        p.bind(Some(2), Some(&geometry));
        assert!(!p.valid());
        assert!(!p.accept(1, &geometry, &body(12).to_string(), now));
        assert!(p.accept(2, &geometry, &body(12).to_string(), now));
        geometry.locked = true;
        p.bind(Some(2), Some(&geometry));
        assert!(!p.valid());
        assert!(!p.accept(2, &geometry, &body(13).to_string(), now));
    }
    #[test]
    fn output_changes_clear_samples_and_advance_snapshot_fence() {
        let now = Instant::now();
        let mut geometry = geometry();
        let mut p = ScenePointer::default();
        p.bind(Some(1), Some(&geometry));
        assert!(p.accept(1, &geometry, &body(11).to_string(), now));
        geometry.sequence = 20;
        geometry
            .outputs
            .insert("OUT".into(), Rect::new(0.0, 0.0, 600.0, 800.0).unwrap());
        p.bind(Some(1), Some(&geometry));
        assert!(!p.valid());
        assert!(!p.accept(1, &geometry, &body(19).to_string(), now));
        assert!(p.accept(1, &geometry, &body(21).to_string(), now));
        geometry.outputs.clear();
        p.bind(Some(1), Some(&geometry));
        assert!(!p.valid());
    }

    #[test]
    fn invalidation_and_out_of_bounds_clear_the_latest_position() {
        let now = Instant::now();
        let geometry = geometry();
        let mut p = ScenePointer::default();
        p.bind(Some(1), Some(&geometry));
        assert!(p.accept(1, &geometry, &body(11).to_string(), now));
        let mut invalid = body(12);
        invalid["valid"] = json!(false);
        invalid["output"] = Value::Null;
        invalid["position"] = Value::Null;
        assert!(p.accept(1, &geometry, &invalid.to_string(), now));
        assert!(!p.valid());
        let mut outside = body(13);
        outside["position"]["x"] = json!(800);
        assert!(!p.accept(1, &geometry, &outside.to_string(), now));
        assert!(!p.valid());
    }
}
