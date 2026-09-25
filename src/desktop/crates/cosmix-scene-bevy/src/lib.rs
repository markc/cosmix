//! Mix Scenes host adapter. Validation and resolved ports belong exclusively to P1.
#[cfg(feature = "gate")]
mod gate;
mod render;
pub use render::{Events as SceneEvents, reconcile as reconcile_scene_mounts};

use bevy::prelude::*;
use cosmix_scene::{ResolvedScene, SceneDocument, Severity};
use cosmix_shell::core::{OutputKey, SubPanelRegistry, SubPanelSeat};
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
    bindings: cosmix_scene::bindings::BindingSet,
    pub tree: ResolvedScene,
    revision: u64,
    prepared: render::PreparedLists,
    render_error: Option<Value>,
    pub mounted: Option<render::Mounted>,
    owner: Option<SceneOwner>,
    // Set by a loader's JSON load envelope; zero fences a stopped behaviour.
    // Authored citizen metadata never grants model-write authority.
    model_generation: Option<u64>,
}

impl SceneEntry {
    /// The reservation must match the loader, receipt, edge and host output.
    fn matching_seat<'a>(
        &self,
        registry: &'a SubPanelRegistry,
        output: &OutputKey,
    ) -> Option<&'a SubPanelSeat> {
        let owner = self.owner.as_ref()?;
        registry.seat(&render::page_id(&self.tree)).filter(|seat| {
            seat.owner == owner.citizen
                && seat.accepted_at == owner.accepted_at
                && seat.edge == render::scene_edge(&self.tree)
                && seat.output == *output
        })
    }
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
    /// Read-only inventory in scene-name order for the host output.
    /// `citizen` is authored routing metadata; `owner` is the verified loader.
    pub fn list(&self, registry: &SubPanelRegistry, output: &OutputKey) -> Value {
        Value::Array(
            self.scenes
                .iter()
                .map(|(name, entry)| {
                    let page = render::page_id(&entry.tree);
                    let seat = entry.matching_seat(registry, output);
                    json!({
                        "name": name,
                        "page": page,
                        "edge": seat.map(|seat| seat.edge.as_str()),
                        "citizen": entry.document.citizen,
                        "owner": entry.owner.as_ref().map(|owner| &owner.citizen),
                        "revision": entry.revision,
                        "applied_revision": entry.mounted.as_ref().map_or(0, |m| m.revision),
                        "diagnostics": entry.render_error.as_ref().map(|e| &e["diagnostics"]).cloned().unwrap_or_else(|| json!([])),
                        "model_generation": entry.model_generation,
                        "digest": digest(&entry.tree),
                        "registered": seat.is_some(),
                    })
                })
                .collect(),
        )
    }

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
                    if let Err(error) = bridge.try_publish_topic(format!("{}.scene.changed", bridge.service_name()), false, wire)
                    {
                        warn!("scene summary publish failed: {error}");
                    }
                }
                (0, reply.to_string())
            }
            Err(error) => {
                let error = refusal(error);
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
                    if let Err(error) = bridge.try_publish_topic(format!("{}.scene.changed", bridge.service_name()), false, wire)
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
            SceneVerb::Validate => {
                let document = cosmix_scene::parse(body).map_err(|d| json!({"diagnostics":d}))?;
                check_size(&document)?;
                let diagnostics = cosmix_scene::lint(&document);
                let tree = cosmix_scene::resolve(&document).map_err(|d| json!({"diagnostics":d}))?;
                render::validate_templates(&tree)?;
                Ok((json!({"scene":tree.name,"valid":true,"diagnostics":diagnostics}), None))
            }
            SceneVerb::Load => {
                let managed_generation = if args.get("model_generation").is_some() {
                    Some(args["model_generation"].as_u64().filter(|g| *g <= 9_007_199_254_740_990)
                        .ok_or_else(|| json!({"error_code":"SCENE_MODEL_GENERATION", "message":"model_generation must be an exact nonnegative integer"}))?)
                } else { None };
                let source = args["source"].as_str().unwrap_or(body);
                let document = cosmix_scene::parse(source).map_err(|d| json!({"diagnostics":d}))?;
                if let Some(entry) = self.scenes.get(&document.name)
                    && entry.model_generation.is_some()
                    && (managed_generation.is_none() || !entry.is_model_authority(mount.as_deref()))
                {
                    return Err(model_authority_refusal(&document.name));
                }
                if managed_generation.is_some() && mount.is_none() {
                    return Err(model_authority_refusal(&document.name));
                }
                let name = document.name.clone();
                let result = self.accept(document, mount, true)?;
                self.scenes.get_mut(&name).unwrap().model_generation = managed_generation;
                Ok(result)
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
                    json!({"scene":name,"revision":entry.revision,"digest":digest(&entry.tree),
                        "applied_revision":entry.mounted.as_ref().map_or(0, |m| m.revision),
                        "diagnostics":entry.render_error.as_ref().map(|e| &e["diagnostics"]).cloned().unwrap_or_else(|| json!([]))})
                } else if args["format"] == "source" {
                    json!({"scene":name,"revision":entry.revision,"source":cosmix_scene::to_source(&entry.document)})
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
                let path = args["path"].as_str().unwrap_or_default();
                let value = args.get("value")
                    .ok_or_else(|| json!({"error":"value is required"}))?;
                if path == "model" || path.starts_with("model.") {
                    if let Some(generation) = entry.model_generation
                        && (!entry.is_model_authority(mount.as_deref())
                            || generation == 0 || args["generation"].as_u64() != Some(generation))
                    {
                        return Err(model_authority_refusal(name));
                    }
                    let result = cosmix_scene::bindings::reevaluate(&entry.tree, &entry.bindings, path, value)
                        .map_err(|d| json!({"scene":name,"diagnostics":d}))?;
                    document.model = Some(result.tree.model.clone());
                    check_size(&document)?;
                    let prepared = render::validate_templates(&result.tree)?;
                    if render::page_id(&result.tree) != render::page_id(&entry.tree)
                        || render::scene_edge(&result.tree) != render::scene_edge(&entry.tree) {
                        return Err(json!({"scene":name,"error_code":"SUBPANEL_COLLISION",
                            "message":"model patch cannot move a scene mount; unload before moving it"}));
                    }
                    // Commit only after all validation. Keep compiled bindings,
                    // the loader receipt and last-good ports from reevaluate.
                    let entry = self.scenes.get_mut(name).unwrap();
                    let revision = self.revisions.entry(name.into()).or_default();
                    *revision += 1;
                    entry.revision = *revision;
                    entry.document = document;
                    entry.tree = result.tree;
                    entry.prepared = prepared;
                    entry.render_error = None;
                    let reply = json!({"scene":name,"revision":*revision,"digest":digest(&entry.tree)});
                    let summary = json!({"scene":name,"revision":*revision,"ops":result.changed.len(),"diagnostics":result.diagnostics});
                    return Ok((reply, Some(summary)));
                }
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
                check_size(&document)?;
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
        check_size(&document)?;
        let diagnostics = cosmix_scene::lint(&document);
        if diagnostics.iter().any(|d| d.severity == Severity::Error) {
            return Err(json!({"scene":document.name,"diagnostics":diagnostics}));
        }
        let tree = cosmix_scene::resolve(&document).map_err(|d| json!({"diagnostics":d}))?;
        let prepared = render::validate_templates(&tree)?;
        let bindings = cosmix_scene::bindings::compile(&document).map_err(|d| json!({"diagnostics":d}))?;
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
        let old = self.scenes.remove(&tree.name);
        let model_generation = old.as_ref().and_then(|old| old.model_generation);
        let mounted = old.and_then(|old| old.mounted);
        self.scenes.insert(
            tree.name.clone(),
            SceneEntry {
                document,
                bindings,
                tree,
                revision,
                prepared,
                render_error: None,
                mounted,
                owner,
                model_generation,
            },
        );
        Ok((reply, Some(summary)))
    }
}

