//! Bounded geometry projection of comp's authoritative property snapshot.
//! Titles, application identifiers and unrelated properties are never retained.

use cosmix_flock::{MAX_OBSTACLES, Rect};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

pub const MAX_SNAPSHOT_BYTES: usize = 1024 * 1024;

#[derive(Clone, Debug)]
pub struct Geometry {
    pub instance: String,
    pub sequence: u64,
    pub lost: u64,
    pub locked: bool,
    pub outputs: BTreeMap<String, Rect>,
    pub obstacles: Vec<Rect>,
    /// Outputs whose entire logical area is excluded by the obstacle policy.
    /// This is not an assertion about other clients' pixel opacity.
    pub covered_outputs: BTreeSet<String>,
}

impl Geometry {
    pub fn decode(body: &str) -> Result<Self, &'static str> {
        if body.len() > MAX_SNAPSHOT_BYTES {
            return Err("snapshot exceeds byte limit");
        }
        let root: Value = serde_json::from_str(body).map_err(|_| "invalid snapshot JSON")?;
        let instance = root["info"]["instance"]
            .as_str()
            .filter(|s| !s.is_empty() && s.len() <= 128)
            .ok_or("missing instance")?
            .to_owned();
        let sequence = root["port"]["event_seq"]
            .as_u64()
            .ok_or("missing sequence")?;
        let lost = root["port"]["lost_count"]
            .as_u64()
            .ok_or("missing loss counter")?;
        let locked = match root["focus"]["session_lock"].as_str() {
            Some("none") => false,
            Some("locking" | "locked" | "orphaned" | "unlocking") => true,
            _ => return Err("unknown lock state"),
        };
        let rows = root["outputs"].as_object().ok_or("missing outputs")?;
        if rows.len() > 16 {
            return Err("too many outputs");
        }
        let mut outputs = BTreeMap::new();
        for row in rows.values() {
            let name = row["name"]
                .as_str()
                .filter(|s| !s.is_empty() && s.len() <= 256)
                .ok_or("invalid output name")?;
            if outputs.insert(name.to_owned(), rectangle(row)?).is_some() {
                return Err("duplicate output name");
            }
        }
        let rows = root["surfaces"].as_object().ok_or("missing surfaces")?;
        if rows.len() > 4096 {
            return Err("too many surfaces");
        }
        let mut obstacles = Vec::new();
        if !locked {
            for row in rows.values() {
                let mapped = row["mapped"].as_bool().ok_or("invalid mapped flag")?;
                let visible = row["visible"].as_bool().ok_or("invalid visible flag")?;
                let minimised = row["minimized"].as_bool().ok_or("invalid minimised flag")?;
                if !mapped || !visible || minimised {
                    continue;
                }
                // Include popup and XWayland geometry, but never our own
                // Background layer or subsurfaces already covered by a parent.
                let obstacle = match row["role"].as_str() {
                    Some("toplevel" | "popup" | "x11-toplevel" | "x11-override-redirect") => true,
                    Some("layer") => matches!(
                        row["layer"]["stratum"].as_str(),
                        Some("top" | "overlay" | "bottom")
                    ),
                    _ => false,
                };
                if obstacle {
                    if obstacles.len() == MAX_OBSTACLES {
                        return Err("too many obstacles");
                    }
                    obstacles.push(rectangle(row)?);
                }
            }
        }
        let covered_outputs = outputs
            .iter()
            .filter(|(_, output)| fully_covered(**output, &obstacles))
            .map(|(name, _)| name.clone())
            .collect();
        Ok(Self {
            instance,
            sequence,
            lost,
            locked,
            outputs,
            obstacles,
            covered_outputs,
        })
    }

    /// Intersect in global logical coordinates before converting to this
    /// output's local coordinates. A spanning window affects both monitors.
    pub fn for_output(
        &self,
        name: &str,
        size: (u32, u32),
        origin: (i32, i32),
    ) -> Option<Vec<Rect>> {
        let output = self.matching_output(name, size, origin)?;
        Some(
            self.obstacles
                .iter()
                .filter_map(|rect| {
                    let x = rect.min.x.max(output.min.x);
                    let y = rect.min.y.max(output.min.y);
                    let right = rect.max.x.min(output.max.x);
                    let bottom = rect.max.y.min(output.max.y);
                    Rect::new(x - output.min.x, y - output.min.y, right - x, bottom - y)
                })
                .collect(),
        )
    }

    pub fn covers_output(&self, name: &str, size: (u32, u32), origin: (i32, i32)) -> bool {
        self.covered_outputs.contains(name) && self.matching_output(name, size, origin).is_some()
    }

    fn matching_output(&self, name: &str, size: (u32, u32), origin: (i32, i32)) -> Option<&Rect> {
        let output = self.outputs.get(name)?;
        if output.min.x != origin.0 as f32
            || output.min.y != origin.1 as f32
            || output.max.x - output.min.x != size.0 as f32
            || output.max.y - output.min.y != size.1 as f32
        {
            return None;
        }
        Some(output)
    }
}

