//! Mix Scenes host adapter. Validation and resolved ports belong exclusively to P1.
#[cfg(feature = "gate")]
mod gate;
mod render;
pub use render::{Events as SceneEvents, reconcile as reconcile_scene_mounts};

use bevy::prelude::*;
use cosmix_scene::{ResolvedScene, SceneDocument, Severity};
use cosmix_shell::core::{OutputKey, SubPanelRegistry};
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
        app.add_systems(
            Update,
            render::remove_unseated_scenes
                .after(ShellRuntimeSet::Model)
                .before(ShellRuntimeSet::Presentation),
        );
    }
}

pub(crate) struct SceneEntry {
    document: SceneDocument,
    pub tree: ResolvedScene,
    revision: u64,
    pub mounted: Option<render::Mounted>,
    owner: Option<SceneOwner>,
}

#[derive(Clone)]
struct SceneOwner {
    citizen: String,
    accepted_at: u64,
}

/// Host-supplied identity and seat, independent of authored scene metadata.
/// The caller must derive `owner` from broker-attested request provenance.
pub struct SceneMount<'a> {
    pub registry: &'a mut SubPanelRegistry,
    pub output: &'a OutputKey,
    pub owner: &'a str,
    pub accepted_at: u64,
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
        mount: &mut SceneMount<'_>,
    ) -> (u8, String) {
        let changes_scene = matches!(verb, SceneVerb::Load | SceneVerb::Patch);
        match self.request_mounted(verb, body, args, Some(mount)) {
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
                if changes_scene {
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

    #[cfg(test)]
    fn request(
        &mut self,
        verb: SceneVerb,
        body: &str,
        args: &Value,
    ) -> Result<(Value, Option<Value>), Value> {
        self.request_mounted(verb, body, args, None)
    }

    fn request_mounted(
        &mut self,
        verb: SceneVerb,
        body: &str,
        args: &Value,
        mount: Option<&mut SceneMount<'_>>,
    ) -> Result<(Value, Option<Value>), Value> {
        let name = args["scene"].as_str().unwrap_or_default();
        match verb {
            SceneVerb::Load => {
                let document = cosmix_scene::parse(body).map_err(|d| json!({"diagnostics":d}))?;
                self.accept(document, mount, true)
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
                if serialised_document(&document).len() > cosmix_scene::MAX_DOCUMENT_BYTES {
                    return Err(json!({"scene":name,"diagnostics":[{
                        "severity":"error", "code":"document-too-large", "line":1,
                        "message":"patched document exceeds 256 KiB"
                    }]}));
                }
                self.accept(document, mount, false)
            }
            SceneVerb::Unload => {
                let entry = self
                    .scenes
                    .remove(name)
                    .ok_or_else(|| json!({"error":"unknown scene"}))?;
                if let Some(mount) = mount {
                    mount.registry.forget(&render::page_id(&entry.tree));
                }
                if let Some(mounted) = entry.mounted {
                    self.removed.push(mounted);
                }
                Ok((json!({"scene":name,"unloaded":true}), None))
            }
        }
    }

    /// Names of the scenes currently owned by `citizen`.
    pub fn scenes_owned_by(&self, citizen: &str) -> Vec<String> {
        self.scenes
            .values()
            .filter(|entry| {
                entry
                    .owner
                    .as_ref()
                    .is_some_and(|owner| owner.citizen == citizen)
            })
            .map(|entry| entry.tree.name.clone())
            .collect()
    }

    /// Unload every scene owned by `citizen`, returning the scene names.
    ///
    /// The owner-disconnect half of sub-panel ownership (panel doc §3): the
    /// broker dropped the citizen's Bus connection, so its content goes too.
    /// Mirrors the `Unload` arm — entries leave the store, mounted pages join
    /// `removed` for the next reconcile to destroy — but works by owner,
    /// because the disconnect names the citizen, not the scenes.
    pub fn unload_owned_by(&mut self, citizen: &str) -> Vec<String> {
        self.unload_owned_before(citizen, u64::MAX)
    }

    /// A deferred absence only removes content accepted before its receipt.
    pub fn unload_owned_before(&mut self, citizen: &str, before: u64) -> Vec<String> {
        let names: Vec<String> = self
            .scenes
            .values()
            .filter(|entry| {
                entry
                    .owner
                    .as_ref()
                    .is_some_and(|owner| owner.citizen == citizen && owner.accepted_at < before)
            })
            .map(|entry| entry.tree.name.clone())
            .collect();
        for name in &names {
            if let Some(entry) = self.scenes.remove(name)
                && let Some(mounted) = entry.mounted
            {
                self.removed.push(mounted);
            }
        }
        names
    }

    fn accept(
        &mut self,
        document: SceneDocument,
        mount: Option<&mut SceneMount<'_>>,
        loading: bool,
    ) -> Result<(Value, Option<Value>), Value> {
        let diagnostics = cosmix_scene::lint(&document);
        if diagnostics.iter().any(|d| d.severity == Severity::Error) {
            return Err(json!({"scene":document.name,"diagnostics":diagnostics}));
        }
        let tree = cosmix_scene::resolve(&document).map_err(|d| json!({"diagnostics":d}))?;
        // A declared mount address is unique across scenes, including scenes
        // from the same citizen. Content revisions cannot rename a live seat;
        // unload first so the old carousel entry is removed transactionally.
        let page = render::page_id(&tree);
        if self.scenes.iter().any(|(name, entry)| {
            (name != &tree.name && render::page_id(&entry.tree) == page)
                || (name == &tree.name && render::page_id(&entry.tree) != page)
        }) {
            return Err(json!({"scene": tree.name, "error_code": "SUBPANEL_COLLISION",
                "error": "scene mount address is occupied or changed; unload before renaming"}));
        }
        let old_owner = self
            .scenes
            .get(&tree.name)
            .and_then(|entry| entry.owner.clone());
        let owner = if let Some(mount) = mount {
            // Patching content is mesh-open but does not take ownership or
            // refresh another citizen's lifetime. Loads are owner registrations.
            let owner = if loading {
                SceneOwner {
                    citizen: mount.owner.to_owned(),
                    accepted_at: mount.accepted_at,
                }
            } else {
                old_owner.unwrap_or_else(|| SceneOwner {
                    citizen: mount.owner.to_owned(),
                    accepted_at: mount.accepted_at,
                })
            };
            mount.registry.mount(
                &render::page_id(&tree), mount.output.clone(), render::scene_edge(&tree),
                &owner.citizen, owner.accepted_at,
            ).map_err(|error| json!({
                "scene":tree.name, "error_code":"SUBPANEL_COLLISION", "error":error.to_string()
            }))?;
            Some(owner)
        } else {
            old_owner
        };
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
                owner,
            },
        );
        Ok((reply, Some(summary)))
    }
}