impl SceneEntry {
    fn is_model_authority(&self, mount: Option<&SceneMount<'_>>) -> bool {
        self.owner.as_ref().zip(mount)
            .is_some_and(|(owner, caller)| owner.citizen == caller.owner)
    }
}

fn model_authority_refusal(name: &str) -> Value {
    json!({"scene":name, "error_code":"SCENE_MODEL_AUTHORITY",
        "message":"managed model writes require the loader and its current generation; use scenes.model"})
}

fn digest(tree: &ResolvedScene) -> String {
    format!("{:x}", Sha256::digest(serde_json::to_vec(tree).unwrap()))
}

#[cfg(test)]
pub(crate) use cosmix_scene::to_source as serialised_document;

fn check_size(document: &SceneDocument) -> Result<(), Value> {
    if cosmix_scene::to_source(document).len() > cosmix_scene::MAX_DOCUMENT_BYTES {
        return Err(json!({"scene":document.name,"error_code":"DOCUMENT_TOO_LARGE",
            "message":"aggregate document and model exceeds 256 KiB",
            "diagnostics":[{"severity":"Error","code":"document-too-large","line":1,
                "message":"aggregate document and model exceeds 256 KiB"}]}));
    }
    Ok(())
}

fn refusal(mut error: Value) -> Value {
    if error.get("error_code").is_none() {
        error["error_code"] = json!("SCENE_REFUSED");
    }
    if error.get("message").is_none() {
        error["message"] = error.get("error").cloned()
            .unwrap_or_else(|| json!("scene validation failed"));
    }
    error
}

