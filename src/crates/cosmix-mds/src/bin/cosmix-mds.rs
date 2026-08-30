//! `cosmix-mds` operator binary.
//!
//! Argparse / output / exit-code conventions are pinned in
//! `_doc/2026-05-02-cosmix-mds-cli.md`. Phase 5.1 ships the
//! scaffold + `list-sets`; subsequent commits add the rest.

use clap::{Args, Parser, Subcommand, ValueEnum};
use cosmix_mds::{
    ChangelogStream, ContainerId, ExportReport, GcReport, ImportReport, Mds, MdsStats, PerSetStats,
    PruneReport, RebuildReport, SetId, SqliteCasMds, VerifyReport, VerifyScope,
};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::SystemTime;

const ENV_ROOT: &str = "COSMIX_MDS_ROOT";

/// Exit codes per the CLI doc:
///   0 = success (including findings — verify mismatches, gc
///       deletions, dry-run pending work),
///   2 = failure (I/O, schema, malformed args, missing root, etc.),
///   1 reserved for a future "succeeded with findings worth gating
///   on" semantic that v1 deliberately does not use.
const EXIT_OK: u8 = 0;
const EXIT_FAIL: u8 = 2;

#[derive(Parser, Debug)]
#[command(
    name = "cosmix-mds",
    about = "Operator surface for the cosmix metadata store",
    version
)]
pub struct Cli {
    /// MDS root directory. Falls back to $COSMIX_MDS_ROOT.
    /// Required: there is no implicit default — accidental local
    /// roots on a tool that mutates persistent storage is a bad
    /// failure mode.
    #[arg(long, global = true, value_name = "PATH")]
    pub root: Option<PathBuf>,

    /// Emit reports as JSON instead of human-readable.
    #[arg(long, global = true)]
    pub json: bool,

    /// Suppress non-error stderr output.
    #[arg(short, long, global = true)]
    pub quiet: bool,

    /// Verbose tracing to stderr.
    #[arg(short, long, global = true)]
    pub verbose: bool,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Enumerate every container set under the MDS root.
    ListSets,
    /// Open the MDS root, applying schema migrations on every set.
    MigrateAll,
    /// Report counters across the MDS root.
    Stats {
        /// Append a per-set breakdown after the global summary.
        #[arg(long)]
        per_set: bool,
    },
    /// Recompute BLAKE3 over CAS blobs and update the verify ledger.
    Verify(VerifyArgs),
    /// Two-pass garbage-collect unreferenced blobs from the CAS.
    Gc {
        /// Run both passes (with re-checks) but skip the unlinks
        /// and row deletes. Counters report what *would* have
        /// been done.
        #[arg(long)]
        dry_run: bool,
    },
    /// Rebuild blobs.sqlite from per-set data.sqlite + CAS.
    RebuildIndex,
    /// Export a set (data.sqlite + referenced CAS blobs) to a tar archive.
    Export {
        /// Container set UUID to export.
        set_id: String,
        /// Destination tarball path.
        tarball: PathBuf,
    },
    /// Import a set from a tar archive produced by `export`.
    /// The set UUID is taken from the tarball's manifest, not
    /// the command line.
    Import {
        /// Source tarball path.
        tarball: PathBuf,
    },
    /// Trim a per-set changelog stream to its newest `keep_n` rows,
    /// advancing the durable retention floor. Clients whose
    /// `sinceState` references a pruned seq get the JMAP
    /// `cannotCalculateChanges` rejection from
    /// `cosmix-maild::mailstore::mailbox_changes`.
    ///
    /// MCS Phase 3 (2026-05-14). See
    /// `_doc/planned/mailbox-changes-substrate.md`.
    PruneChangelog {
        /// Container set UUID to prune within.
        set_id: String,
        /// Target stream: `container-change-set` (lifecycle) or
        /// `set-change` (item events).
        #[arg(long, value_enum)]
        stream: PruneStreamArg,
        /// Number of newest rows to retain. `0` prunes everything.
        #[arg(long, value_name = "N")]
        keep_n: u64,
    },
}

