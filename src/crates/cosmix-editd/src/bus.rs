//! Bus wiring: supervised connect with provenance and the verb manifest
//! (template: `cosmix-nspawnd/src/bus.rs` connect; NOT its Authorizer and NOT
//! its concurrent pump — commands route to the router and per-buffer actors).
//! `HELP` is answered by cosmix-lib-client from the manifest.

use std::sync::Arc;
use std::time::Duration;

use cosmix_client::{ConnState, SupervisedClient};
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
        "edit.recovery.flush" => "Reply once every queued recovery record and repair is durable",
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

/// `cosmix-editd serve`: register `edit`, signal READY=1 once, serve until
/// SIGTERM/SIGINT, then flush the recovery files, log every dirty buffer and
/// exit within `SHUTDOWN_BUDGET_MS`. Restores recovery files first.
pub async fn serve() -> anyhow::Result<()> {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .try_init();
    let config = crate::router::Config::from_env();
    tracing::info!(
        "cosmix-editd: epoch {}, mesh access {}",
        config.epoch,
        if config.mesh_open { "open" } else { "locked (COSMIX_MESH_OPEN=0: mutations node-local only)" }
    );
    // Recovery files are restored BEFORE registration (ced E1 plan §5.2): the
    // scan, quarantine, salvage, sweep and new-generation switches all happen
    // here; the buffers go live below before the first command is served and
    // before READY=1.
    let recovery = crate::recovery::RecoveryConfig::from_env();
    let recovered = match (recovery.enabled, recovery.dir.clone()) {
        (true, Some(dir)) => {
            let epoch = config.epoch.clone();
            let caps = crate::recovery::RestoreCaps {
                max_buffers: crate::limits::MAX_BUFFERS,
                max_bytes: config.budget_cap,
            };
            let (rec, restored) =
                tokio::task::spawn_blocking(move || crate::recovery::Recovery::start(&dir, &epoch, caps)).await?;
            tracing::info!(
                "cosmix-editd: recovery files in {} ({} buffer(s) restored)",
                rec.dir().display(),
                restored.len()
            );
            Some((rec, restored))
        }
        _ => {
            tracing::warn!("cosmix-editd: recovery files disabled (COSMIX_EDIT_RECOVERY=0 or no state directory); buffers are volatile");
            None
        }
    };
    let client = connect().await.map_err(anyhow::Error::msg)?;
    let mut incoming = client.incoming().ok_or_else(|| anyhow::anyhow!("the Bus incoming stream was already taken"))?;
    let sink = Arc::new(crate::events::BusSink(client.clone()));
    let editd = match recovered {
        Some((rec, restored)) => crate::router::Editd::start_recovering(config, rec, restored, sink).await,
        None => crate::router::Editd::start(config, sink),
    };
    // Registered, and every restored buffer is live: `edit` is callable now.
    crate::readiness::notify_ready();

    // Every reconnect edge owes mirrors a `resync all` (events published while
    // disconnected were dropped and are already owed individually).
    let mut state = client.subscribe_state();
    let publisher = editd.publisher().clone();
    tokio::spawn(async move {
        let mut connected = *state.borrow() == ConnState::Connected;
        while state.changed().await.is_ok() {
            let now = *state.borrow_and_update() == ConnState::Connected;
            if now && !connected {
                publisher.reconnected();
            }
            connected = now;
        }
    });

    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut sigint = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
    loop {
        tokio::select! {
            cmd = incoming.recv() => {
                let Some(cmd) = cmd else { break };
                if cmd.command.is_empty() {
                    continue; // a topic delivery, not a verb
                }
                // Routing happens here, in receive order; only the wait is spawned.
                let reply = editd.submit(&cmd);
                let client = client.clone();
                tokio::spawn(async move {
                    let (rc, body) = reply.await;
                    match tokio::time::timeout(Duration::from_secs(10), client.respond(&cmd, rc, &body)).await {
                        Ok(Ok(())) => {}
                        Ok(Err(error)) => tracing::warn!("cosmix-editd: reply to {} failed: {error}", cmd.command),
                        Err(_) => tracing::warn!("cosmix-editd: reply to {} timed out", cmd.command),
                    }
                });
            }
            _ = sigterm.recv() => break,
            _ = sigint.recv() => break,
        }
    }

    let budget = Duration::from_millis(crate::limits::SHUTDOWN_BUDGET_MS);
    let shutdown = async {
        // Drain the recovery queue, finish repairs and sync (plan §5.1: 0 loss on SIGTERM).
        let kept = match editd.recovery_flush().await {
            Some(flushed) if flushed.synced => true,
            Some(_) => {
                tracing::error!("cosmix-editd: recovery files could not be fully synced at shutdown");
                false
            }
            None => false,
        };
        for dirty in editd.dirty_buffers().await {
            tracing::warn!(
                "cosmix-editd: {} unsaved buffer {} ({}) at rev {}",
                if kept { "keeping (in recovery files)" } else { "discarding" },
                dirty.buffer,
                dirty.path.as_deref().unwrap_or("scratch"),
                dirty.rev
            );
        }
        if let Err(error) = client.deregister().await {
            tracing::warn!("cosmix-editd: deregister failed: {error}");
        }
        client.shutdown().await;
    };
    if tokio::time::timeout(budget - Duration::from_millis(500), shutdown).await.is_err() {
        tracing::warn!("cosmix-editd: shutdown budget spent; exiting anyway");
    }
    Ok(())
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
