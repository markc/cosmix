//! Versioned user choices. Transient scene and pointer data never enter this file.

use bevy::prelude::Resource;
use cosmix_flock::Settings;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    io::{Read, Write},
    path::{Path, PathBuf},
};

const MAX_BYTES: u64 = 16_384;

// Reversible wallpaper choices are controllable by registered local services
// and admitted mesh services. Remote identity must come from the recipient's
// noded, which strips client broker headers and stamps proven direct bridges.
fn authorize_preference_write(request: &ctk::bus::InboundRequest) -> Result<(), &'static str> {
    if ctk::app_control::authorize_local_caller(request).is_ok() {
        return Ok(());
    }
    let header = |name: &str| {
        let mut values = request
            .headers
            .iter()
            .filter(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str());
        let value = values.next();
        if values.next().is_some() { None } else { value }
    };
    let service_name = |name: &str| {
        let bytes = name.as_bytes();
        (2..=31).contains(&bytes.len())
            && bytes[0].is_ascii_lowercase()
            && bytes[1..]
                .iter()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'-')
    };
    if request.headers.keys().any(|key| {
        ["source_peer", "permissions", "signed_ident"]
            .iter()
            .any(|name| key.eq_ignore_ascii_case(name))
    }) || header("broker_origin") != Some("mesh")
    {
        return Err("unproven_caller");
    }
    match (header("broker_peer"), header("broker_service")) {
        (Some(peer), Some(service))
            if !peer.is_empty()
                && request.from == format!("bridge-{peer}")
                && service_name(&request.from)
                && service_name(service) =>
        {
            Ok(())
        }
        _ => Err("unproven_caller"),
    }
}