/// Exact rectangle-union coverage, computed once per accepted snapshot.
/// Sweep clipped x edges and count coverage of compressed y intervals. Counts
/// retain overlaps correctly; no pixel rasterisation or epsilon can erase a
/// narrow uncovered strip. At most 2 * MAX_OBSTACLES events/extra coordinates
/// are retained, with bounded quadratic work and linear scratch storage.
fn fully_covered(output: Rect, obstacles: &[Rect]) -> bool {
    let mut events = Vec::new();
    let mut ys = vec![output.min.y, output.max.y];
    for rect in obstacles {
        let left = output.min.x.max(rect.min.x);
        let right = output.max.x.min(rect.max.x);
        let top = output.min.y.max(rect.min.y);
        let bottom = output.max.y.min(rect.max.y);
        if left >= right || top >= bottom {
            continue;
        }
        if left == output.min.x
            && right == output.max.x
            && top == output.min.y
            && bottom == output.max.y
        {
            return true;
        }
        events.push((left, top, bottom, true));
        events.push((right, top, bottom, false));
        ys.extend([top, bottom]);
    }
    ys.sort_by(f32::total_cmp);
    ys.dedup();
    events.sort_by(|a, b| a.0.total_cmp(&b.0));
    let mut counts = vec![0u32; ys.len() - 1];
    let mut uncovered = counts.len();
    let mut x = output.min.x;
    for (edge, top, bottom, entering) in events {
        if edge > x && uncovered != 0 {
            return false;
        }
        x = edge;
        let first = ys.partition_point(|y| *y < top);
        let end = ys.partition_point(|y| *y < bottom);
        for count in &mut counts[first..end] {
            if entering {
                if *count == 0 {
                    uncovered -= 1;
                }
                *count += 1;
            } else {
                *count -= 1;
                if *count == 0 {
                    uncovered += 1;
                }
            }
        }
    }
    x == output.max.x
}

