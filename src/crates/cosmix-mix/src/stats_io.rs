use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use cosmix_mix::stats::{
    BUILTIN_NAMES, CANONICAL_KEYWORDS, ExecutionMode, HOF_NAMES, StatsBucket, StatsCounters,
    UsageStats, current_iso_week, date_for_epoch_day, datetime_string, iso_week_for_date,
};
use rusqlite::{Connection, params};

const WRITER_LOCK_DEADLINE: Duration = Duration::from_millis(200);
const REPORT_LOCK_DEADLINE: Duration = Duration::from_secs(2);
const MAX_PERSISTED_TIMESTAMP: u64 = 253_402_300_799; // 9999-12-31 23:59:59 UTC
static TEMP_SUFFIX: AtomicU64 = AtomicU64::new(0);
static STATS_ENABLED: OnceLock<bool> = OnceLock::new();

pub fn stats_setting_enabled(value: Option<&str>) -> bool {
    !matches!(
        value.map(str::trim),
        Some(v) if v.eq_ignore_ascii_case("off")
            || v.eq_ignore_ascii_case("false")
            || v == "0"
    )
}

/// Read MIX_STATS once for the process. Front ends consult this before
/// allocating a collector, which makes the kill switch skip all event work.
pub fn stats_enabled() -> bool {
    *STATS_ENABLED.get_or_init(|| stats_setting_enabled(std::env::var("MIX_STATS").ok().as_deref()))
}

fn debug_note(message: impl std::fmt::Display) {
    if std::env::var("MIX_DEBUG").is_ok() {
        eprintln!("mix-stats: {message}");
    }
}

fn is_canonical_date(value: &str) -> bool {
    value.len() == 10
        && value.as_bytes()[4] == b'-'
        && value.as_bytes()[7] == b'-'
        && value
            .bytes()
            .enumerate()
            .all(|(index, byte)| matches!(index, 4 | 7) || byte.is_ascii_digit())
        && iso_week_for_date(value).is_some()
}

fn is_canonical_week(value: &str) -> bool {
    value.len() == 8
        && value.as_bytes()[4] == b'-'
        && value.as_bytes()[5] == b'W'
        && value
            .bytes()
            .enumerate()
            .all(|(index, byte)| matches!(index, 4 | 5) || byte.is_ascii_digit())
        && value[6..]
            .parse::<u8>()
            .is_ok_and(|week| (1..=53).contains(&week))
}

pub fn stats_dir() -> Option<PathBuf> {
    if !stats_enabled() {
        return None;
    }
    if let Ok(xdg) = std::env::var("XDG_STATE_HOME")
        && !xdg.is_empty()
    {
        let path = PathBuf::from(&xdg);
        if path.is_absolute() {
            return Some(path.join("mix"));
        }
        debug_note(format!(
            "XDG_STATE_HOME={xdg:?} is not absolute; using HOME fallback"
        ));
    }
    let home = std::env::var("HOME").ok().filter(|s| !s.is_empty())?;
    Some(PathBuf::from(home).join(".local/state/mix"))
}

fn ensure_stats_dir() -> Option<PathBuf> {
    let dir = stats_dir()?;
    if let Err(error) = fs::create_dir_all(&dir) {
        debug_note(format!("cannot create {}: {error}", dir.display()));
        return None;
    }
    Some(dir)
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn legacy_data_file_count(dir: &Path) -> usize {
    let Ok(entries) = fs::read_dir(dir) else {
        return 0;
    };
    entries
        .filter_map(Result::ok)
        .filter(|entry| {
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                return false;
            };
            path.is_file()
                && !name.starts_with('.')
                && (name == "mix.db" || name.ends_with(".json"))
        })
        .count()
}

fn legacy_advisory() {
    let Some(new_dir) = ensure_stats_dir() else {
        return;
    };
    let marker = new_dir.join(".legacy-acknowledged");
    if marker.exists() {
        return;
    }
    let legacy_dir = crate::cosmix_paths::cosmix_src().join("_stats");
    if legacy_dir == new_dir {
        return;
    }
    let count = legacy_data_file_count(&legacy_dir);
    if count == 0 {
        return;
    }
    let legacy = shell_quote(&legacy_dir.display().to_string());
    let new = shell_quote(&new_dir.display().to_string());
    let new_slash = shell_quote(&format!("{}/", new_dir.display()));
    let marker = shell_quote(&marker.display().to_string());
    eprintln!(
        "mix-stats: legacy stats found at {} ({count} stats files).",
        legacy_dir.display()
    );
    eprintln!("           They are not migrated automatically. To migrate:");
    eprintln!("             mkdir -p {new} && \\");
    eprintln!(
        "               (cd {legacy} && mv -n -- *.json mix.db {new_slash} 2>/dev/null || true)"
    );
    eprintln!("           To keep them there and suppress this notice:");
    eprintln!("             touch {marker}");
}

struct StatsLock(File);

