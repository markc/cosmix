use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

pub const STATS_SCHEMA_VERSION: u64 = 2;
pub const MAX_SCRIPT_LABEL_CHARS: usize = 96;
pub const MAX_WEEKLY_SCRIPT_LABELS: usize = 128;
pub const OTHER_SCRIPT_LABEL: &str = "(other)";

/// The authored Mix constructs tracked by usage and static coverage.
pub const CANONICAL_KEYWORDS: &[&str] = &[
    "if", "for", "while", "loop", "function", "return", "select", "print", "eprint", "parse",
    "die", "try", "catch", "finally", "export", "alias", "break", "continue", "send", "address",
    "emit", "on", "source", "include", "sh",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ExecutionMode {
    Interactive,
    Script,
    C,
    Stdin,
    Serve,
}

impl ExecutionMode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Interactive => "interactive",
            Self::Script => "script",
            Self::C => "c",
            Self::Stdin => "stdin",
            Self::Serve => "serve",
        }
    }

    pub fn parse(value: &str) -> Self {
        match value {
            "script" => Self::Script,
            "c" => Self::C,
            "stdin" => Self::Stdin,
            "serve" => Self::Serve,
            _ => Self::Interactive,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatsContext {
    pub mode: ExecutionMode,
    pub script: Option<String>,
}

impl StatsContext {
    pub fn new(mode: ExecutionMode, script: Option<&Path>) -> Self {
        Self {
            mode,
            script: script.and_then(script_basename),
        }
    }
}

pub fn script_basename(path: &Path) -> Option<String> {
    let raw = path.file_name()?.to_string_lossy();
    let value: String = raw.chars().take(MAX_SCRIPT_LABEL_CHARS).collect();
    (!value.is_empty()).then_some(value)
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StatsCounters {
    pub builtins: HashMap<String, u64>,
    pub functions: HashMap<String, u64>,
    pub aliases: HashMap<String, u64>,
    pub commands: HashMap<String, u64>,
    pub keywords: HashMap<String, u64>,
    pub meta: HashMap<String, u64>,
    pub errors: HashMap<String, u64>,
}

impl StatsCounters {
    pub fn merge(&mut self, other: &Self) {
        merge_map(&mut self.builtins, &other.builtins);
        merge_map(&mut self.functions, &other.functions);
        merge_map(&mut self.aliases, &other.aliases);
        merge_map(&mut self.commands, &other.commands);
        merge_map(&mut self.keywords, &other.keywords);
        merge_map(&mut self.meta, &other.meta);
        merge_map(&mut self.errors, &other.errors);
    }

    pub fn events(&self) -> u64 {
        [
            &self.builtins,
            &self.functions,
            &self.aliases,
            &self.commands,
            &self.keywords,
            &self.meta,
            &self.errors,
        ]
        .into_iter()
        .flat_map(|map| map.values())
        .fold(0_u64, |total, count| total.saturating_add(*count))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatsBucket {
    pub date: String,
    pub mode: ExecutionMode,
    pub script: Option<String>,
    pub counters: StatsCounters,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionRecord {
    pub id: String,
    pub started: u64,
    pub duration_secs: u64,
    pub commands: u64,
    pub peak_memory_kb: u64,
    pub mode: ExecutionMode,
    pub script: Option<String>,
}

#[derive(Debug, Clone)]
pub struct UsageStats {
    // Kept public for source compatibility. These are aggregate views of buckets.
    pub builtins: HashMap<String, u64>,
    pub functions: HashMap<String, u64>,
    pub aliases: HashMap<String, u64>,
    pub commands: HashMap<String, u64>,
    pub keywords: HashMap<String, u64>,
    pub meta: HashMap<String, u64>,
    pub errors: HashMap<String, u64>,
    pub buckets: Vec<StatsBucket>,
    pub sessions: Vec<SessionRecord>,
    /// Content fingerprints already imported during week rotation.
    /// Persisted so replaying a crash-interrupted rotation is idempotent.
    pub rotation_sources: Vec<String>,
    pub week: String,
    pub last_date: String,
    pub session_start: u64,
    pub session_commands: u64,
    context: StatsContext,
    session_finalized: bool,
}

fn current_unix_timestamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

pub fn current_epoch_day() -> u64 {
    current_unix_timestamp() / 86_400
}

pub fn current_iso_week() -> String {
    iso_week_for_epoch_day(current_epoch_day())
}

pub fn today_string() -> String {
    date_for_epoch_day(current_epoch_day())
}

pub fn date_for_epoch_day(day: u64) -> String {
    let (year, month, date, _, _) = timestamp_to_date_parts(day.saturating_mul(86_400));
    format!("{year:04}-{month:02}-{date:02}")
}

pub fn iso_week_for_epoch_day(day: u64) -> String {
    let (year, _month, _day, yday, wday) = timestamp_to_date_parts(day.saturating_mul(86_400));
    let iso_wday = if wday == 0 { 7 } else { wday };
    let thursday_yday = yday as i64 + 4 - iso_wday as i64;
    if thursday_yday < 0 {
        let prev_year = year - 1;
        let prev_dec31_days = days_from_epoch(prev_year, 12, 31);
        let prev_dec31_wday = ((prev_dec31_days % 7 + 4) % 7) as u32;
        let prev_iso_wday = if prev_dec31_wday == 0 {
            7
        } else {
            prev_dec31_wday
        };
        let prev_yday = if is_leap_year(prev_year) { 365 } else { 364 };
        let week = (prev_yday as i64 + 4 - prev_iso_wday as i64) / 7 + 1;
        format!("{prev_year}-W{week:02}")
    } else {
        let week = thursday_yday / 7 + 1;
        let days_in_year = if is_leap_year(year) { 366 } else { 365 };
        if thursday_yday >= days_in_year as i64 {
            format!("{}-W01", year + 1)
        } else {
            format!("{year}-W{week:02}")
        }
    }
}

pub fn iso_week_for_date(date: &str) -> Option<String> {
    let mut parts = date.split('-');
    let y = parts.next()?.parse::<i64>().ok()?;
    let m = parts.next()?.parse::<u32>().ok()?;
    let d = parts.next()?.parse::<u32>().ok()?;
    if parts.next().is_some() || !(1..=12).contains(&m) || d == 0 || d > days_in_month(y, m) {
        return None;
    }
    let days = days_from_epoch(y, m, d);
    (days >= 0).then(|| iso_week_for_epoch_day(days as u64))
}

fn is_leap_year(y: i64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

fn days_in_month(y: i64, m: u32) -> u32 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap_year(y) => 29,
        2 => 28,
        _ => 0,
    }
}

fn days_from_epoch(y: i64, m: u32, d: u32) -> i64 {
    let mut total = 0;
    if y >= 1970 {
        for yr in 1970..y {
            total += if is_leap_year(yr) { 366 } else { 365 };
        }
    } else {
        for yr in y..1970 {
            total -= if is_leap_year(yr) { 366 } else { 365 };
        }
    }
    for month in 1..m {
        total += days_in_month(y, month) as i64;
    }
    total + d as i64 - 1
}

/// Returns (year, month, day, day_of_year_0based, day_of_week_0sun).
fn timestamp_to_date_parts(secs: u64) -> (i64, u32, u32, u32, u32) {
    let days = (secs / 86_400) as i64;
    let wday = ((days % 7 + 4) % 7) as u32;
    let mut year = 1970;
    let mut remaining = days;
    loop {
        let ydays = if is_leap_year(year) { 366 } else { 365 };
        if remaining < ydays {
            break;
        }
        remaining -= ydays;
        year += 1;
    }
    let yday = remaining as u32;
    let mut month = 1;
    let mut mdays = remaining as u32;
    loop {
        let dim = days_in_month(year, month);
        if mdays < dim {
            break;
        }
        mdays -= dim;
        month += 1;
    }
    (year, month, mdays + 1, yday, wday)
}

impl Default for UsageStats {
    fn default() -> Self {
        Self::new()
    }
}

impl UsageStats {
    /// Backwards-compatible opt-in collector for library embedders.
    pub fn new() -> Self {
        Self::for_execution(StatsContext {
            mode: ExecutionMode::Interactive,
            script: None,
        })
    }

    pub fn for_execution(context: StatsContext) -> Self {
        let now = current_unix_timestamp();
        Self {
            builtins: HashMap::new(),
            functions: HashMap::new(),
            aliases: HashMap::new(),
            commands: HashMap::new(),
            keywords: HashMap::new(),
            meta: HashMap::new(),
            errors: HashMap::new(),
            buckets: Vec::new(),
            sessions: Vec::new(),
            rotation_sources: Vec::new(),
            week: current_iso_week(),
            last_date: today_string(),
            session_start: now,
            session_commands: 0,
            context,
            session_finalized: false,
        }
    }

    pub fn context(&self) -> &StatsContext {
        &self.context
    }

    fn bucket_mut(&mut self) -> &mut StatsBucket {
        let date = today_string();
        self.last_date.clone_from(&date);
        self.week = iso_week_for_date(&date).unwrap_or_else(current_iso_week);
        let mode = self.context.mode;
        let script = self.context.script.clone();
        let idx = self
            .buckets
            .iter()
            .position(|b| b.date == date && b.mode == mode && b.script == script)
            .unwrap_or_else(|| {
                self.buckets.push(StatsBucket {
                    date,
                    mode,
                    script,
                    counters: StatsCounters::default(),
                });
                self.buckets.len() - 1
            });
        &mut self.buckets[idx]
    }

    fn track(&mut self, category: &str, name: &str) {
        let canonical = if category == "keyword" {
            normalize_legacy_keyword(name)
        } else {
            name
        };
        increment_map(category_map_mut(self, category), canonical);
        increment_map(
            counters_category_map_mut(&mut self.bucket_mut().counters, category),
            canonical,
        );
    }

    pub fn track_builtin(&mut self, name: &str) {
        self.track("builtin", name);
    }
    pub fn track_function(&mut self, name: &str) {
        self.track("function", name);
    }
    pub fn track_alias(&mut self, name: &str) {
        self.track("alias", name);
    }
    pub fn track_command(&mut self, name: &str) {
        self.track("command", name);
    }
    pub fn track_keyword(&mut self, name: &str) {
        if CANONICAL_KEYWORDS.contains(&normalize_legacy_keyword(name)) {
            self.track("keyword", name);
        }
    }
    pub fn track_meta(&mut self, name: &str) {
        self.track("meta", name);
    }
    pub fn track_error(&mut self, msg: &str) {
        let phrase = msg.split(['\'', '"', ':']).next().unwrap_or(msg).trim();
        let key = if phrase.is_empty() {
            msg.trim()
        } else {
            phrase
        };
        self.track("error", key);
    }
    pub fn increment_commands(&mut self) {
        self.session_commands = self.session_commands.saturating_add(1);
    }

    pub fn finalize_session(&mut self) {
        if self.session_finalized {
            return;
        }
        self.session_finalized = true;
        let now = current_unix_timestamp();
        let mode = self.context.mode;
        let script = self.context.script.clone();
        self.sessions.push(SessionRecord {
            id: format!(
                "{}-{}-{}-{:032x}",
                std::process::id(),
                self.session_start,
                mode.as_str(),
                rand::random::<u128>()
            ),
            started: self.session_start,
            duration_secs: now.saturating_sub(self.session_start),
            commands: self.session_commands,
            peak_memory_kb: read_peak_memory_kb(),
            mode,
            script,
        });
    }

    /// Move the pending persisted-counter delta (the daily buckets) out of
    /// the live collector WITHOUT finalizing the session. The caller flushes
    /// the returned delta to disk mid-session — e.g. before a plumbed
    /// `mix stats … | wc -l` line runs `mix stats` as an external child that
    /// must see current data — and, if the flush fails, merges it back with
    /// [`merge`](Self::merge) so nothing is lost. Session fields are
    /// untouched: the eventual exit flush still records exactly ONE session
    /// for this run (no split-session distortion), and the returned delta
    /// carries no session records for the flush to finalize.
    pub fn drain_pending_buckets(&mut self) -> UsageStats {
        let mut delta = UsageStats::for_execution(self.context.clone());
        delta.buckets = std::mem::take(&mut self.buckets);
        delta.rebuild_aggregates();
        delta.week.clone_from(&self.week);
        delta.last_date.clone_from(&self.last_date);
        self.rebuild_aggregates();
        delta
    }

    /// Clear current-week data while retaining older buckets and execution context.
    ///
    /// The REPL uses this after `mix stats reset`: persisted current-week data
    /// and counters accumulated since the last prompt must disappear together,
    /// otherwise the pending batch would recreate the supposedly-reset data at
    /// process exit.
    pub fn reset_current_week(&mut self) {
        let week = current_iso_week();
        self.buckets
            .retain(|bucket| iso_week_for_date(&bucket.date).as_deref() != Some(&week));
        self.sessions.retain(|session| {
            let date = date_for_epoch_day(session.started / 86_400);
            iso_week_for_date(&date).as_deref() != Some(&week)
        });
        self.rebuild_aggregates();
        self.week = week;
        self.last_date = today_string();
        self.session_start = current_unix_timestamp();
        self.session_commands = 0;
        self.session_finalized = false;
    }

    pub fn merge(&mut self, other: &Self) {
        merge_map(&mut self.builtins, &other.builtins);
        merge_map(&mut self.functions, &other.functions);
        merge_map(&mut self.aliases, &other.aliases);
        merge_map(&mut self.commands, &other.commands);
        merge_map(&mut self.keywords, &other.keywords);
        merge_map(&mut self.meta, &other.meta);
        merge_map(&mut self.errors, &other.errors);
        for incoming in &other.buckets {
            if let Some(bucket) = self.buckets.iter_mut().find(|bucket| {
                bucket.date == incoming.date
                    && bucket.mode == incoming.mode
                    && bucket.script == incoming.script
            }) {
                bucket.counters.merge(&incoming.counters);
            } else {
                self.buckets.push(incoming.clone());
            }
        }
        let existing: HashSet<String> = self.sessions.iter().map(|s| s.id.clone()).collect();
        self.sessions.extend(
            other
                .sessions
                .iter()
                .filter(|s| !existing.contains(&s.id))
                .cloned(),
        );
        for source in &other.rotation_sources {
            if !self.rotation_sources.contains(source) {
                self.rotation_sources.push(source.clone());
            }
        }
        self.buckets.sort_by(|a, b| {
            (&a.date, a.mode.as_str(), &a.script).cmp(&(&b.date, b.mode.as_str(), &b.script))
        });
    }

    pub fn rebuild_aggregates(&mut self) {
        self.builtins.clear();
        self.functions.clear();
        self.aliases.clear();
        self.commands.clear();
        self.keywords.clear();
        self.meta.clear();
        self.errors.clear();
        for bucket in &self.buckets {
            merge_map(&mut self.builtins, &bucket.counters.builtins);
            merge_map(&mut self.functions, &bucket.counters.functions);
            merge_map(&mut self.aliases, &bucket.counters.aliases);
            merge_map(&mut self.commands, &bucket.counters.commands);
            merge_map(&mut self.keywords, &bucket.counters.keywords);
            merge_map(&mut self.meta, &bucket.counters.meta);
            merge_map(&mut self.errors, &bucket.counters.errors);
        }
    }

    /// Bound distinct script labels in a single weekly document.
    pub fn bound_script_labels(&mut self) {
        let has_other = self
            .buckets
            .iter()
            .filter_map(|bucket| bucket.script.as_deref())
            .chain(
                self.sessions
                    .iter()
                    .filter_map(|session| session.script.as_deref()),
            )
            .any(|script| script == OTHER_SCRIPT_LABEL);
        let mut labels: Vec<String> = self
            .buckets
            .iter()
            .filter_map(|b| b.script.clone())
            .chain(self.sessions.iter().filter_map(|s| s.script.clone()))
            .filter(|s| s != OTHER_SCRIPT_LABEL)
            .collect();
        labels.sort();
        labels.dedup();
        if labels.len() + usize::from(has_other) <= MAX_WEEKLY_SCRIPT_LABELS {
            return;
        }
        // When overflow exists, `(other)` itself consumes one label slot.
        let keep: HashSet<_> = labels
            .into_iter()
            .take(MAX_WEEKLY_SCRIPT_LABELS.saturating_sub(1))
            .collect();
        for bucket in &mut self.buckets {
            if bucket.script.as_ref().is_some_and(|s| !keep.contains(s)) {
                bucket.script = Some(OTHER_SCRIPT_LABEL.to_string());
            }
        }
        for session in &mut self.sessions {
            if session
                .script
                .as_ref()
                .is_some_and(|script| !keep.contains(script))
            {
                session.script = Some(OTHER_SCRIPT_LABEL.to_string());
            }
        }
        let old = std::mem::take(&mut self.buckets);
        for bucket in old {
            if let Some(existing) = self.buckets.iter_mut().find(|b| {
                b.date == bucket.date && b.mode == bucket.mode && b.script == bucket.script
            }) {
                existing.counters.merge(&bucket.counters);
            } else {
                self.buckets.push(bucket);
            }
        }
        self.buckets.sort_by(|a, b| {
            (&a.date, a.mode.as_str(), &a.script).cmp(&(&b.date, b.mode.as_str(), &b.script))
        });
        self.rebuild_aggregates();
    }

    #[cfg(feature = "json")]
    pub fn to_json(&self) -> serde_json::Value {
        use serde_json::json;
        json!({
            "schema_version": STATS_SCHEMA_VERSION,
            "builtins": hashmap_to_json(&self.builtins),
            "functions": hashmap_to_json(&self.functions),
            "aliases": hashmap_to_json(&self.aliases),
            "commands": hashmap_to_json(&self.commands),
            "keywords": hashmap_to_json(&self.keywords),
            "meta": hashmap_to_json(&self.meta),
            "errors": hashmap_to_json(&self.errors),
            "buckets": self.buckets.iter().map(bucket_to_json).collect::<Vec<_>>(),
            "sessions": self.sessions.iter().map(session_to_json).collect::<Vec<_>>(),
            "rotation_sources": self.rotation_sources,
            "week": self.week,
            "last_date": self.last_date,
        })
    }

    #[cfg(feature = "json")]
    pub fn from_json(v: serde_json::Value) -> Self {
        let obj = v.as_object();
        let mut stats = Self::new();
        stats.builtins = json_to_hashmap(obj.and_then(|o| o.get("builtins")));
        stats.functions = json_to_hashmap(obj.and_then(|o| o.get("functions")));
        stats.aliases = json_to_hashmap(obj.and_then(|o| o.get("aliases")));
        stats.commands = json_to_hashmap(obj.and_then(|o| o.get("commands")));
        stats.keywords =
            normalize_keyword_map(json_to_hashmap(obj.and_then(|o| o.get("keywords"))));
        stats.meta = json_to_hashmap(obj.and_then(|o| o.get("meta")));
        stats.errors = json_to_hashmap(obj.and_then(|o| o.get("errors")));
        stats.week = json_string(obj, "week").unwrap_or_default();
        stats.last_date = json_string(obj, "last_date").unwrap_or_default();
        stats.sessions = obj
            .and_then(|o| o.get("sessions"))
            .and_then(|v| v.as_array())
            .map(|items| {
                items
                    .iter()
                    .enumerate()
                    .filter_map(session_from_json)
                    .collect()
            })
            .unwrap_or_default();
        stats.rotation_sources = obj
            .and_then(|o| o.get("rotation_sources"))
            .and_then(|v| v.as_array())
            .map(|items| {
                items
                    .iter()
                    .filter_map(|value| value.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        stats.buckets = obj
            .and_then(|o| o.get("buckets"))
            .and_then(|v| v.as_array())
            .map(|items| items.iter().filter_map(bucket_from_json).collect())
            .unwrap_or_default();
        if stats.buckets.is_empty() && stats.events() > 0 && !stats.last_date.is_empty() {
            stats.buckets.push(StatsBucket {
                date: stats.last_date.clone(),
                mode: ExecutionMode::Interactive,
                script: None,
                counters: stats.counters(),
            });
        } else if !stats.buckets.is_empty() {
            stats.rebuild_aggregates();
        }
        stats.session_start = current_unix_timestamp();
        stats.session_commands = 0;
        stats.session_finalized = false;
        stats
    }

    pub fn counters(&self) -> StatsCounters {
        StatsCounters {
            builtins: self.builtins.clone(),
            functions: self.functions.clone(),
            aliases: self.aliases.clone(),
            commands: self.commands.clone(),
            keywords: self.keywords.clone(),
            meta: self.meta.clone(),
            errors: self.errors.clone(),
        }
    }

    pub fn events(&self) -> u64 {
        self.counters().events()
    }
}

fn increment_map(map: &mut HashMap<String, u64>, name: &str) {
    let value = map.entry(name.to_string()).or_insert(0);
    *value = value.saturating_add(1);
}

fn merge_map(target: &mut HashMap<String, u64>, source: &HashMap<String, u64>) {
    for (key, count) in source {
        let value = target.entry(key.clone()).or_insert(0);
        *value = value.saturating_add(*count);
    }
}

fn category_map_mut<'a>(stats: &'a mut UsageStats, category: &str) -> &'a mut HashMap<String, u64> {
    match category {
        "builtin" => &mut stats.builtins,
        "function" => &mut stats.functions,
        "alias" => &mut stats.aliases,
        "command" => &mut stats.commands,
        "keyword" => &mut stats.keywords,
        "meta" => &mut stats.meta,
        _ => &mut stats.errors,
    }
}

fn counters_category_map_mut<'a>(
    counters: &'a mut StatsCounters,
    category: &str,
) -> &'a mut HashMap<String, u64> {
    match category {
        "builtin" => &mut counters.builtins,
        "function" => &mut counters.functions,
        "alias" => &mut counters.aliases,
        "command" => &mut counters.commands,
        "keyword" => &mut counters.keywords,
        "meta" => &mut counters.meta,
        _ => &mut counters.errors,
    }
}

pub fn normalize_legacy_keyword(value: &str) -> &str {
    match value {
        "If" => "if",
        "For" | "ForEach" => "for",
        "While" => "while",
        "Loop" => "loop",
        "FunctionDef" => "function",
        "Return" => "return",
        "Select" => "select",
        "Print" => "print",
        "Parse" => "parse",
        "Die" => "die",
        "TryCatch" => "try",
        "Export" => "export",
        "Alias" => "alias",
        "Break" | "BreakIf" => "break",
        "Continue" | "ContinueIf" => "continue",
        "Send" => "send",
        "Address" => "address",
        "Emit" => "emit",
        "On" => "on",
        "Source" => "source",
        "Include" => "include",
        "Sh" => "sh",
        other => other,
    }
}

// Only the JSON (de)serialisers call this; without that feature it is dead
// code and every dependant's clippy run printed the warning.
#[cfg(feature = "json")]
fn normalize_keyword_map(source: HashMap<String, u64>) -> HashMap<String, u64> {
    let mut result: HashMap<String, u64> = HashMap::new();
    for (name, count) in source {
        let value = result
            .entry(normalize_legacy_keyword(&name).to_string())
            .or_insert(0);
        *value = value.saturating_add(count);
    }
    result
}

fn read_peak_memory_kb() -> u64 {
    #[cfg(target_os = "linux")]
    {
        if let Ok(content) = std::fs::read_to_string("/proc/self/status") {
            for line in content.lines() {
                if line.starts_with("VmPeak:") {
                    return line
                        .split_whitespace()
                        .nth(1)
                        .and_then(|s| s.parse::<u64>().ok())
                        .unwrap_or(0);
                }
            }
        }
        0
    }
    #[cfg(not(target_os = "linux"))]
    {
        0
    }
}

pub fn datetime_string(ts: u64) -> String {
    let (year, month, day, _, _) = timestamp_to_date_parts(ts);
    let remainder = ts % 86_400;
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02}",
        remainder / 3600,
        (remainder % 3600) / 60
    )
}

pub use crate::builtin_info::BuiltinInfo;
pub use crate::builtins::{BUILTIN_NAMES, BUILTINS};
pub use crate::builtins_hof::{HOF_NAMES, HOFS};

#[cfg(feature = "json")]
fn hashmap_to_json(map: &HashMap<String, u64>) -> serde_json::Value {
    let obj = map
        .iter()
        .map(|(k, v)| (k.clone(), serde_json::Value::Number((*v).into())))
        .collect();
    serde_json::Value::Object(obj)
}

#[cfg(feature = "json")]
fn json_to_hashmap(val: Option<&serde_json::Value>) -> HashMap<String, u64> {
    match val {
        Some(serde_json::Value::Object(obj)) => obj
            .iter()
            .filter_map(|(k, v)| v.as_u64().map(|n| (k.clone(), n)))
            .collect(),
        _ => HashMap::new(),
    }
}

#[cfg(feature = "json")]
fn counters_to_json(c: &StatsCounters) -> serde_json::Value {
    use serde_json::json;
    json!({
        "builtins": hashmap_to_json(&c.builtins),
        "functions": hashmap_to_json(&c.functions),
        "aliases": hashmap_to_json(&c.aliases),
        "commands": hashmap_to_json(&c.commands),
        "keywords": hashmap_to_json(&c.keywords),
        "meta": hashmap_to_json(&c.meta),
        "errors": hashmap_to_json(&c.errors),
    })
}

#[cfg(feature = "json")]
fn counters_from_json(value: &serde_json::Value) -> StatsCounters {
    let obj = value.as_object();
    StatsCounters {
        builtins: json_to_hashmap(obj.and_then(|o| o.get("builtins"))),
        functions: json_to_hashmap(obj.and_then(|o| o.get("functions"))),
        aliases: json_to_hashmap(obj.and_then(|o| o.get("aliases"))),
        commands: json_to_hashmap(obj.and_then(|o| o.get("commands"))),
        keywords: normalize_keyword_map(json_to_hashmap(obj.and_then(|o| o.get("keywords")))),
        meta: json_to_hashmap(obj.and_then(|o| o.get("meta"))),
        errors: json_to_hashmap(obj.and_then(|o| o.get("errors"))),
    }
}

#[cfg(feature = "json")]
fn bucket_to_json(bucket: &StatsBucket) -> serde_json::Value {
    let mut value = counters_to_json(&bucket.counters);
    if let Some(obj) = value.as_object_mut() {
        obj.insert("date".into(), bucket.date.clone().into());
        obj.insert("mode".into(), bucket.mode.as_str().into());
        obj.insert(
            "script".into(),
            bucket
                .script
                .clone()
                .map_or(serde_json::Value::Null, Into::into),
        );
    }
    value
}

#[cfg(feature = "json")]
fn bucket_from_json(value: &serde_json::Value) -> Option<StatsBucket> {
    let obj = value.as_object()?;
    Some(StatsBucket {
        date: obj.get("date")?.as_str()?.to_string(),
        mode: ExecutionMode::parse(
            obj.get("mode")
                .and_then(|v| v.as_str())
                .unwrap_or("interactive"),
        ),
        script: obj
            .get("script")
            .and_then(|v| v.as_str())
            .map(str::to_string),
        counters: counters_from_json(value),
    })
}

#[cfg(feature = "json")]
fn session_to_json(s: &SessionRecord) -> serde_json::Value {
    serde_json::json!({
        "id": s.id,
        "started": s.started,
        "duration_secs": s.duration_secs,
        "commands": s.commands,
        "peak_memory_kb": s.peak_memory_kb,
        "mode": s.mode.as_str(),
        "script": s.script,
    })
}

#[cfg(feature = "json")]
fn session_from_json((index, value): (usize, &serde_json::Value)) -> Option<SessionRecord> {
    let obj = value.as_object()?;
    let started = obj.get("started").and_then(|v| v.as_u64()).unwrap_or(0);
    Some(SessionRecord {
        id: obj
            .get("id")
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .unwrap_or_else(|| format!("legacy-{started}-{index}")),
        started,
        duration_secs: obj
            .get("duration_secs")
            .and_then(|v| v.as_u64())
            .unwrap_or(0),
        commands: obj.get("commands").and_then(|v| v.as_u64()).unwrap_or(0),
        peak_memory_kb: obj
            .get("peak_memory_kb")
            .and_then(|v| v.as_u64())
            .unwrap_or(0),
        mode: ExecutionMode::parse(
            obj.get("mode")
                .and_then(|v| v.as_str())
                .unwrap_or("interactive"),
        ),
        script: obj
            .get("script")
            .and_then(|v| v.as_str())
            .map(str::to_string),
    })
}

#[cfg(feature = "json")]
fn json_string(
    obj: Option<&serde_json::Map<String, serde_json::Value>>,
    key: &str,
) -> Option<String> {
    obj?.get(key)?.as_str().map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn execution_context_tags_daily_bucket() {
        let mut stats = UsageStats::for_execution(StatsContext::new(
            ExecutionMode::Script,
            Some(Path::new("/private/tree/hello.mix")),
        ));
        stats.track_builtin("len");
        assert_eq!(stats.buckets.len(), 1);
        assert_eq!(stats.buckets[0].mode, ExecutionMode::Script);
        assert_eq!(stats.buckets[0].script.as_deref(), Some("hello.mix"));
    }

    /// The mid-session drain (plumbed `mix stats …` REPL lines) must move
    /// the counters out without finalizing the session, and a merge-back
    /// after a failed flush must restore them exactly — no loss, no
    /// double-count, no phantom session record.
    #[test]
    fn drain_pending_buckets_moves_counters_without_finalizing() {
        let mut stats =
            UsageStats::for_execution(StatsContext::new(ExecutionMode::Interactive, None));
        stats.track_builtin("len");
        stats.track_builtin("len");
        stats.track_command("wc");
        stats.increment_commands();

        let delta = stats.drain_pending_buckets();
        assert_eq!(delta.builtins.get("len"), Some(&2));
        assert_eq!(delta.commands.get("wc"), Some(&1));
        assert!(delta.sessions.is_empty(), "delta must carry no session");
        assert!(!delta.session_finalized);
        assert!(stats.buckets.is_empty(), "live counters moved out");
        assert!(stats.builtins.is_empty(), "aggregates rebuilt to empty");
        assert!(!stats.session_finalized, "session must stay open");
        assert_eq!(stats.session_commands, 1, "session tally untouched");

        // Failed-flush path: merging the delta back restores the counters.
        stats.merge(&delta);
        assert_eq!(stats.builtins.get("len"), Some(&2));
        assert_eq!(stats.commands.get("wc"), Some(&1));
        assert!(stats.sessions.is_empty());

        // Tracking continues into a fresh bucket after a (successful) drain.
        let _ = stats.drain_pending_buckets();
        stats.track_builtin("upper");
        assert_eq!(stats.builtins.get("upper"), Some(&1));
        assert_eq!(stats.builtins.get("len"), None);
    }

    #[cfg(feature = "json")]
    #[test]
    fn legacy_keywords_are_canonicalised_on_load() {
        let value = serde_json::json!({
            "keywords": {"If": 3, "if": 2, "Assignment": 99},
            "week": "2026-W34",
            "last_date": "2026-08-21",
            "future_top_level": {"ignored": true},
            "sessions": [{
                "started": 1,
                "duration_secs": 2,
                "commands": 3,
                "peak_memory_kb": 4,
                "future_session_field": "ignored"
            }]
        });
        let stats = UsageStats::from_json(value);
        assert_eq!(stats.keywords.get("if"), Some(&5));
        assert_eq!(stats.keywords.get("Assignment"), Some(&99));
        assert_eq!(stats.buckets[0].counters.keywords.get("if"), Some(&5));
        assert_eq!(stats.sessions[0].mode, ExecutionMode::Interactive);
        assert!(stats.sessions[0].id.starts_with("legacy-"));
    }

    #[cfg(feature = "json")]
    #[test]
    fn hostile_counters_saturate_during_load_merge_and_tracking() {
        let value = serde_json::json!({
            "keywords": {"If": u64::MAX, "if": 1},
            "week": "2026-W34",
            "last_date": "2026-08-21"
        });
        let mut stats = UsageStats::from_json(value);
        assert_eq!(stats.keywords.get("if"), Some(&u64::MAX));
        stats.track_keyword("if");
        assert_eq!(stats.keywords.get("if"), Some(&u64::MAX));
        assert_eq!(stats.events(), u64::MAX);
    }

    #[test]
    fn reset_current_week_preserves_older_pending_buckets() {
        let mut stats = UsageStats::new();
        stats.track_builtin("len");
        stats.buckets.push(StatsBucket {
            date: "2026-01-01".to_string(),
            mode: ExecutionMode::Interactive,
            script: None,
            counters: StatsCounters {
                builtins: HashMap::from([("upper".to_string(), 2)]),
                ..StatsCounters::default()
            },
        });
        stats.rebuild_aggregates();

        stats.reset_current_week();

        assert_eq!(stats.buckets.len(), 1);
        assert_eq!(stats.buckets[0].date, "2026-01-01");
        assert_eq!(stats.builtins.get("upper"), Some(&2));
        assert!(!stats.builtins.contains_key("len"));
    }

    #[test]
    fn run_ids_include_random_entropy() {
        let mut first = UsageStats::new();
        let mut second = UsageStats::new();
        first.finalize_session();
        second.finalize_session();
        assert_ne!(first.sessions[0].id, second.sessions[0].id);
    }

    #[test]
    fn script_label_bound_includes_sessions_and_other_slot() {
        let mut stats = UsageStats::new();
        for index in 0..=MAX_WEEKLY_SCRIPT_LABELS {
            let script = format!("script-{index:03}.mix");
            stats.buckets.push(StatsBucket {
                date: "2026-08-21".to_string(),
                mode: ExecutionMode::Script,
                script: Some(script.clone()),
                counters: StatsCounters::default(),
            });
            stats.sessions.push(SessionRecord {
                id: format!("run-{index}"),
                started: index as u64,
                duration_secs: 0,
                commands: 1,
                peak_memory_kb: 0,
                mode: ExecutionMode::Script,
                script: Some(script),
            });
        }

        stats.bound_script_labels();

        let labels: HashSet<&str> = stats
            .buckets
            .iter()
            .filter_map(|bucket| bucket.script.as_deref())
            .chain(
                stats
                    .sessions
                    .iter()
                    .filter_map(|session| session.script.as_deref()),
            )
            .collect();
        assert_eq!(labels.len(), MAX_WEEKLY_SCRIPT_LABELS);
        assert!(labels.contains(OTHER_SCRIPT_LABEL));
        assert!(
            stats
                .sessions
                .iter()
                .any(|session| session.script.as_deref() == Some(OTHER_SCRIPT_LABEL))
        );
    }

    #[test]
    fn existing_other_label_consumes_one_of_the_128_slots() {
        let mut stats = UsageStats::new();
        for index in 0..MAX_WEEKLY_SCRIPT_LABELS {
            stats.sessions.push(SessionRecord {
                id: format!("run-{index}"),
                started: index as u64,
                duration_secs: 0,
                commands: 1,
                peak_memory_kb: 0,
                mode: ExecutionMode::Script,
                script: Some(format!("script-{index:03}.mix")),
            });
        }
        stats.sessions.push(SessionRecord {
            id: "run-other".to_string(),
            started: 0,
            duration_secs: 0,
            commands: 1,
            peak_memory_kb: 0,
            mode: ExecutionMode::Script,
            script: Some(OTHER_SCRIPT_LABEL.to_string()),
        });

        stats.bound_script_labels();

        let labels: HashSet<&str> = stats
            .sessions
            .iter()
            .filter_map(|session| session.script.as_deref())
            .collect();
        assert_eq!(labels.len(), MAX_WEEKLY_SCRIPT_LABELS);
        assert!(labels.contains(OTHER_SCRIPT_LABEL));
    }
}