/// CLI-facing analog of [`ChangelogStream`]. Two-step indirection
/// because `ChangelogStream` is the public type and we don't want to
/// derive `ValueEnum` on it (pulls a clap dep into the lib crate).
#[derive(ValueEnum, Debug, Clone, Copy)]
pub enum PruneStreamArg {
    /// `container_change_set` (v1.4) — lifecycle half of
    /// JMAP `Mailbox/changes`.
    ContainerChangeSet,
    /// `set_change` (v1.1) — JMAP `Email/changes` + count half of
    /// `Mailbox/changes`.
    SetChange,
}

impl From<PruneStreamArg> for ChangelogStream {
    fn from(v: PruneStreamArg) -> Self {
        match v {
            PruneStreamArg::ContainerChangeSet => ChangelogStream::ContainerChangeSet,
            PruneStreamArg::SetChange => ChangelogStream::SetChange,
        }
    }
}

/// Scope flags for `verify`. Mutually exclusive — clap enforces
/// the multiple-of-true case at parse time. With none set, the
/// default is `--full` (matches the operator-doc default).
#[derive(Args, Debug)]
pub struct VerifyArgs {
    /// Verify every blob row in `blobs.sqlite` (default).
    #[arg(long, group = "scope")]
    pub full: bool,
    /// Re-verify any blob whose newest verify ledger entry
    /// predates `<DURATION>` ago, plus blobs never verified.
    /// Parsed by `humantime` (`24h`, `7d`, `1h30m`, `30m`).
    #[arg(long, value_name = "DURATION", group = "scope")]
    pub since: Option<String>,
    /// Verify only blobs referenced from items in the named
    /// container UUID (scans every set's `data.sqlite`).
    #[arg(long, value_name = "UUID", group = "scope")]
    pub container: Option<String>,
}

#[derive(serde::Serialize)]
struct ListSetsResponse {
    sets: Vec<String>,
}

#[derive(serde::Serialize)]
struct MigrateAllResponse {
    sets: u64,
    errors: u64,
}

/// Wire shape for `stats`. The `sets` field is `null` by default
/// and an array under `--per-set`. Keeping a single response
/// struct (rather than two distinct schemas) means JSON consumers
/// don't have to branch on flag — they always read the same
/// top-level fields and conditionally walk `sets` if present.
#[derive(serde::Serialize)]
struct StatsResponse {
    set_count: u64,
    container_count: u64,
    item_count: u64,
    blob_count: u64,
    total_bytes: u64,
    dedup_ratio: f64,
    /// `null` by default, an array under `--per-set`. Kept on the
    /// wire as an explicit `null` (no `skip_serializing_if`) so JSON
    /// consumers can read a single stable schema and branch on
    /// whether `sets` is null vs an array, rather than on whether
    /// the key is present.
    sets: Option<Vec<PerSetStatsResponse>>,
}

#[derive(serde::Serialize)]
struct PerSetStatsResponse {
    set_id: String,
    container_count: u64,
    item_count: u64,
    blob_count: u64,
    total_bytes: u64,
}

impl From<&PerSetStats> for PerSetStatsResponse {
    fn from(s: &PerSetStats) -> Self {
        Self {
            set_id: s.set_id.0.to_string(),
            container_count: s.container_count,
            item_count: s.item_count,
            blob_count: s.blob_count,
            total_bytes: s.total_bytes,
        }
    }
}

/// Wire shape for `gc`. Mirrors `GcReport` field-for-field with
/// `duration` flattened to `duration_ms: u64`. `dry_run` is
/// surfaced explicitly on the wire so consumers don't have to
/// re-derive it from the invocation flags.
#[derive(serde::Serialize)]
struct GcResponse {
    dry_run: bool,
    blobs_deleted: u64,
    bytes_freed: u64,
    duration_ms: u64,
    candidates_pass1: u64,
    skipped_re_referenced: u64,
    skipped_re_touched: u64,
    orphan_rows_swept: u64,
    pending_rows_observed: u64,
}

/// Wire shape for `rebuild-index`. `duration` is flattened to
/// `duration_ms: u64` for the same reasons as the other reports
/// (see `VerifyResponse`'s comment).
#[derive(serde::Serialize)]
struct RebuildResponse {
    sets_scanned: u64,
    items_indexed: u64,
    blobs_indexed: u64,
    orphan_blobs_found: u64,
    duration_ms: u64,
}