#[cfg(test)]
mod tests {
    use super::*;
    const FIXTURE: &str = include_str!("../../cosmix-scene/tests/fixtures/conformance.scene.md");

    const MODEL_SCENE: &str = r#"---
scene: 1
name: model-test
citizen: scene-behaviour
model: {"caption":"first","rows":[{"id":"one","cells":["one"]}]}
---
```mix
root: {widget: "column", children: ["caption", "rows"]}
caption: {widget: "text", text: "= $model.caption"}
rows: {widget: "list", rows: "= $model.rows", row: "item", row_height: 24}
item: {widget: "text", text: "{cells[0]}"}
```
"#;

    #[test]
    fn managed_model_writes_require_loader_and_current_generation() {
        let mut store = SceneStore::default();
        let output = OutputKey::new("test-output").unwrap();
        let mut registry = SubPanelRegistry::default();
        let mut mount = SceneMount {
            registry: &mut registry, output: &output, owner: "scenes", accepted_at: 1,
        };
        let load = json!({"source": MODEL_SCENE, "model_generation": 7});
        store.request_mounted(SceneVerb::Load, "", &load, Some(&mut mount)).unwrap();
        let patch = json!({"scene":"model-test", "path":"model.caption", "value":"current", "generation":7});
        store.request_mounted(SceneVerb::Patch, "", &patch, Some(&mut mount)).unwrap();
        assert_eq!(store.scenes["model-test"].tree.nodes["caption"].ports["text"], "current");

        // Equal tokens confer no authority to a direct behaviour or mesh caller.
        for caller in ["scene-model-test", "peer/editor"] {
            mount.owner = caller;
            let before = store.scenes["model-test"].tree.clone();
            let revision = store.scenes["model-test"].revision;
            for path in ["model", "model.caption"] {
                let mut request = patch.clone();
                request["path"] = json!(path);
                request["value"] = if path == "model" { json!({"caption":"bad"}) } else { json!("bad") };
                let error = store.request_mounted(SceneVerb::Patch, "", &request, Some(&mut mount)).unwrap_err();
                assert_eq!(error["error_code"], "SCENE_MODEL_AUTHORITY");
                assert_eq!(store.scenes["model-test"].tree, before);
                assert_eq!(store.scenes["model-test"].revision, revision);
            }
            assert!(store.request_mounted(SceneVerb::Load, MODEL_SCENE, &Value::Null, Some(&mut mount)).is_err());
            assert!(store.request_mounted(SceneVerb::Load, "", &load, Some(&mut mount)).is_err());
        }
        mount.owner = "scenes";
        // A raw load cannot accidentally strip an existing fence, even by its owner.
        assert!(store.request_mounted(SceneVerb::Load, MODEL_SCENE, &Value::Null, Some(&mut mount)).is_err());
        store.request_mounted(SceneVerb::Load, "", &json!({"source":MODEL_SCENE,"model_generation":8}), Some(&mut mount)).unwrap();
        let revision = store.scenes["model-test"].revision;
        for generation in [Value::Null, json!(7), json!("8")] {
            let mut stale = patch.clone();
            stale["generation"] = generation;
            assert!(store.request_mounted(SceneVerb::Patch, "", &stale, Some(&mut mount)).is_err());
            assert_eq!(store.scenes["model-test"].revision, revision);
        }
        let mut current = patch;
        current["generation"] = json!(8);
        store.request_mounted(SceneVerb::Patch, "", &current, Some(&mut mount)).unwrap();
        store.request_mounted(SceneVerb::Patch, "", &json!({"scene":"model-test","path":"caption.text","value":"layout"}), Some(&mut mount)).unwrap();
        assert_eq!(store.scenes["model-test"].model_generation, Some(8));
        store.request_mounted(SceneVerb::Load, "", &json!({"source":MODEL_SCENE,"model_generation":0}), Some(&mut mount)).unwrap();
        current["generation"] = json!(0);
        assert!(store.request_mounted(SceneVerb::Patch, "", &current, Some(&mut mount)).is_err());
    }