impl StatsLock {
    fn acquire(dir: &Path, exclusive: bool, deadline: Duration) -> std::io::Result<Self> {
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(dir.join("stats.lock"))?;
        let operation = if exclusive {
            libc::LOCK_EX
        } else {
            libc::LOCK_SH
        } | libc::LOCK_NB;
        let started = Instant::now();
        loop {
            let result = unsafe { libc::flock(file.as_raw_fd(), operation) };
            if result == 0 {
                return Ok(Self(file));
            }
            let error = std::io::Error::last_os_error();
            if error.kind() != std::io::ErrorKind::WouldBlock || started.elapsed() >= deadline {
                return Err(error);
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for StatsLock {
    fn drop(&mut self) {
        unsafe {
            libc::flock(self.0.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

pub fn flush_batch(mut delta: UsageStats) {
    if !stats_enabled() {
        return;
    }
    delta.finalize_session();
    let Some(dir) = ensure_stats_dir() else {
        return;
    };
    if let Err(error) = flush_batch_to_dir(&dir, &delta) {
        debug_note(format!("flush skipped: {error}"));
    }
}

/// Flush a mid-session delta produced by `UsageStats::drain_pending_buckets`
/// — buckets only, no session record, nothing finalized. Each ISO week is
/// committed **independently**, and `Err` carries ONLY the buckets that were
/// not persisted, for the caller to merge back into the live collector.
/// Handing the whole delta to `flush_batch_to_dir` instead would commit its
/// weeks sequentially, so a failure on week N after week N-1's write would
/// make a whole-delta merge-back double-count the committed week on the next
/// flush. (`flush_batch`'s fire-and-forget drop is only acceptable at
/// process exit, where nothing remains to merge back into.)
pub fn flush_pending_delta(delta: &UsageStats) -> Result<(), Box<UsageStats>> {
    if !stats_enabled() || delta.buckets.is_empty() {
        return Ok(());
    }
    let (week_deltas, mut residual) = split_delta_by_week(delta);
    let Some(dir) = ensure_stats_dir() else {
        return Err(Box::new(delta.clone()));
    };
    for week_delta in week_deltas {
        if let Err(error) = flush_batch_to_dir(&dir, &week_delta) {
            debug_note(format!("pending flush skipped: {error}"));
            residual.buckets.extend(week_delta.buckets);
        }
    }
    if residual.buckets.is_empty() {
        Ok(())
    } else {
        residual.rebuild_aggregates();
        Err(Box::new(residual))
    }
}

/// Group a delta's buckets into one single-week delta per ISO week (each is
/// a single atomic document write in `flush_batch_to_dir`), plus a residual
/// holding buckets whose date maps to no week — those can never be
/// persisted, so they go straight back to the caller.
fn split_delta_by_week(delta: &UsageStats) -> (Vec<UsageStats>, UsageStats) {
    let mut weeks: BTreeMap<String, UsageStats> = BTreeMap::new();
    // `for_execution` starts with no buckets — these collectors only carry
    // what the loop pushes into them.
    let mut residual = UsageStats::for_execution(delta.context().clone());
    for bucket in &delta.buckets {
        match iso_week_for_date(&bucket.date) {
            Some(week) => weeks
                .entry(week)
                .or_insert_with(|| UsageStats::for_execution(delta.context().clone()))
                .buckets
                .push(bucket.clone()),
            None => residual.buckets.push(bucket.clone()),
        }
    }
    let week_deltas = weeks
        .into_values()
        .map(|mut part| {
            part.rebuild_aggregates();
            part
        })
        .collect();
    (week_deltas, residual)
}

/// Compatibility entry point used by older callers.
pub fn save_stats(stats: &mut UsageStats) {
    stats.finalize_session();
    flush_batch(stats.clone());
}

fn flush_batch_to_dir(dir: &Path, delta: &UsageStats) -> Result<(), String> {
    flush_batch_to_dir_with_deadline(dir, delta, WRITER_LOCK_DEADLINE)
}

fn flush_batch_to_dir_with_deadline(
    dir: &Path,
    delta: &UsageStats,
    lock_deadline: Duration,
) -> Result<(), String> {
    fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let _lock =
        StatsLock::acquire(dir, true, lock_deadline).map_err(|e| format!("stats lock: {e}"))?;
    rotate_current(dir)?;

    let mut weeks: BTreeMap<String, UsageStats> = BTreeMap::new();
    for bucket in &delta.buckets {
        let week = iso_week_for_date(&bucket.date)
            .ok_or_else(|| format!("invalid bucket date {:?}", bucket.date))?;
        let doc = weeks
            .entry(week.clone())
            .or_insert_with(|| empty_doc(&week));
        doc.buckets.push(bucket.clone());
    }
    for session in &delta.sessions {
        let date = date_for_epoch_day(session.started / 86_400);
        let week = iso_week_for_date(&date).ok_or_else(|| format!("invalid run date {date}"))?;
        weeks
            .entry(week.clone())
            .or_insert_with(|| empty_doc(&week))
            .sessions
            .push(session.clone());
    }

    let now_week = current_iso_week();
    let mut canonical = Vec::new();
    for (week, mut part) in weeks {
        part.rebuild_aggregates();
        let path = week_path(dir, &week, &now_week);
        let mut doc = read_doc(&path)?.unwrap_or_else(|| empty_doc(&week));
        doc.merge(&part);
        doc.week.clone_from(&week);
        doc.last_date = doc
            .buckets
            .iter()
            .map(|b| b.date.as_str())
            .max()
            .unwrap_or("")
            .to_string();
        doc.bound_script_labels();
        atomic_write_json(&path, &doc)?;
        canonical.push(doc);
    }

    if let Err(error) = update_sqlite(dir, &canonical) {
        debug_note(format!("SQLite update failed after JSON commit: {error}"));
    }
    Ok(())
}

fn rotate_current(dir: &Path) -> Result<(), String> {
    let path = dir.join("current.json");
    let Some(mut current) = read_doc(&path)? else {
        return Ok(());
    };
    let now_week = current_iso_week();
    let week = document_week(&current, &now_week);
    if week == now_week {
        return Ok(());
    }
    current.week.clone_from(&week);
    let source = rotation_fingerprint(&current)?;
    let archive = dir.join(format!("{week}.json"));
    let mut merged = read_doc(&archive)?.unwrap_or_else(|| empty_doc(&week));
    if !merged.rotation_sources.contains(&source) {
        merged.merge(&current);
        merged.week.clone_from(&week);
        merged.rotation_sources.push(source);
        merged.bound_script_labels();
        atomic_write_json(&archive, &merged)?;
    }
    fs::remove_file(&path).map_err(|e| format!("remove rotated current.json: {e}"))
}

fn document_week(stats: &UsageStats, fallback: &str) -> String {
    if is_canonical_week(&stats.week) {
        return stats.week.clone();
    }
    let from_buckets = stats
        .buckets
        .iter()
        .filter(|bucket| is_canonical_date(&bucket.date))
        .max_by_key(|bucket| &bucket.date)
        .and_then(|bucket| iso_week_for_date(&bucket.date));
    let from_sessions = stats
        .sessions
        .iter()
        .max_by_key(|session| session.started)
        .map(|session| date_for_epoch_day(session.started / 86_400))
        .and_then(|date| iso_week_for_date(&date));
    let from_last_date = is_canonical_date(&stats.last_date)
        .then(|| iso_week_for_date(&stats.last_date))
        .flatten();
    let week = from_buckets
        .or(from_sessions)
        .or(from_last_date)
        .unwrap_or_else(|| fallback.to_string());
    debug_note(format!(
        "invalid persisted week {:?}; derived {week:?} from document dates",
        stats.week
    ));
    week
}

fn rotation_fingerprint(stats: &UsageStats) -> Result<String, String> {
    let bytes = serde_json::to_vec(&stats.to_json()).map_err(|error| error.to_string())?;
    Ok(format!("blake3:{}", blake3::hash(&bytes).to_hex()))
}

fn week_path(dir: &Path, week: &str, now_week: &str) -> PathBuf {
    if week == now_week {
        dir.join("current.json")
    } else {
        dir.join(format!("{week}.json"))
    }
}

fn read_doc(path: &Path) -> Result<Option<UsageStats>, String> {
    if !path.exists() {
        return Ok(None);
    }
    let content = fs::read_to_string(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    let value = match serde_json::from_str(&content) {
        Ok(value) => value,
        Err(error) => {
            quarantine_doc(path, &error);
            return Ok(None);
        }
    };
    let stats = UsageStats::from_json(value);
    if let Err(error) = validate_persisted_doc(&stats) {
        quarantine_doc(path, &error);
        return Ok(None);
    }
    Ok(Some(stats))
}

fn validate_persisted_doc(stats: &UsageStats) -> Result<(), String> {
    if !stats.last_date.is_empty() && !is_canonical_date(&stats.last_date) {
        return Err(format!("invalid persisted last_date {:?}", stats.last_date));
    }
    if let Some(bucket) = stats
        .buckets
        .iter()
        .find(|bucket| !is_canonical_date(&bucket.date))
    {
        return Err(format!("invalid persisted bucket date {:?}", bucket.date));
    }
    if let Some(session) = stats
        .sessions
        .iter()
        .find(|session| session.started > MAX_PERSISTED_TIMESTAMP)
    {
        return Err(format!(
            "persisted session timestamp {} exceeds supported range",
            session.started
        ));
    }
    Ok(())
}

fn quarantine_doc(path: &Path, error: &dyn std::fmt::Display) {
    let suffix = TEMP_SUFFIX.fetch_add(1, Ordering::Relaxed);
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("stats.json");
    let corrupt = path.with_file_name(format!("{name}.{}.{}.corrupt", std::process::id(), suffix));
    match fs::rename(path, &corrupt) {
        Ok(()) => debug_note(format!(
            "quarantined corrupt {} as {}: {error}",
            path.display(),
            corrupt.display()
        )),
        Err(rename_error) => debug_note(format!(
            "ignoring corrupt {} (quarantine failed: {rename_error}): {error}",
            path.display()
        )),
    }
}

fn atomic_write_json(path: &Path, stats: &UsageStats) -> Result<(), String> {
    let bytes = serde_json::to_vec_pretty(&stats.to_json()).map_err(|e| e.to_string())?;
    let suffix = TEMP_SUFFIX.fetch_add(1, Ordering::Relaxed);
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("stats.json");
    let temp = path.with_file_name(format!(".{name}.{}.{}.tmp", std::process::id(), suffix));
    let result = (|| -> Result<(), String> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)
            .map_err(|e| format!("create {}: {e}", temp.display()))?;
        file.write_all(&bytes).map_err(|e| e.to_string())?;
        file.sync_all().map_err(|e| e.to_string())?;
        fs::rename(&temp, path).map_err(|e| format!("rename {}: {e}", path.display()))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

fn empty_doc(week: &str) -> UsageStats {
    let mut stats = UsageStats::new();
    stats.builtins.clear();
    stats.functions.clear();
    stats.aliases.clear();
    stats.commands.clear();
    stats.keywords.clear();
    stats.meta.clear();
    stats.errors.clear();
    stats.buckets.clear();
    stats.sessions.clear();
    stats.week = week.to_string();
    stats.last_date.clear();
    stats
}

fn update_sqlite(dir: &Path, docs: &[UsageStats]) -> rusqlite::Result<()> {
    let mut connection = Connection::open(dir.join("mix.db"))?;
    let transaction = connection.transaction()?;
    transaction.execute_batch(
        "CREATE TABLE IF NOT EXISTS usage (
             date TEXT, category TEXT, name TEXT, count INTEGER,
             PRIMARY KEY (date, category, name));
         CREATE TABLE IF NOT EXISTS sessions (
             started INTEGER, duration_secs INTEGER, commands INTEGER, peak_memory_kb INTEGER);
         CREATE TABLE IF NOT EXISTS usage_context (
             date TEXT, mode TEXT, script TEXT DEFAULT '', category TEXT, name TEXT, count INTEGER,
             PRIMARY KEY(date, mode, script, category, name));
         CREATE TABLE IF NOT EXISTS runs (
             id TEXT PRIMARY KEY, started INTEGER, duration_secs INTEGER, commands INTEGER,
             peak_memory_kb INTEGER, mode TEXT, script TEXT DEFAULT '');
         CREATE TABLE IF NOT EXISTS stats_meta (key TEXT PRIMARY KEY, value TEXT);",
    )?;
    let mut dates = HashSet::new();
    for doc in docs {
        dates.extend(doc.buckets.iter().map(|b| b.date.clone()));
    }
    for date in dates {
        transaction.execute("DELETE FROM usage WHERE date = ?1", [&date])?;
        transaction.execute("DELETE FROM usage_context WHERE date = ?1", [&date])?;
        let mut aggregate = StatsCounters::default();
        for doc in docs {
            for bucket in doc.buckets.iter().filter(|b| b.date == date) {
                aggregate.merge(&bucket.counters);
                insert_counter_maps(
                    &transaction,
                    &bucket.date,
                    Some((bucket.mode.as_str(), bucket.script.as_deref().unwrap_or(""))),
                    &bucket.counters,
                )?;
            }
        }
        insert_counter_maps(&transaction, &date, None, &aggregate)?;
    }
    for run in docs.iter().flat_map(|doc| doc.sessions.iter()) {
        // Revisit every canonical run in each touched weekly document. Label
        // folding can change an older run when a new basename consumes the
        // final slot, so inserting only the current delta leaves SQLite with
        // historical labels that JSON has already folded into `(other)`.
        let inserted = transaction.execute(
            "INSERT OR IGNORE INTO runs
             (id, started, duration_secs, commands, peak_memory_kb, mode, script)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                run.id,
                sqlite_i64(run.started),
                sqlite_i64(run.duration_secs),
                sqlite_i64(run.commands),
                sqlite_i64(run.peak_memory_kb),
                run.mode.as_str(),
                run.script.as_deref().unwrap_or("")
            ],
        )?;
        if inserted > 0 {
            transaction.execute(
                "INSERT INTO sessions (started, duration_secs, commands, peak_memory_kb)
                 VALUES (?1, ?2, ?3, ?4)",
                params![
                    sqlite_i64(run.started),
                    sqlite_i64(run.duration_secs),
                    sqlite_i64(run.commands),
                    sqlite_i64(run.peak_memory_kb)
                ],
            )?;
        } else {
            transaction.execute(
                "UPDATE runs SET script = ?1 WHERE id = ?2",
                params![run.script.as_deref().unwrap_or(""), run.id],
            )?;
        }
    }
    transaction.execute(
        "INSERT OR REPLACE INTO stats_meta(key, value) VALUES ('schema_version', '2')",
        [],
    )?;
    transaction.commit()
}

fn insert_counter_maps(
    tx: &rusqlite::Transaction<'_>,
    date: &str,
    context: Option<(&str, &str)>,
    counters: &StatsCounters,
) -> rusqlite::Result<()> {
    for (category, map) in counter_maps(counters) {
        for (name, count) in map {
            if let Some((mode, script)) = context {
                tx.execute(
                    "INSERT OR REPLACE INTO usage_context
                     (date, mode, script, category, name, count) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    params![date, mode, script, category, name, sqlite_i64(*count)],
                )?;
            } else {
                tx.execute(
                    "INSERT OR REPLACE INTO usage(date, category, name, count)
                     VALUES (?1, ?2, ?3, ?4)",
                    params![date, category, name, sqlite_i64(*count)],
                )?;
            }
        }
    }
    Ok(())
}

fn sqlite_i64(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

fn counter_maps(c: &StatsCounters) -> [(&'static str, &HashMap<String, u64>); 7] {
    [
        ("builtin", &c.builtins),
        ("function", &c.functions),
        ("alias", &c.aliases),
        ("command", &c.commands),
        ("keyword", &c.keywords),
        ("meta", &c.meta),
        ("error", &c.errors),
    ]
}

fn saturating_sum<'a>(values: impl IntoIterator<Item = &'a u64>) -> u64 {
    values
        .into_iter()
        .fold(0_u64, |total, value| total.saturating_add(*value))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StatsWindow {
    CurrentWeek,
    Week(String),
    Since(String),
    AllTime,
    LastDays(u32),
}

impl StatsWindow {
    fn header(&self) -> String {
        match self {
            Self::CurrentWeek => format!("week {}", current_iso_week()),
            Self::Week(week) => format!("week {week}"),
            Self::Since(date) => format!("since {date}"),
            Self::AllTime => "all time".to_string(),
            Self::LastDays(days) => format!("last {days} recorded days"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct StatsSnapshot {
    pub window: StatsWindow,
    pub counters: StatsCounters,
    pub buckets: Vec<StatsBucket>,
    pub sessions: Vec<cosmix_mix::stats::SessionRecord>,
}

impl StatsSnapshot {
    fn usage(&self) -> UsageStats {
        let mut result = empty_doc(&self.window.header());
        result.buckets.clone_from(&self.buckets);
        result.sessions.clone_from(&self.sessions);
        result.rebuild_aggregates();
        result.last_date = result
            .buckets
            .iter()
            .map(|bucket| bucket.date.as_str())
            .max()
            .unwrap_or("")
            .to_string();
        result.week = match &self.window {
            StatsWindow::CurrentWeek => current_iso_week(),
            StatsWindow::Week(week) => week.clone(),
            _ => String::new(),
        };
        result
    }
}

pub fn load_snapshot(
    window: StatsWindow,
    pending: Option<&UsageStats>,
) -> Result<StatsSnapshot, String> {
    let dir = ensure_stats_dir().ok_or_else(|| "stats state directory unavailable".to_string())?;
    // Reports may quarantine a corrupt document, so they need the exclusive lock.
    let _lock = StatsLock::acquire(&dir, true, REPORT_LOCK_DEADLINE)
        .map_err(|e| format!("stats lock: {e}"))?;
    load_snapshot_locked(&dir, window, pending)
}

fn load_snapshot_locked(
    dir: &Path,
    window: StatsWindow,
    pending: Option<&UsageStats>,
) -> Result<StatsSnapshot, String> {
    let mut documents = Vec::new();
    let mut paths: Vec<PathBuf> = fs::read_dir(dir)
        .map_err(|e| e.to_string())?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "json"))
        .collect();
    paths.sort();
    for path in paths {
        if let Some(doc) = read_doc(&path)? {
            documents.push(doc);
        }
    }
    if let Some(delta) = pending {
        documents.push(delta.clone());
    }
    let recorded_dates: HashSet<String> = if let StatsWindow::LastDays(days) = window {
        let mut dates: Vec<String> = documents
            .iter()
            .flat_map(|doc| doc.buckets.iter().map(|b| b.date.clone()))
            .collect();
        dates.sort();
        dates.dedup();
        dates.into_iter().rev().take(days as usize).collect()
    } else {
        HashSet::new()
    };
    let matches = |date: &str| match &window {
        StatsWindow::CurrentWeek => iso_week_for_date(date).as_deref() == Some(&current_iso_week()),
        StatsWindow::Week(week) => iso_week_for_date(date).as_deref() == Some(week),
        StatsWindow::Since(start) => date >= start.as_str(),
        StatsWindow::AllTime => true,
        StatsWindow::LastDays(_) => recorded_dates.contains(date),
    };
    let mut buckets = Vec::new();
    let mut sessions = Vec::new();
    let mut seen_runs = HashSet::new();
    for doc in documents {
        buckets.extend(doc.buckets.into_iter().filter(|b| matches(&b.date)));
        for session in doc.sessions {
            let date = date_for_epoch_day(session.started / 86_400);
            if matches(&date) && seen_runs.insert(session.id.clone()) {
                sessions.push(session);
            }
        }
    }
    for bucket in &mut buckets {
        bucket
            .counters
            .keywords
            .retain(|name, _| CANONICAL_KEYWORDS.contains(&name.as_str()));
    }
    let mut counters = StatsCounters::default();
    for bucket in &buckets {
        counters.merge(&bucket.counters);
    }
    Ok(StatsSnapshot {
        window,
        counters,
        buckets,
        sessions,
    })
}

pub fn cmd_stats_dispatch(args: &[&str], pending: Option<&mut UsageStats>) -> i32 {
    if !stats_enabled() {
        println!("Usage statistics — disabled (MIX_STATS)");
        return 0;
    }
    if args.first().copied() == Some("coverage") {
        let Some(dir) = args.get(1) else {
            eprintln!("mix stats coverage: requires a directory");
            return 2;
        };
        return crate::stats_coverage::run(Path::new(dir));
    }
    legacy_advisory();
    match dispatch_report(args, pending) {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("mix stats: {error}");
            1
        }
    }
}

fn dispatch_report(args: &[&str], pending: Option<&mut UsageStats>) -> Result<(), String> {
    match args.first().copied() {
        Some("help") => {
            print_help();
            Ok(())
        }
        Some("reset") => reset_current(pending),
        Some("clear") => clear_name(args.get(1).copied(), pending),
        Some("query") => query_db(&args[1..].join(" ")),
        Some("trend") => {
            let name = args.get(1).ok_or("trend requires a name")?;
            let snapshot = load_snapshot(StatsWindow::LastDays(30), pending.as_deref())?;
            print_trend(&snapshot, name);
            Ok(())
        }
        Some("since") => {
            let date = args.get(1).ok_or("since requires a YYYY-MM-DD date")?;
            if !is_canonical_date(date) {
                return Err(format!("invalid date {date:?}"));
            }
            print_overview(&load_snapshot(
                StatsWindow::Since((*date).to_string()),
                pending.as_deref(),
            )?);
            Ok(())
        }
        Some("week") => {
            let week = args.get(1).ok_or("week requires YYYY-WNN")?;
            if !is_canonical_week(week) {
                return Err(format!("invalid ISO week {week:?}"));
            }
            print_overview(&load_snapshot(
                StatsWindow::Week((*week).to_string()),
                pending.as_deref(),
            )?);
            Ok(())
        }
        Some("all") => {
            print_overview(&load_snapshot(StatsWindow::AllTime, pending.as_deref())?);
            Ok(())
        }
        command => {
            let snapshot = load_snapshot(StatsWindow::CurrentWeek, pending.as_deref())?;
            match command {
                None | Some("overview") => print_overview(&snapshot),
                Some("builtins") => {
                    print_category(&snapshot, "builtins", &snapshot.counters.builtins)
                }
                Some("functions") => {
                    print_category(&snapshot, "functions", &snapshot.counters.functions)
                }
                Some("aliases") => print_category(&snapshot, "aliases", &snapshot.counters.aliases),
                Some("commands") => {
                    print_category(&snapshot, "commands", &snapshot.counters.commands)
                }
                Some("keywords") => {
                    print_category(&snapshot, "keywords", &snapshot.counters.keywords)
                }
                Some("meta") => print_category(&snapshot, "meta", &snapshot.counters.meta),
                Some("errors") => print_category(&snapshot, "errors", &snapshot.counters.errors),
                Some("sessions") => print_sessions(&snapshot),
                Some("never") => print_never(&snapshot),
                Some("raw") => print_raw(&snapshot)?,
                Some("modes") => print_modes(&snapshot),
                Some("scripts") => {
                    let top = args
                        .get(1)
                        .and_then(|n| n.parse().ok())
                        .unwrap_or(10)
                        .clamp(1, 100);
                    print_scripts(&snapshot, top);
                }
                Some(other) => return Err(format!("unknown subcommand {other:?}")),
            }
            Ok(())
        }
    }
}

fn header(snapshot: &StatsSnapshot) {
    println!("Usage statistics — {}", snapshot.window.header());
}

fn print_overview(snapshot: &StatsSnapshot) {
    header(snapshot);
    let mut entries = Vec::new();
    for (category, map) in counter_maps(&snapshot.counters) {
        entries.extend(
            map.iter()
                .map(|(name, count)| (category, name.as_str(), *count)),
        );
    }
    entries.sort_by(|a, b| b.2.cmp(&a.2).then_with(|| a.1.cmp(b.1)));
    println!("{} total events", snapshot.counters.events());
    for (category, name, count) in entries.into_iter().take(20) {
        println!("{name:<30} {category:<10} {count:>8}");
    }
    if !snapshot.buckets.is_empty() {
        println!();
        print_modes_body(snapshot);
        if snapshot.buckets.iter().any(|b| b.script.is_some()) {
            println!();
            print_scripts_body(snapshot, 5);
        }
    }
}

fn print_category(snapshot: &StatsSnapshot, label: &str, map: &HashMap<String, u64>) {
    header(snapshot);
    println!("{label}:");
    let mut items: Vec<_> = map.iter().collect();
    items.sort_by(|a, b| b.1.cmp(a.1).then_with(|| a.0.cmp(b.0)));
    for (name, count) in items {
        println!("{name:<40} {count:>8}");
    }
}

fn print_sessions(snapshot: &StatsSnapshot) {
    header(snapshot);
    for run in &snapshot.sessions {
        println!(
            "{} {:<11} {:>6}s {:>6} commands {}",
            datetime_string(run.started),
            run.mode.as_str(),
            run.duration_secs,
            run.commands,
            run.script.as_deref().unwrap_or("")
        );
    }
}

fn print_never(snapshot: &StatsSnapshot) {
    header(snapshot);
    println!("Never-used builtins:");
    for name in BUILTIN_NAMES.iter().chain(HOF_NAMES.iter()) {
        if snapshot.counters.builtins.get(*name).copied().unwrap_or(0) == 0 {
            println!("  {name}");
        }
    }
    println!("Never-used keywords:");
    for name in CANONICAL_KEYWORDS {
        if snapshot.counters.keywords.get(*name).copied().unwrap_or(0) == 0 {
            println!("  {name}");
        }
    }
}

fn print_modes(snapshot: &StatsSnapshot) {
    header(snapshot);
    print_modes_body(snapshot);
}

fn print_modes_body(snapshot: &StatsSnapshot) {
    #[derive(Default)]
    struct Row {
        events: u64,
        builtins: u64,
        keywords: u64,
        functions: u64,
        commands: u64,
        errors: u64,
        runs: u64,
    }
    let mut rows: BTreeMap<&str, Row> = BTreeMap::new();
    for bucket in &snapshot.buckets {
        let row = rows.entry(bucket.mode.as_str()).or_default();
        row.events = row.events.saturating_add(bucket.counters.events());
        row.builtins = row
            .builtins
            .saturating_add(saturating_sum(bucket.counters.builtins.values()));
        row.keywords = row
            .keywords
            .saturating_add(saturating_sum(bucket.counters.keywords.values()));
        row.functions = row
            .functions
            .saturating_add(saturating_sum(bucket.counters.functions.values()));
        row.commands = row
            .commands
            .saturating_add(saturating_sum(bucket.counters.commands.values()));
        row.errors = row
            .errors
            .saturating_add(saturating_sum(bucket.counters.errors.values()));
    }
    for run in &snapshot.sessions {
        let row = rows.entry(run.mode.as_str()).or_default();
        row.runs = row.runs.saturating_add(1);
    }
    println!("Mode          Events Builtins Keywords Functions Commands Errors Runs");
    for (mode, row) in rows {
        println!(
            "{mode:<12} {:>6} {:>8} {:>8} {:>9} {:>8} {:>6} {:>4}",
            row.events,
            row.builtins,
            row.keywords,
            row.functions,
            row.commands,
            row.errors,
            row.runs
        );
    }
}

fn print_scripts(snapshot: &StatsSnapshot, top: usize) {
    header(snapshot);
    print_scripts_body(snapshot, top);
}

fn print_scripts_body(snapshot: &StatsSnapshot, top: usize) {
    let mut rows: HashMap<(String, ExecutionMode), (u64, u64, u64, u64)> = HashMap::new();
    for bucket in &snapshot.buckets {
        let Some(script) = &bucket.script else {
            continue;
        };
        let row = rows.entry((script.clone(), bucket.mode)).or_default();
        row.0 = row.0.saturating_add(bucket.counters.events());
        row.1 = row
            .1
            .saturating_add(saturating_sum(bucket.counters.builtins.values()));
        row.2 = row
            .2
            .saturating_add(saturating_sum(bucket.counters.keywords.values()));
    }
    for run in &snapshot.sessions {
        if let Some(script) = &run.script {
            let row = rows.entry((script.clone(), run.mode)).or_default();
            row.3 = row.3.saturating_add(1);
        }
    }
    let mut rows: Vec<_> = rows.into_iter().collect();
    rows.sort_by(|a, b| b.1.0.cmp(&a.1.0).then_with(|| a.0.0.cmp(&b.0.0)));
    println!("Script                         Mode        Events Builtins Keywords Runs");
    for ((script, mode), (events, builtins, keywords, runs)) in rows.into_iter().take(top) {
        println!(
            "{script:<30} {:<11} {events:>6} {builtins:>8} {keywords:>8} {runs:>4}",
            mode.as_str()
        );
    }
}

fn print_raw(snapshot: &StatsSnapshot) -> Result<(), String> {
    header(snapshot);
    println!(
        "{}",
        serde_json::to_string_pretty(&snapshot.usage().to_json()).map_err(|e| e.to_string())?
    );
    Ok(())
}

fn print_trend(snapshot: &StatsSnapshot, name: &str) {
    header(snapshot);
    println!("Trend for {name:?}:");
    let mut days: BTreeMap<&str, u64> = BTreeMap::new();
    for bucket in &snapshot.buckets {
        let count = counter_maps(&bucket.counters)
            .iter()
            .fold(0_u64, |total, (_, map)| {
                total.saturating_add(map.get(name).copied().unwrap_or(0))
            });
        let value = days.entry(&bucket.date).or_default();
        *value = value.saturating_add(count);
    }
    for (date, count) in days {
        println!("{date} {count}");
    }
}

fn query_db(sql: &str) -> Result<(), String> {
    if sql.trim().is_empty() {
        return Err("query requires SQL".to_string());
    }
    let dir = ensure_stats_dir().ok_or("stats state directory unavailable")?;
    let _lock = StatsLock::acquire(&dir, false, REPORT_LOCK_DEADLINE).map_err(|e| e.to_string())?;
    println!("Usage statistics — all-time SQLite query");
    let connection = Connection::open(dir.join("mix.db")).map_err(|e| e.to_string())?;
    let mut statement = connection.prepare(sql).map_err(|e| e.to_string())?;
    let columns = statement.column_count();
    let names: Vec<String> = statement
        .column_names()
        .iter()
        .map(|s| s.to_string())
        .collect();
    println!("{}", names.join("\t"));
    let rows = statement
        .query_map([], |row| {
            let mut values = Vec::new();
            for index in 0..columns {
                let value = row.get_ref(index)?;
                values.push(match value {
                    rusqlite::types::ValueRef::Null => "NULL".to_string(),
                    rusqlite::types::ValueRef::Integer(v) => v.to_string(),
                    rusqlite::types::ValueRef::Real(v) => v.to_string(),
                    rusqlite::types::ValueRef::Text(v) => String::from_utf8_lossy(v).into_owned(),
                    rusqlite::types::ValueRef::Blob(v) => format!("<{} bytes>", v.len()),
                });
            }
            Ok(values)
        })
        .map_err(|e| e.to_string())?;
    for row in rows {
        println!("{}", row.map_err(|e| e.to_string())?.join("\t"));
    }
    Ok(())
}

fn reset_current(pending: Option<&mut UsageStats>) -> Result<(), String> {
    let dir = ensure_stats_dir().ok_or("stats state directory unavailable")?;
    let _lock = StatsLock::acquire(&dir, true, REPORT_LOCK_DEADLINE).map_err(|e| e.to_string())?;
    reset_current_locked(&dir, pending)?;
    println!("Stats reset for week {}.", current_iso_week());
    Ok(())
}

fn reset_current_locked(dir: &Path, pending: Option<&mut UsageStats>) -> Result<(), String> {
    rotate_current(dir)?;
    let path = dir.join("current.json");
    if path.exists() {
        fs::remove_file(path).map_err(|e| e.to_string())?;
    }
    if let Some(pending) = pending {
        pending.reset_current_week();
    }
    Ok(())
}

fn clear_name(name: Option<&str>, pending: Option<&mut UsageStats>) -> Result<(), String> {
    let name = name.ok_or("clear requires a name")?;
    let dir = ensure_stats_dir().ok_or("stats state directory unavailable")?;
    let _lock = StatsLock::acquire(&dir, true, REPORT_LOCK_DEADLINE).map_err(|e| e.to_string())?;
    let mut changed = false;
    for entry in fs::read_dir(&dir).map_err(|e| e.to_string())? {
        let path = entry.map_err(|e| e.to_string())?.path();
        if path.extension().is_none_or(|e| e != "json") {
            continue;
        }
        let Some(mut doc) = read_doc(&path)? else {
            continue;
        };
        changed |= clear_name_from_stats(&mut doc, name);
        atomic_write_json(&path, &doc)?;
    }
    let db_path = dir.join("mix.db");
    if db_path.exists() {
        match Connection::open(&db_path) {
            Ok(connection) => {
                let _ = connection.execute("DELETE FROM usage WHERE name = ?1", [name]);
                let _ = connection.execute("DELETE FROM usage_context WHERE name = ?1", [name]);
            }
            Err(error) => debug_note(format!("clear SQLite update failed: {error}")),
        }
    }
    if let Some(pending) = pending {
        changed |= clear_name_from_stats(pending, name);
    }
    println!(
        "{} {name:?}.",
        if changed { "Cleared" } else { "Not found" }
    );
    Ok(())
}

fn clear_name_from_stats(stats: &mut UsageStats, name: &str) -> bool {
    let mut changed = false;
    for bucket in &mut stats.buckets {
        for (_, map) in counter_maps_mut(&mut bucket.counters) {
            changed |= map.remove(name).is_some();
        }
    }
    stats.rebuild_aggregates();
    changed
}

fn counter_maps_mut(c: &mut StatsCounters) -> [(&'static str, &mut HashMap<String, u64>); 7] {
    [
        ("builtin", &mut c.builtins),
        ("function", &mut c.functions),
        ("alias", &mut c.aliases),
        ("command", &mut c.commands),
        ("keyword", &mut c.keywords),
        ("meta", &mut c.meta),
        ("error", &mut c.errors),
    ]
}

fn print_help() {
    println!("mix stats subcommands:");
    println!("  overview | builtins | functions | aliases | commands | keywords | meta | errors");
    println!("  sessions | never | modes | scripts [N] | raw");
    println!("  week YYYY-WNN | since YYYY-MM-DD | all | trend NAME");
    println!("  coverage DIR | query SQL | clear NAME | reset");
}

#[cfg(test)]
mod tests {
    use super::*;
    use cosmix_mix::stats::{
        ExecutionMode, MAX_WEEKLY_SCRIPT_LABELS, OTHER_SCRIPT_LABEL, SessionRecord, StatsContext,
    };

    fn temp_dir(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!("mix-stats-{}-{label}", std::process::id()))
    }

    fn old_doc() -> UsageStats {
        let mut doc = empty_doc("1970-W01");
        doc.buckets.push(StatsBucket {
            date: "1970-01-01".to_string(),
            mode: ExecutionMode::C,
            script: None,
            counters: StatsCounters {
                builtins: HashMap::from([("len".to_string(), 1)]),
                ..StatsCounters::default()
            },
        });
        doc.last_date = "1970-01-01".to_string();
        doc.rebuild_aggregates();
        doc
    }

    /// `flush_pending_delta` commits per ISO week so a failed week merges
    /// back alone (a whole-delta merge-back after a partial commit would
    /// double-count the committed week). This pins the grouping seam: one
    /// single-week delta per week, unpersistable (dateless) buckets to the
    /// residual, and aggregates rebuilt on each part.
    #[test]
    fn split_delta_by_week_groups_and_quarantines_bad_dates() {
        let mut delta = UsageStats::for_execution(StatsContext::new(ExecutionMode::C, None));
        for (date, name) in [
            ("2026-08-16", "len"),   // 2026-W33 (Sunday)
            ("2026-08-17", "upper"), // 2026-W34
            ("not-a-date", "join"),
        ] {
            delta.buckets.push(StatsBucket {
                date: date.to_string(),
                mode: ExecutionMode::C,
                script: None,
                counters: StatsCounters {
                    builtins: HashMap::from([(name.to_string(), 1)]),
                    ..StatsCounters::default()
                },
            });
        }
        let (weeks, residual) = split_delta_by_week(&delta);
        assert_eq!(weeks.len(), 2);
        for part in &weeks {
            assert_eq!(part.buckets.len(), 1);
            let week = iso_week_for_date(&part.buckets[0].date).unwrap();
            assert!(
                part.buckets
                    .iter()
                    .all(|b| iso_week_for_date(&b.date).as_deref() == Some(week.as_str())),
                "each part must be single-week"
            );
            assert_eq!(
                part.builtins.values().sum::<u64>(),
                1,
                "aggregates rebuilt per part"
            );
        }
        assert_eq!(residual.buckets.len(), 1);
        assert_eq!(residual.buckets[0].date, "not-a-date");
        assert!(
            weeks.iter().all(|p| p.sessions.is_empty()) && residual.sessions.is_empty(),
            "no part may grow a session record"
        );
    }

    #[test]
    fn kill_switch_values_are_case_insensitive() {
        for value in ["off", "OFF", " false ", "FALSE", "0"] {
            assert!(!stats_setting_enabled(Some(value)), "{value}");
        }
        for value in [None, Some(""), Some("on"), Some("1"), Some("no")] {
            assert!(stats_setting_enabled(value));
        }
    }

    #[test]
    fn current_window_keeps_overview_and_never_on_same_counters() {
        let mut pending = UsageStats::for_execution(StatsContext::new(ExecutionMode::C, None));
        pending.track_keyword("if");
        let dir = temp_dir("window");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let snapshot =
            load_snapshot_locked(&dir, StatsWindow::CurrentWeek, Some(&pending)).unwrap();
        assert_eq!(snapshot.counters.keywords.get("if"), Some(&1));
        assert!(
            !CANONICAL_KEYWORDS
                .iter()
                .filter(|name| snapshot.counters.keywords.get(**name).copied().unwrap_or(0) == 0)
                .any(|name| *name == "if")
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn parallel_flushes_merge_exactly() {
        let dir = temp_dir("parallel");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let mut workers = Vec::new();
        for _ in 0..64 {
            let dir = dir.clone();
            workers.push(std::thread::spawn(move || {
                let mut delta =
                    UsageStats::for_execution(StatsContext::new(ExecutionMode::C, None));
                delta.track_builtin("len");
                delta.finalize_session();
                // This stress test checks lossless merge serialisation, not the
                // best-effort production deadline used by automatic flushes.
                flush_batch_to_dir_with_deadline(&dir, &delta, Duration::from_secs(10)).unwrap();
            }));
        }
        for worker in workers {
            worker.join().unwrap();
        }
        let snapshot = load_snapshot_locked(&dir, StatsWindow::CurrentWeek, None).unwrap();
        assert_eq!(snapshot.counters.builtins.get("len"), Some(&64));
        assert_eq!(snapshot.sessions.len(), 64);
        let db = Connection::open(dir.join("mix.db")).unwrap();
        let count: i64 = db
            .query_row(
                "SELECT count FROM usage WHERE category='builtin' AND name='len'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 64);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn corrupt_json_is_quarantined_and_recording_self_heals() {
        let dir = temp_dir("corrupt-json");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("current.json"), "{broken").unwrap();
        let mut delta = UsageStats::for_execution(StatsContext::new(ExecutionMode::C, None));
        delta.track_builtin("len");

        flush_batch_to_dir(&dir, &delta).unwrap();

        assert_eq!(
            read_doc(&dir.join("current.json"))
                .unwrap()
                .unwrap()
                .builtins
                .get("len"),
            Some(&1)
        );
        assert!(
            fs::read_dir(&dir)
                .unwrap()
                .filter_map(Result::ok)
                .any(|entry| {
                    entry
                        .file_name()
                        .to_str()
                        .is_some_and(|name| name.ends_with(".corrupt"))
                })
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn corrupt_archive_does_not_fail_reports() {
        let dir = temp_dir("corrupt-report");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("1970-W01.json"), "not json").unwrap();
        let mut current = UsageStats::new();
        current.track_builtin("upper");
        atomic_write_json(&dir.join("current.json"), &current).unwrap();

        let snapshot = load_snapshot_locked(&dir, StatsWindow::AllTime, None).unwrap();

        assert_eq!(snapshot.counters.builtins.get("upper"), Some(&1));
        assert!(!dir.join("1970-W01.json").exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn out_of_range_persisted_dates_and_timestamps_are_quarantined() {
        let dir = temp_dir("hostile-dates");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("current.json");
        fs::write(
            &path,
            serde_json::json!({
                "week": "2026-W34",
                "last_date": "2026-08-21",
                "buckets": [{"date": "999999999999-01-01"}],
                "sessions": [{"id": "hostile", "started": u64::MAX}]
            })
            .to_string(),
        )
        .unwrap();

        assert!(read_doc(&path).unwrap().is_none());
        assert!(!path.exists());
        assert!(
            fs::read_dir(&dir)
                .unwrap()
                .filter_map(Result::ok)
                .any(|entry| {
                    entry
                        .file_name()
                        .to_str()
                        .is_some_and(|name| name.ends_with(".corrupt"))
                })
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn rotation_replay_is_idempotent() {
        let dir = temp_dir("rotation-replay");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let current = old_doc();
        atomic_write_json(&dir.join("current.json"), &current).unwrap();
        rotate_current(&dir).unwrap();
        // Recreate the exact source document to model a crash after archive
        // commit but before current.json removal.
        atomic_write_json(&dir.join("current.json"), &current).unwrap();

        rotate_current(&dir).unwrap();

        let archive = read_doc(&dir.join("1970-W01.json")).unwrap().unwrap();
        assert_eq!(archive.builtins.get("len"), Some(&1));
        assert_eq!(archive.rotation_sources.len(), 1);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn invalid_persisted_week_is_derived_without_path_escape() {
        let dir = temp_dir("week-path");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let mut current = old_doc();
        current.week = "../escaped".to_string();
        atomic_write_json(&dir.join("current.json"), &current).unwrap();

        rotate_current(&dir).unwrap();

        assert!(dir.join("1970-W01.json").exists());
        assert!(!dir.parent().unwrap().join("escaped.json").exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn reset_archives_old_current_and_preserves_old_pending_buckets() {
        let dir = temp_dir("boundary-reset");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        atomic_write_json(&dir.join("current.json"), &old_doc()).unwrap();
        let mut pending = UsageStats::new();
        pending.track_builtin("upper");
        pending.buckets.extend(old_doc().buckets);
        pending.rebuild_aggregates();

        reset_current_locked(&dir, Some(&mut pending)).unwrap();

        assert!(dir.join("1970-W01.json").exists());
        assert!(!dir.join("current.json").exists());
        assert_eq!(pending.buckets.len(), 1);
        assert_eq!(pending.buckets[0].date, "1970-01-01");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn sqlite_legacy_sessions_are_idempotent_by_run_id() {
        let dir = temp_dir("session-idempotency");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let mut delta = UsageStats::for_execution(StatsContext::new(ExecutionMode::C, None));
        delta.finalize_session();

        flush_batch_to_dir(&dir, &delta).unwrap();
        flush_batch_to_dir(&dir, &delta).unwrap();

        let connection = Connection::open(dir.join("mix.db")).unwrap();
        let count: i64 = connection
            .query_row("SELECT COUNT(*) FROM sessions", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 1);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn sqlite_reconciles_historical_run_labels_after_json_folding() {
        let dir = temp_dir("sqlite-label-fold");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let mut first = UsageStats::for_execution(StatsContext::new(ExecutionMode::Script, None));
        for index in 0..MAX_WEEKLY_SCRIPT_LABELS {
            first.sessions.push(SessionRecord {
                id: format!("run-{index}"),
                started: 0,
                duration_secs: 0,
                commands: 1,
                peak_memory_kb: 0,
                mode: ExecutionMode::Script,
                script: Some(format!("script-{index:03}.mix")),
            });
        }
        flush_batch_to_dir(&dir, &first).unwrap();

        let mut second = UsageStats::for_execution(StatsContext::new(ExecutionMode::Script, None));
        second.sessions.push(SessionRecord {
            id: "run-new".to_string(),
            started: 0,
            duration_secs: 0,
            commands: 1,
            peak_memory_kb: 0,
            mode: ExecutionMode::Script,
            script: Some("aaa.mix".to_string()),
        });
        flush_batch_to_dir(&dir, &second).unwrap();

        let connection = Connection::open(dir.join("mix.db")).unwrap();
        let labels: i64 = connection
            .query_row("SELECT COUNT(DISTINCT script) FROM runs", [], |row| {
                row.get(0)
            })
            .unwrap();
        let folded: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM runs WHERE script = ?1",
                [OTHER_SCRIPT_LABEL],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(labels, MAX_WEEKLY_SCRIPT_LABELS as i64);
        assert_eq!(folded, 2);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn sqlite_clamps_unsigned_counters_and_run_fields_to_i64_max() {
        let dir = temp_dir("sqlite-clamp");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let mut delta = empty_doc("1970-W01");
        delta.buckets.push(StatsBucket {
            date: "1970-01-01".to_string(),
            mode: ExecutionMode::C,
            script: None,
            counters: StatsCounters {
                builtins: HashMap::from([("len".to_string(), u64::MAX)]),
                ..StatsCounters::default()
            },
        });
        delta.sessions.push(SessionRecord {
            id: "saturated-run".to_string(),
            started: 0,
            duration_secs: u64::MAX,
            commands: u64::MAX,
            peak_memory_kb: u64::MAX,
            mode: ExecutionMode::C,
            script: None,
        });
        delta.rebuild_aggregates();
        flush_batch_to_dir(&dir, &delta).unwrap();

        let connection = Connection::open(dir.join("mix.db")).unwrap();
        let count: i64 = connection
            .query_row(
                "SELECT count FROM usage WHERE category = 'builtin' AND name = 'len'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let run: (i64, i64, i64) = connection
            .query_row(
                "SELECT duration_secs, commands, peak_memory_kb FROM runs WHERE id = 'saturated-run'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(count, i64::MAX);
        assert_eq!(run, (i64::MAX, i64::MAX, i64::MAX));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn report_windows_require_canonical_values() {
        assert!(is_canonical_date("2026-08-01"));
        assert!(!is_canonical_date("2026-8-1"));
        assert!(is_canonical_week("2026-W34"));
        assert!(!is_canonical_week("definitely-not-a-week"));
        assert!(!is_canonical_week("2026-W00"));
    }

    #[test]
    fn lock_timeout_skips_the_batch_without_json_write() {
        let dir = temp_dir("lock-timeout");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let _held = StatsLock::acquire(&dir, true, REPORT_LOCK_DEADLINE).unwrap();
        let mut delta = UsageStats::for_execution(StatsContext::new(ExecutionMode::C, None));
        delta.track_builtin("len");
        let started = Instant::now();
        assert!(flush_batch_to_dir(&dir, &delta).is_err());
        let elapsed = started.elapsed();
        assert!(elapsed >= Duration::from_millis(190), "elapsed={elapsed:?}");
        assert!(elapsed < Duration::from_secs(1), "elapsed={elapsed:?}");
        assert!(!dir.join("current.json").exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn old_sqlite_schema_is_upgraded_additively() {
        let dir = temp_dir("old-sqlite");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let connection = Connection::open(dir.join("mix.db")).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE usage (
                    date TEXT, category TEXT, name TEXT, count INTEGER,
                    PRIMARY KEY(date, category, name));
                 CREATE TABLE sessions (
                    started INTEGER, duration_secs INTEGER, commands INTEGER,
                    peak_memory_kb INTEGER);",
            )
            .unwrap();
        drop(connection);
        let mut delta = UsageStats::for_execution(StatsContext::new(ExecutionMode::C, None));
        delta.track_builtin("len");
        delta.finalize_session();
        flush_batch_to_dir(&dir, &delta).unwrap();
        let connection = Connection::open(dir.join("mix.db")).unwrap();
        let tables: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master
                 WHERE type='table' AND name IN ('usage_context','runs','stats_meta')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(tables, 3);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_multi_week_batch_updates_archive_and_current_documents() {
        let dir = temp_dir("multi-week");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let mut delta = UsageStats::for_execution(StatsContext::new(ExecutionMode::Serve, None));
        delta.track_builtin("len");
        let old_bucket = StatsBucket {
            date: "2026-01-01".to_string(),
            mode: ExecutionMode::Serve,
            script: Some("worker.mix".to_string()),
            counters: delta.buckets[0].counters.clone(),
        };
        delta.buckets.push(old_bucket);
        delta.rebuild_aggregates();
        delta.finalize_session();
        flush_batch_to_dir(&dir, &delta).unwrap();
        assert!(dir.join("2026-W01.json").exists());
        assert!(dir.join("current.json").exists());
        let archived = read_doc(&dir.join("2026-W01.json")).unwrap().unwrap();
        assert_eq!(archived.builtins.get("len"), Some(&1));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn every_window_has_an_explicit_header() {
        assert!(StatsWindow::CurrentWeek.header().starts_with("week "));
        assert_eq!(
            StatsWindow::Week("2026-W34".into()).header(),
            "week 2026-W34"
        );
        assert_eq!(
            StatsWindow::Since("2026-08-01".into()).header(),
            "since 2026-08-01"
        );
        assert_eq!(StatsWindow::AllTime.header(), "all time");
        assert_eq!(StatsWindow::LastDays(30).header(), "last 30 recorded days");
    }
}