/// Wire shape for `export`. `set_id` is on the wire so consumers
/// don't have to re-derive it from the invocation argv; `tarball`
/// is the operator's chosen destination path (forwarded verbatim
/// for log-correlation).
#[derive(serde::Serialize)]
struct ExportResponse {
    set_id: String,
    tarball: String,
    item_count: u64,
    blob_count: u64,
    bytes_written: u64,
    duration_ms: u64,
}

/// Wire shape for `import`. `set_id` is read from the tarball's
/// manifest and surfaced so JSON consumers can correlate without
/// re-parsing the tarball; `tarball` is the operator's source path.
#[derive(serde::Serialize)]
struct ImportResponse {
    set_id: String,
    tarball: String,
    item_count: u64,
    blob_count: u64,
    bytes_read: u64,
    duration_ms: u64,
}

/// Wire shape for `verify`. `duration` lives at the boundary as
/// `duration_ms: u64` so the JSON stays machine-friendly (vs
/// serde's default `Duration` shape `{secs: ..., nanos: ...}`).
/// `scope` is `"full" | "since" | "container"` per the CLI doc.
#[derive(serde::Serialize)]
struct VerifyResponse {
    blobs_checked: u64,
    mismatches: u64,
    mismatches_hash: u64,
    mismatches_missing: u64,
    duration_ms: u64,
    scope: &'static str,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli, &mut std::io::stdout()) {
        Ok(()) => ExitCode::from(EXIT_OK),
        Err(e) => {
            eprintln!("cosmix-mds: {e}");
            ExitCode::from(EXIT_FAIL)
        }
    }
}

/// Run the parsed CLI against an arbitrary stdout sink. Pulled out
/// of `main` so integration tests can drive the binary without
/// spawning a subprocess where useful — although the stdout-capture
/// shape is *also* exercised end-to-end via `assert_cmd` so the
/// real binary path is covered.
pub fn run<W: std::io::Write>(cli: Cli, stdout: &mut W) -> Result<(), CliError> {
    let root = resolve_root(cli.root.as_deref(), std::env::var_os(ENV_ROOT).as_deref())?;
    match cli.command {
        Command::ListSets => list_sets(&root, cli.json, stdout),
        Command::MigrateAll => migrate_all(&root, cli.json, stdout),
        Command::Stats { per_set } => stats(&root, per_set, cli.json, stdout),
        Command::Verify(args) => verify(&root, args, cli.json, stdout),
        Command::Gc { dry_run } => gc(&root, dry_run, cli.json, stdout),
        Command::RebuildIndex => rebuild_index(&root, cli.json, stdout),
        Command::Export { set_id, tarball } => export(&root, &set_id, &tarball, cli.json, stdout),
        Command::Import { tarball } => import(&root, &tarball, cli.json, stdout),
        Command::PruneChangelog {
            set_id,
            stream,
            keep_n,
        } => prune_changelog(&root, &set_id, stream.into(), keep_n, cli.json, stdout),
    }
}

#[derive(thiserror::Error, Debug)]
pub enum CliError {
    #[error("no MDS root specified: pass --root <PATH> or set the {ENV_ROOT} environment variable")]
    MissingRoot,