    #[test]
    fn model_patch_is_transactional_and_retains_bindings_and_last_good_values() {
        let mut store = SceneStore::default();
        store.request(SceneVerb::Load, MODEL_SCENE, &Value::Null).unwrap();
        let bindings = store.scenes["model-test"].bindings.clone();
        store.request(SceneVerb::Patch, "", &json!({"scene":"model-test","path":"model.caption","value":"second"})).unwrap();
        assert_eq!(store.scenes["model-test"].tree.nodes["caption"].ports["text"], "second");
        assert_eq!(store.scenes["model-test"].document.model.as_ref().unwrap()["caption"], "second");
        assert_eq!(store.scenes["model-test"].bindings, bindings);
        // A failed expression keeps its last-good port, while accepting model
        // data and returning an evaluation diagnostic, per core binding policy.
        let (_, summary) = store.request(SceneVerb::Patch, "", &json!({"scene":"model-test","path":"model","value":{"caption":7,"rows":[]}})).unwrap();
        assert_eq!(store.scenes["model-test"].tree.nodes["caption"].ports["text"], "second");
        assert!(!summary.unwrap()["diagnostics"].as_array().unwrap().is_empty());
        let before = store.scenes["model-test"].tree.clone();
        let revision = store.scenes["model-test"].revision;
        for (path, value) in [("model..caption", json!("bad")), ("model.rows.child", json!("bad")), ("model", json!(7)),
            ("model", json!({"large":"x".repeat(cosmix_scene::MAX_DOCUMENT_BYTES)}))] {
            assert!(store.request(SceneVerb::Patch, "", &json!({"scene":"model-test","path":path,"value":value})).is_err());
            assert_eq!(store.scenes["model-test"].tree, before);
            assert_eq!(store.scenes["model-test"].revision, revision);
        }
    }