pub const CHANGED_TOPIC: &str = "wallpaper.props.changed";
pub const PATHS: [&str; 9] = [
    "enabled",
    "paused",
    "preset",
    "flock.count",
    "speed",
    "pointer.radius",
    "window.margin",
    "fps_limit",
    "seed",
];

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Preset {
    #[default]
    Ocean,
    Ember,
    Twilight,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Preferences {
    pub enabled: bool,
    pub paused: bool,
    pub preset: Preset,
    pub count: usize,
    pub speed: f32,
    pub pointer_radius: f32,
    pub window_margin: f32,
    pub fps_limit: u32,
    pub seed: u64,
}
impl Default for Preferences {
    fn default() -> Self {
        let settings = Settings::default();
        Self {
            enabled: true,
            paused: false,
            preset: Preset::default(),
            count: settings.count,
            speed: settings.speed,
            pointer_radius: settings.pointer_radius,
            window_margin: settings.window_margin,
            fps_limit: 30,
            seed: 42,
        }
    }
}
impl Preferences {
    pub fn settings(&self) -> Settings {
        Settings {
            count: self.count,
            speed: self.speed,
            pointer_radius: self.pointer_radius,
            window_margin: self.window_margin,
        }
    }
    pub fn validate(&self) -> Result<(), String> {
        if !self.settings().valid() || !(1..=60).contains(&self.fps_limit) {
            return Err("preference outside supported range".into());
        }
        Ok(())
    }
    pub fn tree(&self) -> Value {
        json!({"enabled": self.enabled, "paused": self.paused, "preset": self.preset,
            "flock":{"count":self.count}, "speed":self.speed,
            "pointer":{"radius":self.pointer_radius}, "window":{"margin":self.window_margin},
            "fps_limit":self.fps_limit, "seed":self.seed})
    }
    pub fn get(&self, path: &str) -> Option<Value> {
        let mut value = self.tree();
        if !path.is_empty() {
            for segment in path.split('.') {
                value = value.get(segment)?.clone();
            }
        }
        Some(value)
    }
    pub fn describe(&self, path: &str) -> Option<Value> {
        let value = self.get(path)?;
        if let Some(object) = value.as_object() {
            return Some(json!({"type":"object","children":object.keys().collect::<Vec<_>>()}));
        }
        let (kind, range) = match path {
            "enabled" | "paused" => ("boolean", Value::Null),
            "preset" => ("string", Value::Null),
            "flock.count" => ("integer", json!({"min":0,"max":1024})),
            "fps_limit" => ("integer", json!({"min":1,"max":60})),
            "seed" => ("integer", json!({"min":0,"max":u64::MAX})),
            "speed" => ("number", json!({"min":5,"max":300})),
            "pointer.radius" => ("number", json!({"min":0,"max":600})),
            "window.margin" => ("number", json!({"min":0,"max":100})),
            _ => return None,
        };
        let mut descriptor =
            json!({"type":kind,"mutable":true,"persistence":"file","owner":"wallpaper"});
        if !range.is_null() {
            descriptor["range"] = range;
        }
        if path == "preset" {
            descriptor["enum"] = json!(["ocean", "ember", "twilight"]);
        }
        Some(descriptor)
    }
    pub fn changed(&self, path: &str, value: Value) -> Result<Self, String> {
        let field = match path {
            "flock.count" => "count",
            "pointer.radius" => "pointer_radius",
            "window.margin" => "window_margin",
            path if PATHS.contains(&path) => path,
            _ => return Err("unknown preference path".into()),
        };
        let mut candidate = serde_json::to_value(self).map_err(|e| e.to_string())?;
        candidate[field] = value;
        let candidate: Self = serde_json::from_value(candidate).map_err(|e| e.to_string())?;
        candidate.validate()?;
        Ok(candidate)
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Saved {
    version: u32,
    preferences: Preferences,
}

#[derive(Resource)]
pub struct PreferenceStore {
    pub current: Preferences,
    pub error: Option<String>,
    path: Option<PathBuf>,
    instance: String,
    revision: u64,
    notification_pending: bool,
    topic: String,
}
impl PreferenceStore {
    pub fn set_service(&mut self, service: &str) {
        self.topic = format!("{service}.props.changed");
    }
    pub fn watch(&self) -> Value {
        json!({"version":1,"topic":self.topic,"instance":self.instance,
            "event_seq":self.revision,"scope":"","reconcile_ms":1000})
    }
    pub fn invalidate_subscribers(&mut self) {
        self.notification_pending = true;
    }
    pub fn has_pending_notification(&self) -> bool {
        self.notification_pending
    }
    /// One root invalidation covers any number of writes. Publication is
    /// best-effort; consumers reconcile with get, including after reconnect.
    pub fn publish_pending(&mut self, bridge: &ctk::bus::BusBridge) -> bool {
        if !self.notification_pending {
            return true;
        }
        let wire = cosmix_bus::bus::BusMessage::new()
            .with_header("command", &self.topic)
            .with_body(&self.watch().to_string())
            .to_wire();
        if bridge.try_publish_topic(&self.topic, false, wire).is_err() {
            return false;
        }
        self.notification_pending = false;
        true
    }
    pub fn reply(&mut self, request: &ctk::bus::InboundRequest) -> (u8, String) {
        let result = (|| -> Result<Value, String> {
            if request.body.len() > 4096 {
                return Err("request too large".into());
            }
            let body: Value = if request.body.trim().is_empty() {
                json!({})
            } else {
                serde_json::from_str(&request.body).map_err(|e| e.to_string())?
            };
            let object = body.as_object().ok_or("expected an object")?;
            let argument = if request.command == "wallpaper.props.list" {
                object.get("prefix").or_else(|| object.get("path"))
            } else {
                object.get("path")
            };
            let path = match argument {
                Some(value) => value.as_str().ok_or("path must be a string")?,
                None => "",
            };
            match request.command.as_str() {
                "wallpaper.props.watch" => {
                    if self.current.get(path).is_none() {
                        return Err("unknown preference path".into());
                    }
                    Ok(self.watch())
                }
                "wallpaper.props.get" => self
                    .current
                    .get(path)
                    .ok_or("unknown preference path".into()),
                "wallpaper.props.describe" => self
                    .current
                    .describe(path)
                    .ok_or("unknown preference path".into()),
                "wallpaper.props.list" => Ok(json!(
                    PATHS
                        .iter()
                        .filter(|p| path.is_empty()
                            || **p == path
                            || p.starts_with(&format!("{path}.")))
                        .collect::<Vec<_>>()
                )),
                "wallpaper.props.set" => {
                    authorize_preference_write(request)?;
                    if object.keys().any(|key| key != "path" && key != "value") {
                        return Err("unknown request field".into());
                    }
                    let value = object.get("value").ok_or("missing value")?.clone();
                    let old = self.current.get(path).ok_or("unknown preference path")?;
                    self.set(path, value)?;
                    Ok(
                        json!({"path":path,"old":old,"new":self.current.get(path),"persisted":true,
                        "instance":self.instance,"event_seq":self.revision}),
                    )
                }
                _ => Err("unknown_verb".into()),
            }
        })();
        match result {
            Ok(value) => (0, value.to_string()),
            Err(error) => (10, json!({"error":error}).to_string()),
        }
    }
    pub fn load(path: Option<PathBuf>) -> Self {
        let mut nonce = [0u8; 16];
        std::fs::File::open("/dev/urandom")
            .and_then(|mut file| file.read_exact(&mut nonce))
            .expect("wallpaper needs a unique property instance");
        let instance = nonce.iter().map(|byte| format!("{byte:02x}")).collect();
        let loaded = path.as_deref().map(read).transpose();
        match loaded {
            Ok(current) => Self {
                current: current.flatten().unwrap_or_default(),
                error: None,
                path,
                instance,
                revision: 0,
                notification_pending: false,
                topic: CHANGED_TOPIC.into(),
            },
            Err(error) => Self {
                current: Preferences::default(),
                error: Some(error),
                path,
                instance,
                revision: 0,
                notification_pending: false,
                topic: CHANGED_TOPIC.into(),
            },
        }
    }
    /// Saving succeeds before the live value changes. A failed write leaves
    /// both the old complete file and the running settings intact.
    pub fn set(&mut self, path: &str, value: Value) -> Result<(), String> {
        let candidate = self.current.changed(path, value)?;
        let changed = candidate != self.current;
        let revision = if changed {
            self.revision
                .checked_add(1)
                .ok_or("property revision exhausted")?
        } else {
            self.revision
        };
        let destination = self.path.as_deref().ok_or("preferences path unavailable")?;
        if let Err(error) = save(destination, &candidate) {
            self.error = Some(error.clone());
            return Err(error);
        }
        self.current = candidate;
        self.revision = revision;
        self.notification_pending |= changed;
        self.error = None;
        Ok(())
    }
}

pub fn state_path() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("COSMIX_WALLPAPER_STATE_FILE") {
        let path = PathBuf::from(path);
        return path.is_absolute().then_some(path);
    }
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| {
            std::env::var_os("HOME")
                .map(|p| PathBuf::from(p).join(".local/state"))
                .filter(|p| p.is_absolute())
        })?;
    Some(base.join("cosmix/wallpaper.json"))
}
fn read(path: &Path) -> Result<Option<Preferences>, String> {
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.to_string()),
    };
    let mut bytes = Vec::new();
    file.take(MAX_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    if bytes.len() as u64 > MAX_BYTES {
        return Err("preferences file too large".into());
    }
    let saved: Saved = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
    if saved.version != 1 {
        return Err("unsupported preferences version".into());
    }
    saved.preferences.validate()?;
    Ok(Some(saved.preferences))
}
fn save(path: &Path, preferences: &Preferences) -> Result<(), String> {
    preferences.validate()?;
    let parent = path.parent().ok_or("preferences path has no parent")?;
    std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    let bytes = serde_json::to_vec_pretty(&Saved {
        version: 1,
        preferences: preferences.clone(),
    })
    .map_err(|e| e.to_string())?;
    let mut temp = tempfile::NamedTempFile::new_in(parent).map_err(|e| e.to_string())?;
    temp.write_all(&bytes).map_err(|e| e.to_string())?;
    temp.as_file().sync_all().map_err(|e| e.to_string())?;
    temp.persist(path).map_err(|e| e.error.to_string())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn unproven_mesh_writes_leave_memory_disk_and_revision_unchanged() {
        use std::collections::BTreeMap;
        let valid = ctk::bus::InboundRequest {
            connection_generation: 1,
            from: "bridge-alpha".into(),
            command: "wallpaper.props.set".into(),
            headers: BTreeMap::from([
                ("broker_origin".into(), "mesh".into()),
                ("broker_peer".into(), "alpha".into()),
                ("broker_service".into(), "mix-test".into()),
            ]),
            body: json!({"path":"paused","value":true}).to_string(),
            reply_id: Some("write-test".into()),
        };
        let mut cases = Vec::new();
        for name in ["broker_origin", "broker_peer", "broker_service"] {
            let mut missing = valid.clone();
            missing.headers.remove(name);
            cases.push(missing);
            let mut duplicate = valid.clone();
            duplicate
                .headers
                .insert(name.to_uppercase(), valid.headers[name].clone());
            cases.push(duplicate);
        }
        for (name, value) in [
            ("broker_origin", "local-unregistered"),
            ("broker_peer", "beta"),
            ("broker_peer", ""),
            ("broker_service", ""),
            ("broker_service", "Invalid.service"),
            ("Source_Peer", "alpha"),
            ("PERMISSIONS", "all"),
            ("signed_ident", "assertion"),
        ] {
            let mut request = valid.clone();
            request.headers.insert(name.into(), value.into());
            cases.push(request);
        }
        for from in ["", "mix-test", "bridge-beta"] {
            let mut request = valid.clone();
            request.from = from.into();
            cases.push(request);
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wallpaper.json");
        let mut store = PreferenceStore::load(Some(path.clone()));
        for request in cases {
            assert_eq!(store.reply(&request).0, 10, "{request:?}");
            assert!(!store.current.paused);
            assert_eq!(store.watch()["event_seq"], 0);
            assert!(!path.exists());
        }
        assert_eq!(store.reply(&valid).0, 0);
        assert!(store.current.paused);
        assert_eq!(store.watch()["event_seq"], 1);
        assert!(PreferenceStore::load(Some(path)).current.paused);
    }

    #[test]
    fn notifications_coalesce_retry_and_fence_restarts() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("preferences.json");
        let mut store = PreferenceStore::load(Some(path.clone()));
        let (bridge, peer) = ctk::bus::test_bridge("wallpaper");
        let mut filled = 0;
        while bridge
            .try_publish_topic("test.changed", false, "{}")
            .is_ok()
        {
            filled += 1;
            assert!(filled <= 64, "test bridge must remain bounded");
        }
        store.set("paused", json!(true)).unwrap();
        store.set("speed", json!(100)).unwrap();
        assert!(!store.publish_pending(&bridge));
        assert_eq!(peer.drain_publishes().len(), filled);
        assert!(store.publish_pending(&bridge));
        let publications = peer.drain_publishes();
        assert_eq!(publications.len(), 1);
        assert_eq!(publications[0].headers["name"], CHANGED_TOPIC);
        assert_eq!(publications[0].headers["retain"], "false");
        let message = cosmix_bus::bus::parse_strict(&publications[0].body).unwrap();
        let event: Value = serde_json::from_str(&message.body).unwrap();
        assert_eq!(event["event_seq"], 2);
        assert_eq!(event["instance"], store.watch()["instance"]);
        assert_eq!(event["scope"], "");
        assert!(store.publish_pending(&bridge));
        assert!(peer.drain_publishes().is_empty());
        store.set("speed", json!(100)).unwrap();
        assert!(store.set("speed", json!(-1)).is_err());
        assert_eq!(store.watch()["event_seq"], 2);
        assert!(store.publish_pending(&bridge));
        assert!(peer.drain_publishes().is_empty());
        store.invalidate_subscribers();
        assert!(store.publish_pending(&bridge));
        assert_eq!(peer.drain_publishes().len(), 1);
        let restored = PreferenceStore::load(Some(path));
        assert_eq!(restored.current, store.current);
        assert_ne!(restored.watch()["instance"], store.watch()["instance"]);
        assert_eq!(restored.watch()["event_seq"], 0);
    }

    #[test]
    fn failed_persistence_does_not_advance_notification_revision() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = PreferenceStore::load(Some(dir.path().to_owned()));
        assert!(store.set("paused", json!(true)).is_err());
        assert_eq!(store.watch()["event_seq"], 0);
        let (bridge, peer) = ctk::bus::test_bridge("wallpaper");
        assert!(store.publish_pending(&bridge));
        assert!(peer.drain_publishes().is_empty());
    }

    #[test]
    fn invalid_writes_leave_existing_values_untouched() {
        let p = Preferences::default();
        for (path, value) in [
            ("flock.count", json!(1025)),
            ("flock.count", json!(-1)),
            ("speed", json!(0)),
            ("fps_limit", json!(61)),
            ("paused", json!("false")),
            ("preset", json!("unknown")),
            ("pointer.radius", json!(601)),
            ("window.margin", json!(-1)),
            ("seed", json!(-1)),
            ("flock", json!({})),
        ] {
            assert!(p.changed(path, value).is_err(), "{path}");
        }
        assert_eq!(p, Preferences::default());
        assert_eq!(p.get("flock"), Some(json!({"count":192})));
        assert!(p.get("floc").is_none());
    }
    #[test]
    fn every_user_choice_round_trips_without_transient_scene_data() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("preferences.json");
        let mut store = PreferenceStore::load(Some(path.clone()));
        store.set("preset", json!("ember")).unwrap();
        store.set("flock.count", json!(64)).unwrap();
        store.set("seed", json!(u64::MAX)).unwrap();
        store.set("paused", json!(true)).unwrap();
        let restored = PreferenceStore::load(Some(path.clone()));
        assert!(restored.error.is_none());
        assert_eq!(restored.current, store.current);
        let disk = std::fs::read_to_string(path).unwrap();
        assert!(
            !disk.contains("obstacles") && !disk.contains("position") && !disk.contains("instance")
        );
    }
    #[test]
    fn corrupt_or_unwritable_storage_is_reported_and_not_silently_applied() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("preferences.json");
        std::fs::write(&path, "bad json").unwrap();
        let store = PreferenceStore::load(Some(path.clone()));
        assert!(store.error.is_some());
        assert_eq!(std::fs::read_to_string(path).unwrap(), "bad json");
        let mut unwritable = PreferenceStore::load(Some(dir.path().to_owned()));
        assert!(unwritable.set("speed", json!(100)).is_err());
        assert_eq!(unwritable.current.speed, 80.0);
    }
}
