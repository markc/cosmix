//! Mix Scenes host adapter. Validation and resolved ports belong exclusively to P1.
#[cfg(feature = "gate")]
mod gate;
mod render;
pub use render::{Events as SceneEvents, reconcile as reconcile_scene_mounts};

use bevy::prelude::*;
use cosmix_scene::{ResolvedScene, SceneDocument, Severity};
use cosmix_shell::core::{
    DialogSeat, DialogSeatError, DialogSlot, OutputKey, SubPanelRegistry, SubPanelSeat,
};
use cosmix_shell::runtime::{SceneVerb, ShellRuntimeSet};
use ctk::bus::BusBridge;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

/// `shell.scene.layout` geometry (scene-editor plan §4.3): `{scene, revision,
/// applied_revision, nodes:{id:{x,y,w,h,hidden}}, instances:{list:{item:
/// {x,y,w,h}}}}` in logical px relative to the scene's surface, measured from
/// the engine's last layout. `node` narrows to one document node (and, for a
/// list, its rows). The caller adds `visible` and `surface`.
pub fn scene_layout(world: &mut World, scene: &str, node: Option<&str>) -> Result<Value, Value> {
    render::layout(world, scene, node)
}

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
    rebuild_mount: bool,
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

    /// A dialog scene's reservation is the host's one dialog seat, held by
    /// this scene under the same loader receipt.
    fn holds_dialog_seat(&self, slot: &DialogSlot) -> bool {
        let Some(owner) = self.owner.as_ref() else {
            return false;
        };
        slot.seat().is_some_and(|seat| {
            seat.scene == self.tree.name
                && seat.owner == owner.citizen
                && seat.accepted_at == owner.accepted_at
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
    /// The host's one dialog seat (scene-editor plan §4.3 Q2). A dialog-kind
    /// load takes it here and never a carousel seat.
    pub(crate) dialog: DialogSlot,
    /// `shell.scene.changed` notices for displaced dialog holders, published
    /// by `dispatch` after the pre-empting load is accepted.
    notices: Vec<Value>,
}

impl SceneStore {
    /// The dialog seat, if a dialog scene holds it.
    pub fn dialog_seat(&self) -> Option<&DialogSeat> {
        self.dialog.seat()
    }

    /// `Some(true)` for a loaded dialog scene, `Some(false)` for a loaded
    /// edge scene, `None` when no scene of that name is loaded.
    pub fn is_dialog(&self, name: &str) -> Option<bool> {
        self.scenes.get(name).map(|entry| render::is_dialog(&entry.tree))
    }

    /// Carousel page id and edge of a loaded edge scene; `None` for a dialog
    /// or an unknown scene.
    pub fn edge_page(&self, name: &str) -> Option<(String, cosmix_shell::core::Edge)> {
        self.scenes
            .get(name)
            .filter(|entry| !render::is_dialog(&entry.tree))
            .map(|entry| (render::page_id(&entry.tree), render::scene_edge(&entry.tree)))
    }

    /// Move the seated dialog to `output` (it maps on the selected output
    /// when shown). False when `scene` does not hold the seat.
    pub fn retarget_dialog(&mut self, scene: &str, output: &OutputKey) -> bool {
        let Some(mut seat) = self.dialog.seat().filter(|seat| seat.scene == scene).cloned() else {
            return false;
        };
        if seat.output == *output {
            return true;
        }
        seat.output = output.clone();
        self.dialog.register_dialog(seat).is_ok()
    }

    /// Read-only inventory in scene-name order for the host output.
    /// `citizen` is authored routing metadata; `owner` is the verified loader.
    /// A dialog scene's row adds `kind:"dialog"` and has no page and no edge;
    /// it is `registered` while it holds the dialog seat. Edge rows are
    /// unchanged (no `kind`).
    pub fn list(&self, registry: &SubPanelRegistry, output: &OutputKey) -> Value {
        Value::Array(
            self.scenes
                .iter()
                .map(|(name, entry)| {
                    if render::is_dialog(&entry.tree) {
                        return json!({
                            "name": name,
                            "kind": "dialog",
                            "page": null,
                            "edge": null,
                            "citizen": entry.document.citizen,
                            "owner": entry.owner.as_ref().map(|owner| &owner.citizen),
                            "revision": entry.revision,
                            "applied_revision": entry.mounted.as_ref().map_or(0, |m| m.revision),
                            "diagnostics": entry.render_error.as_ref().map(|e| &e["diagnostics"]).cloned().unwrap_or_else(|| json!([])),
                            "model_generation": entry.model_generation,
                            "digest": digest(&entry.tree),
                            "registered": entry.holds_dialog_seat(&self.dialog),
                        });
                    }
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
        let result = self.request_mounted(verb, body, args, Some(mount));
        // A pre-empted dialog holder hears about it before the new holder's
        // own summary, so an owner never sees its successor first.
        for notice in std::mem::take(&mut self.notices) {
            let wire = format!("---\ncommand: shell.scene.changed\n---\n{notice}");
            if let Err(error) =
                bridge.try_publish_topic(format!("{}.scene.changed", bridge.service_name()), false, wire)
            {
                warn!("dialog pre-emption notice publish failed: {error}");
            }
        }
        match result {
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
                // A pre-empting dialog load displaces whoever holds the name
                // (accept/seat_dialog); another owner's model fence must not
                // turn that into a refusal, or a squatter bricks safe mode.
                let preempting = args["preempt_dialog"].as_bool() == Some(true)
                    && mount.is_some()
                    && cosmix_scene::resolve(&document).is_ok_and(|tree| render::is_dialog(&tree));
                if let Some(entry) = self.scenes.get(&document.name)
                    && entry.model_generation.is_some()
                    && !(preempting && !entry.is_model_authority(mount.as_deref()))
                    && (managed_generation.is_none() || !entry.is_model_authority(mount.as_deref()))
                {
                    return Err(model_authority_refusal(&document.name));
                }
                if managed_generation.is_some() && mount.is_none() {
                    return Err(model_authority_refusal(&document.name));
                }
                let name = document.name.clone();
                // The loader's envelope flag: a dialog load that must win
                // the seat (the Scene Editor), so a squatter cannot brick it.
                let preempt = args["preempt_dialog"].as_bool() == Some(true);
                let result = self.accept(document, mount, true, preempt)?;
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
                        || render::scene_edge(&result.tree) != render::scene_edge(&entry.tree)
                        || render::is_dialog(&result.tree) != render::is_dialog(&entry.tree)
                        || (render::is_dialog(&entry.tree)
                            && render::dialog_geometry(&result.tree) != render::dialog_geometry(&entry.tree)) {
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
                self.accept(document, mount, false, false)
            }
            SceneVerb::Unload => {
                let entry = self
                    .scenes
                    .remove(name)
                    .ok_or_else(|| json!({"error":"unknown scene"}))?;
                if render::is_dialog(&entry.tree) {
                    // Only this scene's own seat; a pre-empting successor's
                    // seat is not ours to free.
                    if entry.holds_dialog_seat(&self.dialog) {
                        let _ = self.dialog.release_dialog(name);
                    }
                } else if let Some(mount) = mount {
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

    /// Whether a Load of `source` would pass the document checks `accept`
    /// makes (parse, size, lint errors, resolve, templates, bindings). The
    /// ownership, seat and model-authority checks stay the load's own; this
    /// lets a host refuse to give a page up for a document that cannot take it.
    pub fn document_acceptable(source: &str) -> bool {
        let Ok(document) = cosmix_scene::parse(source) else {
            return false;
        };
        check_size(&document).is_ok()
            && !cosmix_scene::lint(&document)
                .iter()
                .any(|d| d.severity == Severity::Error)
            && cosmix_scene::resolve(&document)
                .is_ok_and(|tree| render::validate_templates(&tree).is_ok())
            && cosmix_scene::bindings::compile(&document).is_ok()
    }

    /// Whether a scene named `name`, or one mounting `page`, belongs to
    /// anyone but `owner` (an unowned entry counts as someone else's). A
    /// host fallback stands aside on this, not only on the seat: a seat can
    /// go (`sub.remove`) while the other owner's entry still names the page.
    pub fn claimed_by_other(&self, name: &str, page: &str, owner: &str) -> bool {
        self.scenes.iter().any(|(entry_name, entry)| {
            (entry_name == name || render::page_id(&entry.tree) == page)
                && entry
                    .owner
                    .as_ref()
                    .is_none_or(|entry_owner| entry_owner.citizen != owner)
        })
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
            if let Some(entry) = self.scenes.remove(name) {
                if entry.holds_dialog_seat(&self.dialog) {
                    let _ = self.dialog.release_dialog(name);
                }
                if let Some(mounted) = entry.mounted {
                    self.removed.push(mounted);
                }
            }
        }
        names
    }

    fn accept(
        &mut self,
        document: SceneDocument,
        mut mount: Option<&mut SceneMount<'_>>,
        loading: bool,
        preempt: bool,
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
        // A dialog takes no carousel page, so it is outside the page-id
        // namespace in both directions: an edge scene declaring
        // `panel:"scene-editor"` cannot squat the editor (Stage R, Opus M1).
        let page = render::page_id(&tree);
        let dialog = render::is_dialog(&tree);
        if !dialog
            && self.scenes.iter().filter(|(_, entry)| !render::is_dialog(&entry.tree)).any(
                |(name, entry)| {
                    (name != &tree.name && render::page_id(&entry.tree) == page)
                        || (name == &tree.name && render::page_id(&entry.tree) != page)
                },
            )
        {
            return Err(json!({"scene": tree.name, "error_code": "SUBPANEL_COLLISION",
                "error": "scene mount address is occupied or changed; unload before renaming"}));
        }
        // A dialog and an edge page are different mounts: a live scene
        // cannot switch between them any more than it can change edges. A
        // pre-empting dialog load displaces a same-name edge scene instead,
        // so an edge scene named `editor` cannot brick safe mode either.
        if self
            .scenes
            .get(&tree.name)
            .is_some_and(|old| render::is_dialog(&old.tree) != dialog)
        {
            let Some(caller) = mount.as_deref_mut().filter(|_| dialog && preempt) else {
                return Err(json!({"scene": tree.name, "error_code": "SUBPANEL_COLLISION",
                    "error": "a loaded scene cannot switch between dialog and edge; unload it first"}));
            };
            let old = self.scenes.remove(&tree.name).expect("checked above");
            caller.registry.forget(&render::page_id(&old.tree));
            if let Some(mounted) = old.mounted {
                self.removed.push(mounted);
            }
            self.notices.push(json!({
                "scene": tree.name,
                "revision": old.revision,
                "ops": ["unloaded"],
                "reason": "preempted",
                "by": {"scene": tree.name, "owner": caller.owner},
                "diagnostics": [],
            }));
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
            if dialog {
                self.seat_dialog(&tree, &owner, mount.output, preempt)?;
            } else {
                mount.registry.mount(
                    &render::page_id(&tree), mount.output.clone(), render::scene_edge(&tree),
                    &owner.citizen, owner.accepted_at,
                ).map_err(|error| json!({
                    "scene":tree.name, "error_code":"SUBPANEL_COLLISION", "error":error.to_string()
                }))?;
            }
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
        let rebuild_mount = old.as_ref().is_some_and(|old| old.rebuild_mount);
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
                rebuild_mount,
                mounted,
                owner,
                model_generation,
            },
        );
        Ok((reply, Some(summary)))
    }
}

impl SceneStore {
    /// Take (or keep) the dialog seat for `tree`. Refuses `DIALOG_BUSY` when
    /// another scene or owner holds it, unless the load pre-empts; then the
    /// displaced holder's scene is unloaded here and its notice queued. Last
    /// in `accept`: nothing after it can refuse, so a refused load leaves
    /// the incumbent untouched.
    fn seat_dialog(
        &mut self,
        tree: &ResolvedScene,
        owner: &SceneOwner,
        output: &OutputKey,
        preempt: bool,
    ) -> Result<(), Value> {
        let (w, h, title, chrome) = render::dialog_geometry(tree);
        let seat = DialogSeat {
            scene: tree.name.clone(),
            owner: owner.citizen.clone(),
            accepted_at: owner.accepted_at,
            output: output.clone(),
            w,
            h,
            title,
            chrome,
        };
        if !preempt {
            return self.dialog.register_dialog(seat).map_err(|error| match error {
                DialogSeatError::Busy { scene, owner } => json!({
                    "scene": tree.name, "error_code": "DIALOG_BUSY",
                    "message": "the dialog seat is held by another scene",
                    "holder": {"scene": scene, "owner": owner},
                }),
                other => json!({"scene": tree.name, "error_code": "SCENE_REFUSED",
                    "message": other.to_string()}),
            });
        }
        let displaced = self.dialog.preempt_dialog(seat).map_err(|error| {
            json!({"scene": tree.name, "error_code": "SCENE_REFUSED", "message": error.to_string()})
        })?;
        if let Some(displaced) = displaced {
            // A different scene is unloaded outright; the same scene under a
            // new owner is replaced by this load's own commit.
            if displaced.scene != tree.name
                && let Some(entry) = self.scenes.remove(&displaced.scene)
                && let Some(mounted) = entry.mounted
            {
                self.removed.push(mounted);
            }
            let revision = self.revisions.get(&displaced.scene).copied().unwrap_or(0);
            self.notices.push(json!({
                "scene": displaced.scene,
                "revision": revision,
                "ops": ["unloaded"],
                "reason": "preempted",
                "by": {"scene": tree.name, "owner": owner.citizen},
                "diagnostics": [],
            }));
        }
        Ok(())
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

    fn dialog_source(name: &str, w: u32) -> String {
        format!(
            "---\nscene: 1\nname: {name}\ncitizen: scene-{name}\nwindow: {{\"chrome\":true,\"h\":620,\"kind\":\"dialog\",\"title\":\"Scene Editor\",\"w\":{w}}}\n---\n```mix\nroot: {{widget: \"column\", children: [\"caption\"]}}\ncaption: {{widget: \"text\", text: \"hi\"}}\n```\n"
        )
    }

    fn load(
        store: &mut SceneStore,
        registry: &mut SubPanelRegistry,
        owner: &str,
        receipt: u64,
        source: &str,
        preempt: bool,
    ) -> Result<(Value, Option<Value>), Value> {
        let output = OutputKey::new("DP-1").unwrap();
        let mut mount = SceneMount { registry, output: &output, owner, accepted_at: receipt };
        let mut args = json!({"source": source, "model_generation": 3});
        if preempt {
            args["preempt_dialog"] = json!(true);
        }
        store.request_mounted(SceneVerb::Load, "", &args, Some(&mut mount))
    }

    #[test]
    fn a_dialog_load_takes_the_dialog_seat_never_an_edge_page() {
        let mut store = SceneStore::default();
        let mut registry = SubPanelRegistry::default();
        load(&mut store, &mut registry, "scenes", 1, &dialog_source("editor", 880), false).unwrap();
        let seat = store.dialog_seat().unwrap();
        assert_eq!((seat.scene.as_str(), seat.owner.as_str(), seat.w, seat.h), ("editor", "scenes", 880.0, 620.0));
        assert_eq!((seat.title.as_deref(), seat.chrome), (Some("Scene Editor"), true));
        assert!(registry.seat("scene-editor").is_none(), "no carousel seat, no right-edge page");
        assert_eq!(store.is_dialog("editor"), Some(true));
        assert_eq!(store.edge_page("editor"), None);
        let row = &store.list(&registry, &OutputKey::new("DP-1").unwrap())[0];
        assert_eq!((row["kind"].as_str(), row["edge"].is_null(), row["registered"].as_bool()), (Some("dialog"), true, Some(true)));
        // The same holder reloads (a new size) in place.
        load(&mut store, &mut registry, "scenes", 1, &dialog_source("editor", 900), false).unwrap();
        assert_eq!(store.dialog_seat().unwrap().w, 900.0);
        // A loaded dialog cannot become an edge page in place.
        let edge = dialog_source("editor", 880).replace(
            "{\"chrome\":true,\"h\":620,\"kind\":\"dialog\",\"title\":\"Scene Editor\",\"w\":880}",
            "{\"kind\":\"edge\",\"edge\":\"right\"}",
        );
        let error = load(&mut store, &mut registry, "scenes", 1, &edge, false).unwrap_err();
        assert_eq!(error["error_code"], "SUBPANEL_COLLISION");
        assert!(registry.seat("scene-editor").is_none());
    }

    #[test]
    fn a_second_dialog_is_busy_unless_it_preempts_and_the_incumbent_is_told() {
        let mut store = SceneStore::default();
        let mut registry = SubPanelRegistry::default();
        load(&mut store, &mut registry, "some-citizen", 1, &dialog_source("other-dialog", 400), false).unwrap();
        let before = store.scenes["other-dialog"].revision;
        let error = load(&mut store, &mut registry, "scenes", 2, &dialog_source("editor", 880), false).unwrap_err();
        assert_eq!(error["error_code"], "DIALOG_BUSY");
        assert_eq!(error["holder"], json!({"scene":"other-dialog","owner":"some-citizen"}));
        assert!(store.scenes.contains_key("other-dialog") && !store.scenes.contains_key("editor"));
        assert!(store.notices.is_empty());

        load(&mut store, &mut registry, "scenes", 3, &dialog_source("editor", 880), true).unwrap();
        assert_eq!(store.dialog_seat().unwrap().scene, "editor");
        assert!(!store.scenes.contains_key("other-dialog"), "the incumbent is unloaded");
        let fixture: Value = serde_json::from_str(include_str!(
            "../../../scripts/tests/fixtures/scene-editor/shell-verbs.json"
        ))
        .unwrap();
        let mut expected = fixture["shell.scene.load"]["preemption_notice"]["body"].clone();
        expected["revision"] = json!(before);
        assert_eq!(std::mem::take(&mut store.notices), vec![expected]);
        // Pre-empting again by the holder itself displaces nobody.
        load(&mut store, &mut registry, "scenes", 3, &dialog_source("editor", 880), true).unwrap();
        assert!(store.notices.is_empty());
    }

    fn edge_source(name: &str, panel: Option<&str>) -> String {
        let panel = panel.map_or_else(String::new, |panel| format!(",\"panel\":\"{panel}\""));
        format!(
            "---\nscene: 1\nname: {name}\ncitizen: squatter\nwindow: {{\"kind\":\"edge\",\"edge\":\"right\"{panel}}}\n---\n```mix\nroot: {{widget: \"column\", children: []}}\n```\n"
        )
    }

    /// Stage R (Opus M1): an edge scene on page `scene-editor` cannot squat
    /// the editor, because a dialog is outside the page-id namespace.
    #[test]
    fn an_edge_page_squatter_cannot_block_the_editor() {
        let mut store = SceneStore::default();
        let mut registry = SubPanelRegistry::default();
        load(&mut store, &mut registry, "someone", 1, &edge_source("squat", Some("scene-editor")), false)
            .unwrap();
        assert!(registry.seat("scene-editor").is_some());
        for preempt in [false, true] {
            load(&mut store, &mut registry, "scenes", 2, &dialog_source("editor", 880), preempt)
                .unwrap();
        }
        assert_eq!(store.dialog_seat().unwrap().scene, "editor");
        assert!(store.scenes.contains_key("squat"), "no conflict, nothing displaced");
        assert!(store.notices.is_empty());
        // Nor can a dialog take an edge page's address from it.
        assert!(registry.seat("scene-editor").is_some_and(|seat| seat.owner == "someone"));
    }

    /// Stage R (Opus M1): an edge scene named `editor`, even another owner's
    /// fenced one, is displaced by the pre-empting editor load; without the
    /// flag the switch is refused as before.
    #[test]
    fn a_name_squatter_is_displaced_only_by_a_preempting_dialog_load() {
        let mut store = SceneStore::default();
        let mut registry = SubPanelRegistry::default();
        load(&mut store, &mut registry, "someone", 1, &edge_source("editor", None), false).unwrap();
        assert!(registry.seat("scene-editor").is_some());
        let error =
            load(&mut store, &mut registry, "scenes", 2, &dialog_source("editor", 880), false)
                .unwrap_err();
        assert_eq!(error["error_code"], "SCENE_MODEL_AUTHORITY", "{error}");
        load(&mut store, &mut registry, "scenes", 3, &dialog_source("editor", 880), true).unwrap();
        assert_eq!(store.is_dialog("editor"), Some(true));
        assert_eq!(store.dialog_seat().unwrap().owner, "scenes");
        assert!(registry.seat("scene-editor").is_none(), "the squatter's page is released");
        let notices = std::mem::take(&mut store.notices);
        assert_eq!(notices.len(), 1);
        assert_eq!(notices[0]["reason"], "preempted");
        assert_eq!(notices[0]["by"], json!({"scene":"editor","owner":"scenes"}));
        assert_eq!(store.removed.len(), 0, "the squatter was never rendered in this fixture");
    }

    #[test]
    fn unload_and_owner_disconnect_release_the_dialog_seat() {
        let mut store = SceneStore::default();
        let mut registry = SubPanelRegistry::default();
        load(&mut store, &mut registry, "scenes", 1, &dialog_source("editor", 880), false).unwrap();
        let output = OutputKey::new("DP-1").unwrap();
        let mut mount = SceneMount { registry: &mut registry, output: &output, owner: "scenes", accepted_at: 2 };
        store
            .request_mounted(SceneVerb::Unload, "", &json!({"scene":"editor"}), Some(&mut mount))
            .unwrap();
        assert!(store.dialog_seat().is_none());

        load(&mut store, &mut registry, "scenes", 3, &dialog_source("editor", 880), false).unwrap();
        assert!(store.unload_owned_before("scenes", 3).is_empty(), "accepted at the cutoff stays");
        assert_eq!(store.unload_owned_before("scenes", 4), ["editor"]);
        assert!(store.dialog_seat().is_none());
        // The freed seat is anyone's.
        load(&mut store, &mut registry, "someone", 5, &dialog_source("other-dialog", 400), false).unwrap();
    }

    #[test]
    fn a_preempting_dispatch_publishes_the_incumbents_notice_first() {
        let (bridge, peer) = ctk::bus::test_bridge("shell");
        let mut store = SceneStore::default();
        let mut registry = SubPanelRegistry::default();
        let output = OutputKey::new("DP-1").unwrap();
        for (owner, receipt, name, preempt) in
            [("some-citizen", 1, "other-dialog", false), ("scenes", 2, "editor", true)]
        {
            let mut args = json!({"source": dialog_source(name, 880), "model_generation": 3});
            if preempt {
                args["preempt_dialog"] = json!(true);
            }
            let mut mount = SceneMount { registry: &mut registry, output: &output, owner, accepted_at: receipt };
            let (rc, body) = store.dispatch(SceneVerb::Load, "", &args, &bridge, &mut mount);
            assert_eq!(rc, 0, "{body}");
        }
        let changes: Vec<Value> = peer
            .drain_publishes()
            .iter()
            .filter(|publish| publish.headers.get("name").is_some_and(|n| n == "shell.scene.changed"))
            .map(|publish| serde_json::from_str(publish.body.split_once("\n---\n").unwrap().1).unwrap())
            .collect();
        let scenes: Vec<_> = changes.iter().map(|c| (c["scene"].as_str().unwrap(), c["reason"].as_str())).collect();
        assert_eq!(
            scenes,
            [("other-dialog", None), ("other-dialog", Some("preempted")), ("editor", None)],
            "the displaced owner hears before its successor's summary"
        );
        assert_eq!(changes[1]["by"], json!({"scene":"editor","owner":"scenes"}));
        assert!(store.notices.is_empty());
    }

    #[test]
    fn show_targets_the_selected_output() {
        let mut store = SceneStore::default();
        let mut registry = SubPanelRegistry::default();
        load(&mut store, &mut registry, "scenes", 1, &dialog_source("editor", 880), false).unwrap();
        let hdmi = OutputKey::new("HDMI-A-1").unwrap();
        assert!(store.retarget_dialog("editor", &hdmi));
        assert_eq!(store.dialog_seat().unwrap().output, hdmi);
        assert!(!store.retarget_dialog("other", &hdmi));
    }
}
