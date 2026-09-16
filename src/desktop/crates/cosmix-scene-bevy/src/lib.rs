//! Mix Scenes host adapter. Validation and resolved ports belong exclusively to P1.
#[cfg(feature = "gate")]
mod gate;
mod render;
pub use render::Events as SceneEvents;

use bevy::prelude::*;
use cosmix_scene::{ResolvedScene, SceneDocument, Severity};
use cosmix_shell::runtime::{SceneVerb, ShellRuntimeSet};
use ctk::bus::BusBridge;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

pub struct ScenePlugin;
impl Plugin for ScenePlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<SceneStore>();
        render::install(app);
        #[cfg(feature = "gate")]
        gate::install(app);
        app.add_systems(
            Update,
            render::reconcile
                .after(ShellRuntimeSet::Input)
                .before(ShellRuntimeSet::Model),
        );
    }
}

pub(crate) struct SceneEntry {
    document: SceneDocument,
    pub tree: ResolvedScene,
    revision: u64,
    pub mounted: Option<render::Mounted>,
}

#[derive(Resource, Default)]
pub struct SceneStore {
    pub(crate) scenes: BTreeMap<String, SceneEntry>,
    pub(crate) removed: Vec<render::Mounted>,
    revisions: BTreeMap<String, u64>,
}

impl SceneStore {
    /// Transactional Bus ingress: a rejected candidate never replaces last-good.
    pub fn dispatch(
        &mut self,
        verb: SceneVerb,
        body: &str,
        args: &Value,
        bridge: &BusBridge,
    ) -> (u8, String) {
        match self.request(verb, body, args) {
            Ok((reply, summary)) => {
                if let Some(summary) = summary {
                    let wire = format!("---\ncommand: shell.scene.changed\n---\n{summary}");
                    if let Err(error) = bridge.try_publish_topic("shell.scene.changed", false, wire)
                    {
                        warn!("scene summary publish failed: {error}");
                    }
                }
                (0, reply.to_string())
            }
            Err(error) => {
                if matches!(verb, SceneVerb::Load | SceneVerb::Patch) {
                    let name = error["scene"].as_str().or_else(|| args["scene"].as_str());
                    let revision = name
                        .and_then(|name| self.revisions.get(name))
                        .copied()
                        .unwrap_or(0);
                    let diagnostics = error
                        .get("diagnostics")
                        .cloned()
                        .unwrap_or_else(|| json!([error]));
                    let summary =
                        json!({"scene":name,"revision":revision,"ops":0,"diagnostics":diagnostics});
                    let wire = format!("---\ncommand: shell.scene.changed\n---\n{summary}");
                    if let Err(error) = bridge.try_publish_topic("shell.scene.changed", false, wire)
                    {
                        warn!("scene summary publish failed: {error}");
                    }
                }
                (10, error.to_string())
            }
        }
    }