fn digest(tree: &ResolvedScene) -> String {
    format!("{:x}", Sha256::digest(serde_json::to_vec(tree).unwrap()))
}

// Measure the complete canonical AMP document, including metadata and JSON
// escaping, rather than the patch or the original source retained by P1.
fn serialised_document(document: &SceneDocument) -> String {
    let mut wire = format!(
        "---\nscene: 1\nname: {}\ncitizen: {}\n",
        document.name, document.citizen
    );
    for (key, value) in [
        ("window", &document.window),
        ("subscribe", &document.subscribe),
        ("targets", &document.targets),
        ("model", &document.model),
    ] {
        if let Some(value) = value {
            wire.push_str(&format!("{key}: {value}\n"));
        }
    }
    wire.push_str("---\n```mix\n");
    for (id, node) in &document.nodes {
        let mut value = serde_json::Map::new();
        value.insert("widget".into(), json!(node.widget));
        value.extend(
            node.ports
                .iter()
                .map(|(key, value)| (key.clone(), value.clone())),
        );
        wire.push_str(&format!("{}: {}\n", json!(id), Value::Object(value)));
    }
    wire.push_str("```\n");
    wire
}

#[cfg(test)]
mod tests {
    use super::*;
    const FIXTURE: &str = include_str!("../../cosmix-scene/tests/fixtures/conformance.scene.md");
    #[test]
    fn cumulative_patch_size_is_transactional() {
        let mut store = SceneStore::default();
        store
            .request(SceneVerb::Load, FIXTURE, &Value::Null)
            .unwrap();
        // Each patch fits the ingress bound; their combined document does not.
        for path in ["text.text", "field.value"] {
            store
                .request(
                    SceneVerb::Patch,
                    "",
                    &json!({"scene":"conformance","path":path,"value":"x".repeat(100_000)}),
                )
                .unwrap();
        }
        let before = store.scenes["conformance"].tree.clone();
        let revision = store.scenes["conformance"].revision;
        let error = store
            .request(
                SceneVerb::Patch,
                "",
                &json!({"scene":"conformance","path":"button.label","value":"x".repeat(100_000)}),
            )
            .unwrap_err();
        assert_eq!(error["diagnostics"][0]["code"], "document-too-large");
        assert_eq!(store.scenes["conformance"].tree, before);
        assert_eq!(store.scenes["conformance"].revision, revision);
    }

    #[test]
    fn measured_candidate_is_a_complete_scene_document() {
        let document = cosmix_scene::parse(FIXTURE).unwrap();
        let wire = serialised_document(&document);
        let reparsed = cosmix_scene::parse(&wire).unwrap();
        let expected = cosmix_scene::resolve(&document).unwrap();
        let actual = cosmix_scene::resolve(&reparsed).unwrap();
        assert_eq!(expected.name, actual.name);
        assert_eq!(expected.citizen, actual.citizen);
        assert_eq!(expected.window, actual.window);
        for (id, node) in &expected.nodes {
            assert_eq!(node.family, actual.nodes[id].family);
            assert_eq!(node.ports, actual.nodes[id].ports);
        }
    }
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