    #[test]
    fn cumulative_model_bounds_source_export_and_validation_have_no_side_effects() {
        let mut store = SceneStore::default();
        store.request(SceneVerb::Validate, MODEL_SCENE, &Value::Null).unwrap();
        assert!(store.scenes.is_empty());
        assert!(store.revisions.is_empty());
        store.request(SceneVerb::Load, MODEL_SCENE, &Value::Null).unwrap();
        store.request(SceneVerb::Patch, "", &json!({"scene":"model-test","path":"model.a","value":"x".repeat(150_000)})).unwrap();
        let revision = store.scenes["model-test"].revision;
        assert!(store.request(SceneVerb::Patch, "", &json!({"scene":"model-test","path":"model.b","value":"x".repeat(150_000)})).is_err());
        assert_eq!(store.scenes["model-test"].revision, revision);
        let (export, _) = store.request(SceneVerb::Get, "", &json!({"scene":"model-test","format":"source"})).unwrap();
        let document = cosmix_scene::parse(export["source"].as_str().unwrap()).unwrap();
        assert_eq!(document.nodes["caption"].ports["text"], "= $model.caption");
        assert_eq!(document.model, store.scenes["model-test"].document.model);
        assert_eq!(document.nodes["item"].ports["text"], "{cells[0]}");
        // Authored behaviour is event routing only; it cannot revoke loader seats.
        store.scenes.get_mut("model-test").unwrap().owner = Some(SceneOwner {citizen:"scenes".into(), accepted_at:1});
        assert!(store.unload_owned_by("scene-behaviour").is_empty());
        assert!(store.scenes.contains_key("model-test"));
    }

    #[test]
    fn model_patch_cannot_move_a_live_mount_address() {
        let mut store = SceneStore::default();
        let source = "---\nscene: 1\nname: edge-binding\ncitizen: behaviour\nmodel: {\"edge\":\"right\"}\n---\n```mix\nroot: {widget: \"window\", kind: \"edge\", edge: \"= $model.edge\"}\n```\n";
        store.request(SceneVerb::Load, source, &Value::Null).unwrap();
        let before = store.scenes["edge-binding"].tree.clone();
        let revision = store.scenes["edge-binding"].revision;
        let error = store.request(SceneVerb::Patch, "", &json!({"scene":"edge-binding", "path":"model.edge", "value":"left"})).unwrap_err();
        assert_eq!(error["error_code"], "SUBPANEL_COLLISION");
        assert_eq!(store.scenes["edge-binding"].tree, before);
        assert_eq!(store.scenes["edge-binding"].revision, revision);
    }

    #[test]
    fn inventory_requires_the_render_reservation_and_reports_unowned_watch() {
        use cosmix_shell::core::Edge;

        let mut store = SceneStore::default();
        store
            .request(SceneVerb::Load, FIXTURE, &Value::Null)
            .unwrap();
        let output = OutputKey::new("test-output").unwrap();
        let mut registry = SubPanelRegistry::default();
        let watched = store
            .request(SceneVerb::Watch, "", &json!({"scene":"conformance"}))
            .unwrap()
            .0;
        let row = &store.list(&registry, &output)[0];
        assert_eq!(row["owner"], Value::Null);
        assert_eq!(row["registered"], false);
        assert_eq!(row["revision"], watched["revision"]);
        assert_eq!(row["digest"], watched["digest"]);

        let entry = store.scenes.get_mut("conformance").unwrap();
        entry.owner = Some(SceneOwner {
            citizen: "loader".into(),
            accepted_at: 7,
        });
        let page = render::page_id(&entry.tree);
        let edge = render::scene_edge(&entry.tree);
        let other_edge = if edge == Edge::Left {
            Edge::Right
        } else {
            Edge::Left
        };
        for (owner, receipt, seat_edge, seat_output, registered) in [
            ("loader", 7, edge, output.clone(), true),
            ("other", 7, edge, output.clone(), false),
            ("loader", 8, edge, output.clone(), false),
            ("loader", 7, other_edge, output.clone(), false),
            (
                "loader",
                7,
                edge,
                OutputKey::new("other-output").unwrap(),
                false,
            ),
        ] {
            registry.forget(&page);
            registry
                .mount(&page, seat_output, seat_edge, owner, receipt)
                .unwrap();
            let rows = store.list(&registry, &output);
            assert_eq!(rows[0]["registered"], registered);
            assert_eq!(
                rows[0]["edge"],
                if registered {
                    json!(edge.as_str())
                } else {
                    Value::Null
                }
            );
            assert_eq!(rows[0]["owner"], "loader");
        }
    }

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