    #[error("mds: {0}")]
    Mds(#[from] cosmix_mds::Error),

    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    #[error("json: {0}")]
    Json(#[from] serde_json::Error),

    #[error("invalid duration {0:?}: {1}")]
    BadDuration(String, String),

    #[error("invalid container UUID {0:?}: {1}")]
    BadContainerUuid(String, String),

    #[error("'since' is in the future relative to system clock")]
    SinceInFuture,

    #[error("invalid set UUID {0:?}: {1}")]
    BadSetUuid(String, String),
}

/// Resolve `--root` arg vs `$COSMIX_MDS_ROOT` env, in that order.
/// Public for testability — the resolution rule itself is part of
/// the operator contract.
pub fn resolve_root(
    arg: Option<&std::path::Path>,
    env: Option<&std::ffi::OsStr>,
) -> Result<PathBuf, CliError> {
    if let Some(p) = arg {
        return Ok(p.to_path_buf());
    }
    if let Some(p) = env {
        return Ok(PathBuf::from(p));
    }
    Err(CliError::MissingRoot)
}

fn list_sets<W: std::io::Write>(
    root: &std::path::Path,
    json: bool,
    w: &mut W,
) -> Result<(), CliError> {
    let mds = SqliteCasMds::open(root)?;
    let sets: Vec<SetId> = mds.list_sets()?;
    let ids: Vec<String> = sets.iter().map(|s| s.0.to_string()).collect();
    if json {
        let resp = ListSetsResponse { sets: ids };
        serde_json::to_writer(&mut *w, &resp)?;
        writeln!(w)?;
    } else {
        for id in &ids {
            writeln!(w, "{id}")?;
        }
    }
    Ok(())
}

/// Open the MDS root and report the number of sets that were
/// discovered and migrated. `SqliteCasMds::open` already runs
/// `apply_blobs_migrations` on the box-wide blobs.sqlite and
/// `apply_data_migrations` on every per-set `data.sqlite` it
/// discovers, so this command is genuinely a thin wrapper:
/// the work happens during `open`, the CLI just reports the
/// resulting count.
///
/// Errors during open() bubble up as a CliError and exit 2;
/// in v1 there is no partial-failure mode because `open()`
/// stops on the first failing set. The `errors` field is
/// always `0` on the success path — it is reserved for a
/// future per-set try/continue API that does not yet exist.
fn migrate_all<W: std::io::Write>(
    root: &std::path::Path,
    json: bool,
    w: &mut W,
) -> Result<(), CliError> {
    let mds = SqliteCasMds::open(root)?;
    let sets: u64 = mds.list_sets()?.len() as u64;
    if json {
        let resp = MigrateAllResponse { sets, errors: 0 };
        serde_json::to_writer(&mut *w, &resp)?;
        writeln!(w)?;
    } else {
        writeln!(w, "migrate-all: {sets} sets opened, 0 errors")?;
    }
    Ok(())
}

/// Translate parsed `--full | --since=<DUR> | --container=<UUID>`
/// flags into a library `VerifyScope`. Returns the resolved scope
/// plus the wire-format scope tag (`"full" | "since" | "container"`)
/// for the `--json` response.
///
/// Mutual exclusion is enforced by clap's `group = "scope"`. With
/// none set, the default is `Full` per the CLI doc.
pub fn resolve_verify_scope(args: &VerifyArgs) -> Result<(VerifyScope, &'static str), CliError> {
    if let Some(s) = &args.since {
        let dur = humantime::parse_duration(s)
            .map_err(|e| CliError::BadDuration(s.clone(), e.to_string()))?;
        let t = SystemTime::now()
            .checked_sub(dur)
            .ok_or(CliError::SinceInFuture)?;
        return Ok((VerifyScope::Since(t), "since"));
    }
    if let Some(s) = &args.container {
        let uuid = uuid::Uuid::parse_str(s)
            .map_err(|e| CliError::BadContainerUuid(s.clone(), e.to_string()))?;
        return Ok((VerifyScope::Container(ContainerId(uuid)), "container"));
    }
    // `--full` is the documented default; whether it was set
    // explicitly or not, the scope is identical.
    let _ = args.full;
    Ok((VerifyScope::Full, "full"))
}

fn verify<W: std::io::Write>(
    root: &std::path::Path,
    args: VerifyArgs,
    json: bool,
    w: &mut W,
) -> Result<(), CliError> {
    let (scope, scope_tag) = resolve_verify_scope(&args)?;
    let mds = SqliteCasMds::open(root)?;
    let report: VerifyReport = mds.verify_blobs(scope)?;
    if json {
        let resp = VerifyResponse {
            blobs_checked: report.blobs_checked,
            mismatches: report.mismatches,
            mismatches_hash: report.mismatches_hash,
            mismatches_missing: report.mismatches_missing,
            duration_ms: report.duration.as_millis() as u64,
            scope: scope_tag,
        };
        serde_json::to_writer(&mut *w, &resp)?;
        writeln!(w)?;
    } else {
        write_human_verify(w, &report)?;
    }
    Ok(())
}

fn write_human_verify<W: std::io::Write>(w: &mut W, r: &VerifyReport) -> std::io::Result<()> {
    writeln!(
        w,
        "verified {} blobs in {}",
        thousands(r.blobs_checked),
        human_duration(r.duration),
    )?;
    if r.mismatches == 0 {
        writeln!(w, "mismatches: 0")?;
    } else {
        writeln!(
            w,
            "mismatches: {} ({} hash-mismatch, {} missing)",
            thousands(r.mismatches),
            thousands(r.mismatches_hash),
            thousands(r.mismatches_missing),
        )?;
    }
    Ok(())
}

fn gc<W: std::io::Write>(
    root: &std::path::Path,
    dry_run: bool,
    json: bool,
    w: &mut W,
) -> Result<(), CliError> {
    let mds = SqliteCasMds::open(root)?;
    let report: GcReport = mds.gc(dry_run)?;
    if json {
        let resp = GcResponse {
            dry_run,
            blobs_deleted: report.blobs_deleted,
            bytes_freed: report.bytes_freed,
            duration_ms: report.duration.as_millis() as u64,
            candidates_pass1: report.candidates_pass1,
            skipped_re_referenced: report.skipped_re_referenced,
            skipped_re_touched: report.skipped_re_touched,
            orphan_rows_swept: report.orphan_rows_swept,
            pending_rows_observed: report.pending_rows_observed,
        };
        serde_json::to_writer(&mut *w, &resp)?;
        writeln!(w)?;
    } else {
        write_human_gc(w, &report, dry_run)?;
    }
    Ok(())
}

fn rebuild_index<W: std::io::Write>(
    root: &std::path::Path,
    json: bool,
    w: &mut W,
) -> Result<(), CliError> {
    let mds = SqliteCasMds::open(root)?;
    let report: RebuildReport = mds.rebuild_index()?;
    if json {
        let resp = RebuildResponse {
            sets_scanned: report.sets_scanned,
            items_indexed: report.items_indexed,
            blobs_indexed: report.blobs_indexed,
            orphan_blobs_found: report.orphan_blobs_found,
            duration_ms: report.duration.as_millis() as u64,
        };
        serde_json::to_writer(&mut *w, &resp)?;
        writeln!(w)?;
    } else {
        write_human_rebuild(w, &report)?;
    }
    Ok(())
}

fn export<W: std::io::Write>(
    root: &std::path::Path,
    set_id: &str,
    tarball: &std::path::Path,
    json: bool,
    w: &mut W,
) -> Result<(), CliError> {
    let uuid = uuid::Uuid::parse_str(set_id)
        .map_err(|e| CliError::BadSetUuid(set_id.to_string(), e.to_string()))?;
    let set = SetId(uuid);
    let mds = SqliteCasMds::open(root)?;
    let report: ExportReport = mds.export_set(&set, tarball)?;
    if json {
        let resp = ExportResponse {
            set_id: report.set_id.0.to_string(),
            tarball: tarball.display().to_string(),
            item_count: report.item_count,
            blob_count: report.blob_count,
            bytes_written: report.bytes_written,
            duration_ms: report.duration.as_millis() as u64,
        };
        serde_json::to_writer(&mut *w, &resp)?;
        writeln!(w)?;
    } else {
        write_human_export(w, &report, tarball)?;
    }
    Ok(())
}

fn write_human_export<W: std::io::Write>(
    w: &mut W,
    r: &ExportReport,
    tarball: &std::path::Path,
) -> std::io::Result<()> {
    writeln!(
        w,
        "exported set {} \u{2192} {} ({}, {} blobs, {})",
        r.set_id.0,
        tarball.display(),
        human_bytes(r.bytes_written),
        thousands(r.blob_count),
        human_duration(r.duration),
    )?;
    Ok(())
}

fn import<W: std::io::Write>(
    root: &std::path::Path,
    tarball: &std::path::Path,
    json: bool,
    w: &mut W,
) -> Result<(), CliError> {
    let mds = SqliteCasMds::open(root)?;
    let report: ImportReport = mds.import_set(tarball)?;
    if json {
        let resp = ImportResponse {
            set_id: report.set_id.0.to_string(),
            tarball: tarball.display().to_string(),
            item_count: report.item_count,
            blob_count: report.blob_count,
            bytes_read: report.bytes_read,
            duration_ms: report.duration.as_millis() as u64,
        };
        serde_json::to_writer(&mut *w, &resp)?;
        writeln!(w)?;
    } else {
        write_human_import(w, &report, tarball)?;
    }
    Ok(())
}

#[derive(serde::Serialize)]
struct PruneChangelogResponse {
    set_id: String,
    stream: &'static str,
    keep_n: u64,
    rows_removed: u64,
    new_floor: u64,
}

fn prune_changelog<W: std::io::Write>(
    root: &std::path::Path,
    set_id: &str,
    stream: ChangelogStream,
    keep_n: u64,
    json: bool,
    w: &mut W,
) -> Result<(), CliError> {
    let set = uuid::Uuid::parse_str(set_id)
        .map(SetId)
        .map_err(|e| CliError::BadSetUuid(set_id.to_string(), e.to_string()))?;
    let mds = SqliteCasMds::open(root)?;
    let report: PruneReport = mds.prune_changelog(&set, stream, keep_n)?;
    if json {
        let resp = PruneChangelogResponse {
            set_id: set.0.to_string(),
            stream: stream.as_str(),
            keep_n,
            rows_removed: report.rows_removed,
            new_floor: report.new_floor,
        };
        serde_json::to_writer(&mut *w, &resp)?;
        writeln!(w)?;
    } else {
        writeln!(
            w,
            "prune-changelog: set={} stream={} keep_n={} removed={} new_floor={}",
            set.0,
            stream,
            keep_n,
            thousands(report.rows_removed),
            thousands(report.new_floor),
        )?;
    }
    Ok(())
}

fn write_human_import<W: std::io::Write>(
    w: &mut W,
    r: &ImportReport,
    tarball: &std::path::Path,
) -> std::io::Result<()> {
    writeln!(
        w,
        "imported set {} \u{2190} {} ({}, {} blobs, {})",
        r.set_id.0,
        tarball.display(),
        human_bytes(r.bytes_read),
        thousands(r.blob_count),
        human_duration(r.duration),
    )?;
    Ok(())
}

fn write_human_rebuild<W: std::io::Write>(w: &mut W, r: &RebuildReport) -> std::io::Result<()> {
    writeln!(
        w,
        "rebuild-index: {} sets, {} items, {} blobs in {}",
        thousands(r.sets_scanned),
        thousands(r.items_indexed),
        thousands(r.blobs_indexed),
        human_duration(r.duration),
    )?;
    writeln!(
        w,
        "  orphan blobs found: {}",
        thousands(r.orphan_blobs_found),
    )?;
    Ok(())
}

fn write_human_gc<W: std::io::Write>(
    w: &mut W,
    r: &GcReport,
    dry_run: bool,
) -> std::io::Result<()> {
    // Headline distinguishes dry-run "would" from real "did" so
    // operators reading log scrape don't confuse the two even when
    // the counters happen to be zero.
    let (prefix, verb, freed_verb) = if dry_run {
        ("gc dry-run:", "would delete", "free")
    } else {
        ("gc:", "deleted", "freed")
    };
    writeln!(
        w,
        "{prefix} {verb} {blobs} blobs, {freed_verb} {freed} in {dur}",
        blobs = thousands(r.blobs_deleted),
        freed = human_bytes(r.bytes_freed),
        dur = human_duration(r.duration),
    )?;
    writeln!(
        w,
        "  pass 1 candidates:    {}",
        thousands(r.candidates_pass1)
    )?;
    writeln!(w, "  pass 2 deleted:       {}", thousands(r.blobs_deleted))?;
    writeln!(
        w,
        "  skipped (re-ref):     {}",
        thousands(r.skipped_re_referenced)
    )?;
    writeln!(
        w,
        "  skipped (re-touched): {}",
        thousands(r.skipped_re_touched)
    )?;
    writeln!(
        w,
        "  orphan rows swept:    {}",
        thousands(r.orphan_rows_swept)
    )?;
    writeln!(
        w,
        "  refcount_pending:     {}",
        thousands(r.pending_rows_observed)
    )?;
    Ok(())
}

/// Human-facing duration: sub-second renders as `XXXms`, anything
/// else as `X.Ys` with one decimal. Matches the format in
/// `_doc/2026-05-02-cosmix-mds-cli.md` §human-format.
fn human_duration(d: std::time::Duration) -> String {
    if d < std::time::Duration::from_secs(1) {
        format!("{}ms", d.as_millis())
    } else {
        format!("{:.1}s", d.as_secs_f64())
    }
}

fn stats<W: std::io::Write>(
    root: &std::path::Path,
    per_set: bool,
    json: bool,
    w: &mut W,
) -> Result<(), CliError> {
    let mds = SqliteCasMds::open(root)?;
    let global: MdsStats = mds.stats()?;
    let per: Option<Vec<PerSetStats>> = if per_set {
        Some(mds.stats_per_set()?)
    } else {
        None
    };
    if json {
        let resp = StatsResponse {
            set_count: global.set_count,
            container_count: global.container_count,
            item_count: global.item_count,
            blob_count: global.blob_count,
            total_bytes: global.total_bytes,
            dedup_ratio: global.dedup_ratio,
            sets: per
                .as_ref()
                .map(|v| v.iter().map(PerSetStatsResponse::from).collect()),
        };
        serde_json::to_writer(&mut *w, &resp)?;
        writeln!(w)?;
    } else {
        write_human_global(w, &global)?;
        if let Some(per) = per {
            writeln!(w)?;
            write_human_per_set(w, &per)?;
        }
    }
    Ok(())
}

fn write_human_global<W: std::io::Write>(w: &mut W, s: &MdsStats) -> std::io::Result<()> {
    writeln!(w, "sets:        {}", thousands(s.set_count))?;
    writeln!(w, "containers:  {}", thousands(s.container_count))?;
    writeln!(w, "items:       {}", thousands(s.item_count))?;
    writeln!(w, "blobs:       {}", thousands(s.blob_count))?;
    writeln!(w, "total:       {}", human_bytes(s.total_bytes))?;
    writeln!(w, "dedup ratio: {:.2}", s.dedup_ratio)?;
    Ok(())
}

fn write_human_per_set<W: std::io::Write>(w: &mut W, rows: &[PerSetStats]) -> std::io::Result<()> {
    // Fixed-width columns sized to handle UUIDs and reasonable
    // counters. We don't auto-fit the widest row because that
    // requires a second pass and the gain is marginal — operators
    // pipe to less or grep, not visually align across runs.
    writeln!(
        w,
        "{:<36}  {:>10}  {:>9}  {:>9}  {:>10}",
        "SET", "CONTAINERS", "ITEMS", "BLOBS", "BYTES"
    )?;
    for r in rows {
        writeln!(
            w,
            "{:<36}  {:>10}  {:>9}  {:>9}  {:>10}",
            r.set_id.0,
            thousands(r.container_count),
            thousands(r.item_count),
            thousands(r.blob_count),
            human_bytes(r.total_bytes),
        )?;
    }
    Ok(())
}

/// Insert `,` thousands separators. Tiny inline implementation
/// rather than pulling in a formatter dep — the format is for
/// human eyeballing, not parsing (machine consumers use `--json`),
/// and the binary stays dependency-light.
fn thousands(n: u64) -> String {
    let s = n.to_string();
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, b) in bytes.iter().enumerate() {
        if i > 0 && (bytes.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(*b as char);
    }
    out
}

/// Render bytes as a short human string (e.g., `"1.1 GB"`).
/// Uses base-10 units (KB/MB/GB) because that's what most
/// operator tooling shows for storage. Values < 1 KB render
/// as raw byte counts so 0 / single-digit byte cases stay
/// legible.
fn human_bytes(n: u64) -> String {
    const KB: u64 = 1_000;
    const MB: u64 = 1_000_000;
    const GB: u64 = 1_000_000_000;
    const TB: u64 = 1_000_000_000_000;
    if n < KB {
        format!("{n} B")
    } else if n < MB {
        format!("{:.1} KB", n as f64 / KB as f64)
    } else if n < GB {
        format!("{:.1} MB", n as f64 / MB as f64)
    } else if n < TB {
        format!("{:.1} GB", n as f64 / GB as f64)
    } else {
        format!("{:.2} TB", n as f64 / TB as f64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;

    #[test]
    fn resolve_root_prefers_arg_over_env() {
        let arg = std::path::Path::new("/from/arg");
        let env = OsString::from("/from/env");
        let r = resolve_root(Some(arg), Some(env.as_os_str())).unwrap();
        assert_eq!(r, PathBuf::from("/from/arg"));
    }

    #[test]
    fn resolve_root_falls_back_to_env() {
        let env = OsString::from("/from/env");
        let r = resolve_root(None, Some(env.as_os_str())).unwrap();
        assert_eq!(r, PathBuf::from("/from/env"));
    }

    #[test]
    fn resolve_root_errors_when_neither_set() {
        let err = resolve_root(None, None).unwrap_err();
        assert!(matches!(err, CliError::MissingRoot));
    }

    #[test]
    fn thousands_formats_common_magnitudes() {
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(42), "42");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(1_000), "1,000");
        assert_eq!(thousands(12_340), "12,340");
        assert_eq!(thousands(1_234_567), "1,234,567");
    }

    #[test]
    fn human_bytes_picks_unit_per_magnitude() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(999), "999 B");
        assert_eq!(human_bytes(1_500), "1.5 KB");
        assert_eq!(human_bytes(2_400_000_000), "2.4 GB");
    }

