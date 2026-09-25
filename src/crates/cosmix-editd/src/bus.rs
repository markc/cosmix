//! Bus wiring: supervised connect with provenance and the verb manifest
//! (template: `cosmix-nspawnd/src/bus.rs` connect; NOT its Authorizer and NOT
//! its concurrent pump — commands route to the router and per-buffer actors).
//! `HELP` is answered by cosmix-lib-client from the manifest.

use std::sync::Arc;

use cosmix_client::SupervisedClient;
use cosmix_edit_core::wire::{SERVICE, VERBS};

/// Declared args per verb (the manifest's `args` column).
fn verb_args(verb: &str) -> &'static [&'static str] {
    match verb {
        "edit.open" => &["path", "create", "language", "origin"],
        "edit.close" => &["buffer", "force", "origin", "op_id"],
        "edit.save" => &["buffer", "path", "expect_rev", "force", "origin", "op_id"],
        "edit.reload" => &["buffer", "force", "expect_rev", "origin", "op_id"],
        "edit.get" => &["buffer", "range", "numbered", "expect_rev", "snapshot"],
        "edit.insert" => &["buffer", "at", "text", "expect_rev", "base_rev", "coalesce", "cursor", "origin", "op_id"],
        "edit.delete" => &["buffer", "range", "expect_rev", "base_rev", "coalesce", "cursor", "origin", "op_id"],
        "edit.replace" => &["buffer", "range", "text", "expect_rev", "base_rev", "coalesce", "cursor", "origin", "op_id"],
        "edit.apply" => &["buffer", "ops", "expect_rev", "base_rev", "coalesce", "cursor", "origin", "op_id"],
        "edit.find" => &["buffer", "pattern", "regex", "case", "range", "groups", "limit", "from"],
        "edit.select" => &["buffer", "ranges", "origin", "op_id"],
        "edit.cursor" => &["buffer", "at", "origin", "op_id"],
        "edit.anchor.set" => &["buffer", "name", "at", "range", "bias", "origin", "op_id"],
        "edit.anchor.get" => &["buffer", "name"],
        "edit.anchor.clear" => &["buffer", "name", "origin", "op_id"],
        "edit.undo" | "edit.redo" => &["buffer", "origin", "as", "expect_rev", "op_id"],
        "edit.history" => &["buffer", "since_rev", "limit"],
        "edit.props.get" | "edit.props.describe" => &["path"],
        _ => &[],
    }
}

fn verb_description(verb: &str) -> &'static str {
    match verb {
        "edit.ping" => "Liveness",
        "edit.info" => "Build, epoch, counts and limits",
        "edit.list" => "Open buffers",
        "edit.open" => "Open (or reopen) a file, or a scratch buffer",
        "edit.close" => "Release this caller's hold; free when unheld and clean",
        "edit.save" => "Atomic save (or save-as) with a revalidated disk precondition",
        "edit.reload" => "Re-read from disk as an undoable tool:disk edit",
        "edit.get" => "Read text (paged, optionally numbered or snapshot-pinned)",
        "edit.insert" => "Insert text",
        "edit.delete" => "Delete a range",
        "edit.replace" => "Replace a range",
        "edit.apply" => "Apply several ops as one transaction",
        "edit.find" => "Literal or regex search",
        "edit.select" => "Set this caller's selections",
        "edit.cursor" => "Set this caller's caret",
        "edit.anchor.set" => "Set a named anchor that moves with edits",
        "edit.anchor.get" => "Read named anchors",
        "edit.anchor.clear" => "Remove a named anchor",
        "edit.undo" => "Undo the newest group of a lane (default: own)",
        "edit.redo" => "Redo the newest undone group of a lane",
        "edit.history" => "Read the op log",
        "edit.props.get" => "SPEC-07 property read",
        "edit.props.list" => "SPEC-07 property paths",
        "edit.props.describe" => "SPEC-07 property description",
        "edit.props.watch" => "Topics to subscribe for changes",
        _ => "",
    }
}

/// The verb manifest, derived from the frozen `wire::VERBS` list.
pub fn verb_manifest() -> Vec<cosmix_bus::VerbDescriptor> {
    let mut verbs = vec![cosmix_bus::VerbDescriptor::new(
        "HELP",
        &[],
        "List all commands this service accepts",
        true,
    )];
    for (verb, read_only) in VERBS {
        verbs.push(cosmix_bus::VerbDescriptor::new(verb, verb_args(verb), verb_description(verb), *read_only));
    }
    verbs
}

/// Supervised connect registering `edit` with provenance and the manifest.
pub async fn connect() -> Result<Arc<SupervisedClient>, String> {
    let build = cosmix_buildinfo::build_info!();
    let provenance = cosmix_bus::RegisterProvenance::from_parts(
        build.pkg,
        build.version,
        build.git_sha,
        build.git_dirty,
        build.build_time,
        cosmix_buildinfo::now_rfc3339(),
    );
    SupervisedClient::connect_options(SERVICE, &cosmix_config::client_helpers::resolve_noded_url())
        .with_provenance(provenance)
        .with_verbs(verb_manifest())
        .connect()
        .await
        .map(Arc::new)
        .map_err(|error| format!("connecting supervised Bus client: {error}"))
}

/// `cosmix-editd serve`. Stage S skeleton: refuses to run.
pub async fn serve() -> anyhow::Result<()> {
    anyhow::bail!("cosmix-editd 0.1.0 is a Stage S skeleton: serve is not implemented yet (ced E0b)")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_covers_every_verb_with_a_description() {
        let manifest = verb_manifest();
        assert_eq!(manifest.len(), VERBS.len() + 1);
        for (verb, _) in VERBS {
            assert!(!verb_description(verb).is_empty(), "{verb}: no description");
        }
    }
}