fn rectangle(row: &Value) -> Result<Rect, &'static str> {
    let mut fields = [0.0f32; 4];
    for (target, key) in fields.iter_mut().zip(["x", "y", "width", "height"]) {
        let value = row[key].as_f64().ok_or("missing rectangle coordinate")?;
        if !value.is_finite() || value.abs() > 16_777_216.0 {
            return Err("rectangle out of range");
        }
        *target = value as f32;
    }
    if fields[2] > 32768.0 || fields[3] > 32768.0 {
        return Err("rectangle too large");
    }
    Rect::new(fields[0], fields[1], fields[2], fields[3]).ok_or("invalid rectangle")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn fixture() -> Value {
        json!({"info":{"instance":"test-instance"},"port":{"event_seq":7,"lost_count":0},
            "focus":{"session_lock":"none"},
            "outputs":{"a":{"name":"LEFT","x":-800,"y":0,"width":800,"height":600},
                "b":{"name":"RIGHT","x":0,"y":0,"width":800,"height":600}},
            "surfaces":{"s1":{"role":"toplevel","mapped":true,"visible":true,"minimized":false,
                "x":-50,"y":20,"width":100,"height":100,"title":"not retained"}}})
    }
    #[test]
    fn coverage_matches_cell_oracle_for_all_pairs_of_small_rectangles() {
        let output = Rect::new(-20.5, -10.25, 3.0, 3.0).unwrap();
        let mut rectangles = Vec::new();
        for left in 0..3 {
            for right in left + 1..=3 {
                for top in 0..3 {
                    for bottom in top + 1..=3 {
                        rectangles.push(
                            Rect::new(
                                output.min.x + left as f32,
                                output.min.y + top as f32,
                                (right - left) as f32,
                                (bottom - top) as f32,
                            )
                            .unwrap(),
                        );
                    }
                }
            }
        }
        assert!(!fully_covered(output, &[]));
        for a in &rectangles {
            for b in &rectangles {
                let expected = (0..3).all(|x| {
                    (0..3).all(|y| {
                        let point = cosmix_flock::Point::new(
                            output.min.x + x as f32 + 0.5,
                            output.min.y + y as f32 + 0.5,
                        );
                        a.contains(point) || b.contains(point)
                    })
                });
                assert_eq!(fully_covered(output, &[*a, *b]), expected, "{a:?} {b:?}");
            }
        }
    }

    #[test]
    fn coverage_preserves_narrow_gaps_and_counts_overlapping_clipped_windows() {
        let output = Rect::new(0.0, 0.0, 800.0, 600.0).unwrap();
        let left = Rect::new(-100.0, -100.0, 500.0, 800.0).unwrap();
        let right = Rect::new(400.25, 0.0, 500.0, 600.0).unwrap();
        assert!(!fully_covered(output, &[left, left, right]));
        let bridge = Rect::new(399.0, 0.0, 2.0, 600.0).unwrap();
        assert!(fully_covered(output, &[left, left, right, bridge]));
        let almost = Rect::new(399.0, 0.0, 2.0, 599.75).unwrap();
        assert!(!fully_covered(output, &[left, left, right, almost]));
    }

    #[test]
    fn snapshot_coverage_tracks_minimise_and_output_identity() {
        let mut root = fixture();
        root["surfaces"]["s1"]["x"] = json!(-800);
        root["surfaces"]["s1"]["y"] = json!(0);
        root["surfaces"]["s1"]["width"] = json!(800);
        root["surfaces"]["s1"]["height"] = json!(600);
        assert_eq!(
            Geometry::decode(&root.to_string()).unwrap().covered_outputs,
            BTreeSet::from(["LEFT".to_owned()])
        );
        root["surfaces"]["s1"]["minimized"] = json!(true);
        assert!(
            Geometry::decode(&root.to_string())
                .unwrap()
                .covered_outputs
                .is_empty()
        );
    }
    #[test]
    fn spanning_windows_project_to_negative_origin_and_both_outputs() {
        let scene = Geometry::decode(&fixture().to_string()).unwrap();
        assert_eq!(
            scene.for_output("LEFT", (800, 600), (-800, 0)).unwrap(),
            vec![Rect::new(750.0, 20.0, 50.0, 100.0).unwrap()]
        );
        assert_eq!(
            scene.for_output("RIGHT", (800, 600), (0, 0)).unwrap(),
            vec![Rect::new(0.0, 20.0, 50.0, 100.0).unwrap()]
        );
        assert!(scene.for_output("LEFT", (801, 600), (-800, 0)).is_none());
        assert!(!format!("{scene:?}").contains("not retained"));
    }
    #[test]
    fn hidden_and_locked_geometry_is_excluded() {
        let mut root = fixture();
        root["surfaces"]["s1"]["visible"] = json!(false);
        assert!(
            Geometry::decode(&root.to_string())
                .unwrap()
                .obstacles
                .is_empty()
        );
        root["surfaces"]["s1"]["visible"] = json!(true);
        root["focus"]["session_lock"] = json!("locked");
        assert!(
            Geometry::decode(&root.to_string())
                .unwrap()
                .obstacles
                .is_empty()
        );
        root["focus"]["session_lock"] = json!("unexpected");
        assert!(Geometry::decode(&root.to_string()).is_err());
    }
    #[test]
    fn malformed_and_oversized_geometry_is_rejected() {
        let mut root = fixture();
        root["surfaces"]["s1"]["width"] = json!(-1);
        assert!(Geometry::decode(&root.to_string()).is_err());
        assert!(Geometry::decode(&" ".repeat(MAX_SNAPSHOT_BYTES + 1)).is_err());
        let mut root = fixture();
        let row = root["surfaces"]["s1"].clone();
        for id in 0..=MAX_OBSTACLES {
            root["surfaces"][format!("s{id}")] = row.clone();
        }
        assert!(Geometry::decode(&root.to_string()).is_err());
    }
    #[test]
    fn native_and_xwayland_roles_are_obstacles_but_wallpaper_and_drag_icons_are_not() {
        for role in ["toplevel", "popup", "x11-toplevel", "x11-override-redirect"] {
            let mut root = fixture();
            root["surfaces"]["s1"]["role"] = json!(role);
            assert_eq!(
                Geometry::decode(&root.to_string()).unwrap().obstacles.len(),
                1,
                "{role}"
            );
        }
        for role in ["subsurface", "drag-icon", "cursor"] {
            let mut root = fixture();
            root["surfaces"]["s1"]["role"] = json!(role);
            assert!(
                Geometry::decode(&root.to_string())
                    .unwrap()
                    .obstacles
                    .is_empty(),
                "{role}"
            );
        }
        let mut root = fixture();
        root["surfaces"]["s1"]["role"] = json!("layer");
        root["surfaces"]["s1"]["layer"] = json!({"stratum":"background"});
        assert!(
            Geometry::decode(&root.to_string())
                .unwrap()
                .obstacles
                .is_empty()
        );
        root["surfaces"]["s1"]["layer"]["stratum"] = json!("top");
        assert_eq!(
            Geometry::decode(&root.to_string()).unwrap().obstacles.len(),
            1
        );
    }
}
