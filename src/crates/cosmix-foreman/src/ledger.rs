//! The task ledger: SQLite in WAL mode, one foreman process arbitrating.
//! Deliberately boring — atomic claims via a guarded UPDATE, append-only
//! events, every session accounted in `runs`. This schema is what the
//! foreman MCP server (Phase 1) exposes to the agents themselves.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use chrono::{SecondsFormat, Utc};
use rusqlite::{Connection, ErrorCode, OpenFlags, OptionalExtension, params};
use serde::{Deserialize, Serialize};

use crate::executor::Usage;

include!("ledger/busy.rs");
include!("ledger/finding_types.rs");
include!("ledger/task_state.rs");
include!("ledger/types.rs");
include!("ledger/connection.rs");
include!("ledger/schema.rs");
include!("ledger/tasks_create.rs");
include!("ledger/tasks_query.rs");
include!("ledger/claims.rs");
include!("ledger/requeue.rs");
include!("ledger/finish_worker.rs");
include!("ledger/finish_landing.rs");
include!("ledger/park_retire.rs");
include!("ledger/runs.rs");
include!("ledger/findings.rs");
include!("ledger/reporting.rs");
include!("ledger/reaping.rs");
include!("ledger/governor.rs");
include!("ledger/verification.rs");
include!("ledger/landing.rs");
include!("ledger/push_journal.rs");
#[cfg(test)]
include!("ledger/tests.rs");