    fn request(
        &mut self,
        verb: SceneVerb,
        body: &str,
        args: &Value,
    ) -> Result<(Value, Option<Value>), Value> {
        let name = args["scene"].as_str().unwrap_or_default();
        match verb {
            SceneVerb::Load => {
                let document = cosmix_scene::parse(body).map_err(|d| json!({"diagnostics":d}))?;
                self.accept(document)
            }
            SceneVerb::Describe => {
                let families = [
                    "window", "column", "row", "text", "field", "button", "toggle", "list",
                    "image", "spacer",
                ];
                let value = if let Some(family) = args["family"].as_str() {
                    json!(
                        cosmix_scene::describe(family)
                            .ok_or_else(|| json!({"error":"unknown family"}))?
                    )
                } else {
                    families
                        .into_iter()
                        .map(|family| {
                            (
                                family.to_owned(),
                                json!(cosmix_scene::describe(family).unwrap()),
                            )
                        })
                        .collect::<serde_json::Map<_, _>>()
                        .into()
                };
                Ok((value, None))
            }
            SceneVerb::Get | SceneVerb::Watch => {
                let entry = self
                    .scenes
                    .get(name)
                    .ok_or_else(|| json!({"error":"unknown scene"}))?;
                let value = if verb == SceneVerb::Watch {
                    json!({"scene":name,"revision":entry.revision,"digest":digest(&entry.tree)})
                } else if let Some(path) = args["path"].as_str() {
                    let (id, port) = path
                        .split_once('.')
                        .ok_or_else(|| json!({"error":"path must be node.port"}))?;
                    entry
                        .tree
                        .nodes
                        .get(id)
                        .and_then(|n| n.ports.get(port))
                        .cloned()
                        .ok_or_else(|| json!({"error":"unknown path"}))?
                } else {
                    json!(entry.tree)
                };
                Ok((value, None))
            }
            SceneVerb::Patch => {
                let entry = self
                    .scenes
                    .get(name)
                    .ok_or_else(|| json!({"error":"unknown scene"}))?;
                let mut document = entry.document.clone();
                let (id, port) = args["path"]
                    .as_str()
                    .and_then(|p| p.split_once('.'))
                    .ok_or_else(|| json!({"error":"path must be node.port"}))?;
                let value = args
                    .get("value")
                    .ok_or_else(|| json!({"error":"value is required"}))?;
                let node = document
                    .nodes
                    .get_mut(id)
                    .ok_or_else(|| json!({"error":"unknown node"}))?;
                if !cosmix_scene::describe(&node.widget)
                    .is_some_and(|ports| ports.iter().any(|description| description.path == port))
                {
                    return Err(json!({"error":"unknown port"}));
                }
                if value.is_null() {
                    node.ports.shift_remove(port);
                } else {
                    node.ports.insert(port.into(), value.clone());
                }
                self.accept(document)
            }
            SceneVerb::Unload => {
                let entry = self
                    .scenes
                    .remove(name)
                    .ok_or_else(|| json!({"error":"unknown scene"}))?;
                if let Some(mounted) = entry.mounted {
                    self.removed.push(mounted);
                }
                Ok((json!({"scene":name,"unloaded":true}), None))
            }
        }
    }

    fn accept(&mut self, document: SceneDocument) -> Result<(Value, Option<Value>), Value> {
        let diagnostics = cosmix_scene::lint(&document);
        if diagnostics.iter().any(|d| d.severity == Severity::Error) {
            return Err(json!({"scene":document.name,"diagnostics":diagnostics}));
        }
        let tree = cosmix_scene::resolve(&document).map_err(|d| json!({"diagnostics":d}))?;
        let ops = self.scenes.get(&tree.name).map_or(tree.nodes.len(), |old| {
            cosmix_scene::diff(&old.tree, &tree).len()
        });
        let revision = self.revisions.entry(tree.name.clone()).or_default();
        *revision += 1;
        let revision = *revision;
        let reply = json!({"scene":tree.name,"revision":revision,"digest":digest(&tree)});
        let summary =
            json!({"scene":tree.name,"revision":revision,"ops":ops,"diagnostics":diagnostics});
        let mounted = self.scenes.remove(&tree.name).and_then(|old| old.mounted);
        self.scenes.insert(
            tree.name.clone(),
            SceneEntry {
                document,
                tree,
                revision,
                mounted,
            },
        );
        Ok((reply, Some(summary)))
    }
}

fn digest(tree: &ResolvedScene) -> String {
    format!("{:x}", Sha256::digest(serde_json::to_vec(tree).unwrap()))
}

#[cfg(test)]
mod tests {
    use super::*;
    const FIXTURE: &str = include_str!("../../cosmix-scene/tests/fixtures/conformance.scene.md");
    #[test]
    fn conformance_and_last_good() {
        let expected = cosmix_scene::resolve(&cosmix_scene::parse(FIXTURE).unwrap()).unwrap();
        let mut store = SceneStore::default();
        store
            .request(SceneVerb::Load, FIXTURE, &Value::Null)
            .unwrap();
        let get = json!({"scene":"conformance"});
        assert_eq!(
            store.request(SceneVerb::Get, "", &get).unwrap().0,
            json!(expected)
        );
        assert!(store.request(SceneVerb::Load, "bad", &Value::Null).is_err());
        assert!(
            store
                .request(
                    SceneVerb::Patch,
                    "",
                    &json!({"scene":"conformance","path":"field.value","value":false})
                )
                .is_err()
        );
        assert_eq!(
            store.request(SceneVerb::Get, "", &get).unwrap().0,
            json!(expected)
        );
        assert_eq!(store.scenes["conformance"].revision, 1);
    }
}