    #[test]
    fn human_duration_picks_unit() {
        use std::time::Duration;
        assert_eq!(human_duration(Duration::from_millis(0)), "0ms");
        assert_eq!(human_duration(Duration::from_millis(420)), "420ms");
        assert_eq!(human_duration(Duration::from_millis(999)), "999ms");
        assert_eq!(human_duration(Duration::from_secs(1)), "1.0s");
        assert_eq!(human_duration(Duration::from_millis(9_400)), "9.4s");
    }

    #[test]
    fn verify_scope_defaults_to_full_when_no_flag_set() {
        let args = VerifyArgs {
            full: false,
            since: None,
            container: None,
        };
        let (_scope, tag) = resolve_verify_scope(&args).unwrap();
        assert_eq!(tag, "full");
    }

    #[test]
    fn verify_scope_full_flag_resolves_to_full() {
        let args = VerifyArgs {
            full: true,
            since: None,
            container: None,
        };
        let (_scope, tag) = resolve_verify_scope(&args).unwrap();
        assert_eq!(tag, "full");
    }

    #[test]
    fn verify_scope_parses_humantime_since() {
        let args = VerifyArgs {
            full: false,
            since: Some("1h30m".into()),
            container: None,
        };
        let (_scope, tag) = resolve_verify_scope(&args).unwrap();
        assert_eq!(tag, "since");
    }

    #[test]
    fn verify_scope_rejects_bad_duration() {
        let args = VerifyArgs {
            full: false,
            since: Some("not-a-duration".into()),
            container: None,
        };
        let err = resolve_verify_scope(&args).unwrap_err();
        assert!(matches!(err, CliError::BadDuration(_, _)));
    }

    #[test]
    fn verify_scope_parses_container_uuid() {
        let u = uuid::Uuid::now_v7();
        let args = VerifyArgs {
            full: false,
            since: None,
            container: Some(u.to_string()),
        };
        let (_scope, tag) = resolve_verify_scope(&args).unwrap();
        assert_eq!(tag, "container");
    }

    #[test]
    fn verify_scope_rejects_bad_uuid() {
        let args = VerifyArgs {
            full: false,
            since: None,
            container: Some("not-a-uuid".into()),
        };
        let err = resolve_verify_scope(&args).unwrap_err();
        assert!(matches!(err, CliError::BadContainerUuid(_, _)));
    }
}
